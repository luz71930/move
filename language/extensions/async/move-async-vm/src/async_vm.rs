// Copyright (c) The Diem Core Contributors
// Copyright (c) The Move Contributors
// SPDX-License-Identifier: Apache-2.0

use std::{
    collections::HashMap,
    error::Error,
    fmt::{Debug, Display, Formatter},
};

use move_binary_format::errors::{Location, PartialVMError, PartialVMResult, VMError, VMResult};
use move_core_types::{
    account_address::AccountAddress,
    effects::{ChangeSet, Event},
    gas_schedule::GasAlgebra,
    identifier::Identifier,
    language_storage::{ModuleId, StructTag, TypeTag},
    resolver::MoveResolver,
    vm_status::StatusCode,
};
use move_vm_runtime::{
    move_vm::MoveVM,
    native_extensions::NativeContextExtensions,
    native_functions::NativeFunction,
    session::{SerializedReturnValues, Session},
};
use move_vm_types::{
    gas_schedule::GasStatus,
    values::{Reference, Value},
};

use crate::{actor_metadata, actor_metadata::ActorMetadata, natives, natives::AsyncExtension};

/// Represents an instance of an async VM.
pub struct AsyncVM {
    move_vm: MoveVM,
    actor_metadata: HashMap<ModuleId, ActorMetadata>,
    message_table: HashMap<u64, (ModuleId, Identifier)>,
}

impl AsyncVM {
    /// Creates a new VM, registering the given natives and actors.
    pub fn new<I, A>(async_lib_addr: AccountAddress, natives: I, actors: A) -> VMResult<Self>
    where
        I: IntoIterator<Item = (AccountAddress, Identifier, Identifier, NativeFunction)>,
        A: IntoIterator<Item = ActorMetadata>,
    {
        let actor_metadata: HashMap<ModuleId, ActorMetadata> = actors
            .into_iter()
            .map(|a| (a.module_id.clone(), a))
            .collect();
        let message_table: HashMap<u64, (ModuleId, Identifier)> = actor_metadata
            .values()
            .flat_map(|a| {
                a.messages.iter().map(move |m| {
                    (
                        actor_metadata::message_hash(&a.module_id, m.as_ident_str()),
                        (a.module_id.clone(), m.clone()),
                    )
                })
            })
            .collect();
        Ok(AsyncVM {
            move_vm: MoveVM::new(
                natives
                    .into_iter()
                    .chain(natives::actor_natives(async_lib_addr).into_iter()),
            )?,
            actor_metadata,
            message_table,
        })
    }

    /// Creates a new session.
    pub fn new_session<'r, 'l, S: MoveResolver>(
        &'l self,
        for_actor: AccountAddress,
        virtual_time: u128,
        move_resolver: &'r mut S,
    ) -> AsyncSession<'r, 'l, S> {
        let extensions = make_extensions(for_actor, virtual_time, true);
        AsyncSession {
            vm: self,
            vm_session: self
                .move_vm
                .new_session_with_extensions(move_resolver, extensions),
        }
    }

    /// Get the underlying Move VM.
    pub fn get_move_vm(&mut self) -> &mut MoveVM {
        &mut self.move_vm
    }

    /// Resolve the message hash into module and handler function.
    pub fn resolve_message_hash(&self, message_hash: u64) -> Option<&(ModuleId, Identifier)> {
        self.message_table.get(&message_hash)
    }

    /// Get the actor metadata.
    pub fn actor_metadata(&self, module_id: &ModuleId) -> Option<&ActorMetadata> {
        self.actor_metadata.get(module_id)
    }

    /// Get all know actors.
    pub fn actors(&self) -> Vec<ModuleId> {
        self.actor_metadata.keys().cloned().collect()
    }
}

/// Represents an Async Move execution session.
pub struct AsyncSession<'r, 'l, S> {
    vm: &'l AsyncVM,
    vm_session: Session<'r, 'l, S>,
}

/// Represents a message being sent, consisting of target address, message hash, and arguments.
pub type Message = (AccountAddress, u64, Vec<Vec<u8>>);

/// A structure to represent success for the execution of an async session operation.
#[derive(Debug, Clone)]
pub struct AsyncSuccess {
    pub change_set: ChangeSet,
    pub events: Vec<Event>,
    pub messages: Vec<Message>,
    pub gas_used: u64,
}

/// A structure to represent failure for the execution of an async session operation.
#[derive(Debug, Clone)]
pub struct AsyncError {
    pub error: VMError,
    pub gas_used: u64,
}

/// Result type for operations of an AsyncSession.
pub type AsyncResult = Result<AsyncSuccess, AsyncError>;

impl<'r, 'l, S: MoveResolver> AsyncSession<'r, 'l, S> {
    /// Get the underlying Move VM session.
    pub fn get_move_session(&mut self) -> &mut Session<'r, 'l, S> {
        &mut self.vm_session
    }

    /// Creates a new actor, identified by the module_id, at the given account address.
    /// This calls the initializer function of the actor, and returns on success
    /// a changeset which needs to be committed to persist the new actors state.
    pub fn new_actor(
        mut self,
        module_id: &ModuleId,
        actor_addr: AccountAddress,
        gas_status: &mut GasStatus,
    ) -> AsyncResult {
        let actor = self
            .vm
            .actor_metadata
            .get(module_id)
            .ok_or_else(|| async_extension_error(format!("actor `{}` unknown", module_id)))?;
        let state_type_tag = TypeTag::Struct(actor.state_tag.clone());
        let state_type = self
            .vm_session
            .load_type(&state_type_tag)
            .map_err(vm_error_to_async)?;

        // Check whether the actor state already exists.
        let state = self
            .vm_session
            .get_data_store()
            .load_resource(actor_addr, &state_type)
            .map_err(partial_vm_error_to_async)?;
        if state.exists().map_err(partial_vm_error_to_async)? {
            return Err(async_extension_error(format!(
                "actor `{}` already exists at `{}`",
                module_id.short_str_lossless(),
                actor_addr.short_str_lossless()
            )));
        }

        // Execute the initializer.
        let gas_before = gas_status.remaining_gas().get();
        let result = self
            .vm_session
            .execute_function_bypass_visibility(
                &actor.module_id,
                &actor.initializer,
                vec![],
                Vec::<Vec<u8>>::new(),
                gas_status,
            )
            .and_then(|ret| Ok((ret, self.vm_session.finish_with_extensions()?)));
        let gas_used = gas_status.remaining_gas().get() - gas_before;

        // Process the result, moving the return value of the initializer function into the
        // changeset.
        match result {
            Ok((
                SerializedReturnValues {
                    mutable_reference_outputs: _,
                    mut return_values,
                },
                (mut change_set, events, mut native_extensions),
            )) => {
                if return_values.len() != 1 {
                    Err(async_extension_error(format!(
                        "inconsistent initializer `{}`",
                        actor.initializer
                    )))
                } else {
                    publish_actor_state(
                        &mut change_set,
                        actor_addr,
                        actor.state_tag.clone(),
                        return_values.remove(0).0,
                    )
                    .map_err(partial_vm_error_to_async)?;
                    let async_ext = native_extensions.remove::<AsyncExtension>();
                    Ok(AsyncSuccess {
                        change_set,
                        events,
                        messages: async_ext.sent,
                        gas_used,
                    })
                }
            }
            Err(error) => Err(AsyncError { error, gas_used }),
        }
    }

    /// Handles a message at `actor` with the given `message_hash`. This will call the
    /// according function as determined by the AsyncResolver, passing a reference to
    /// the actors state.
    pub fn handle_message(
        mut self,
        actor_addr: AccountAddress,
        message_hash: u64,
        mut args: Vec<Vec<u8>>,
        gas_status: &mut GasStatus,
    ) -> AsyncResult {
        // Resolve actor and function which handles the message.
        let (module_id, handler_id) =
            self.vm.message_table.get(&message_hash).ok_or_else(|| {
                async_extension_error(format!("unknown message hash `{}`", message_hash))
            })?;
        let actor = self.vm.actor_metadata.get(module_id).ok_or_else(|| {
            async_extension_error(format!(
                "actor `{}` unknown",
                module_id.short_str_lossless()
            ))
        })?;

        // Load the resource representing the actor state and add to arguments.
        let state_type_tag = TypeTag::Struct(actor.state_tag.clone());
        let state_type = self
            .vm_session
            .load_type(&state_type_tag)
            .map_err(vm_error_to_async)?;

        let actor_state_global = self
            .vm_session
            .get_data_store()
            .load_resource(actor_addr, &state_type)
            .map_err(partial_vm_error_to_async)?;
        let actor_state = actor_state_global
            .borrow_global()
            .and_then(|v| v.value_as::<Reference>())
            .and_then(|r| r.read_ref())
            .map_err(partial_vm_error_to_async)?;
        args.insert(
            0,
            self.to_bcs(actor_state, &state_type_tag)
                .map_err(partial_vm_error_to_async)?,
        );

        // Execute the handler.
        let gas_before = gas_status.remaining_gas().get();
        let result = self
            .vm_session
            .execute_function_bypass_visibility(module_id, handler_id, vec![], args, gas_status)
            .and_then(|ret| Ok((ret, self.vm_session.finish_with_extensions()?)));

        let gas_used = gas_status.remaining_gas().get() - gas_before;

        // Process the result, moving the mutated value of the handlers first parameter
        // into the changeset.
        match result {
            Ok((
                SerializedReturnValues {
                    mut mutable_reference_outputs,
                    return_values: _,
                },
                (mut change_set, events, mut native_extensions),
            )) => {
                if mutable_reference_outputs.len() > 1 {
                    Err(async_extension_error(format!(
                        "inconsistent handler `{}`",
                        handler_id
                    )))
                } else {
                    if !mutable_reference_outputs.is_empty() {
                        publish_actor_state(
                            &mut change_set,
                            actor_addr,
                            actor.state_tag.clone(),
                            mutable_reference_outputs.remove(0).1,
                        )
                        .map_err(partial_vm_error_to_async)?;
                    }
                    let async_ext = native_extensions.remove::<AsyncExtension>();
                    Ok(AsyncSuccess {
                        change_set,
                        events,
                        messages: async_ext.sent,
                        gas_used,
                    })
                }
            }
            Err(error) => Err(AsyncError { error, gas_used }),
        }
    }

    fn to_bcs(&self, value: Value, tag: &TypeTag) -> PartialVMResult<Vec<u8>> {
        let type_layout = self
            .vm_session
            .get_type_layout(tag)
            .map_err(|e| e.to_partial())?;
        value
            .simple_serialize(&type_layout)
            .ok_or_else(|| partial_extension_error("serialization failed"))
    }
}

fn make_extensions<'a>(
    actor_addr: AccountAddress,
    virtual_time: u128,
    in_initializer: bool,
) -> NativeContextExtensions<'a> {
    let mut exts = NativeContextExtensions::default();
    exts.add(AsyncExtension {
        current_actor: actor_addr,
        sent: vec![],
        in_initializer,
        virtual_time,
    });
    exts
}

fn publish_actor_state(
    change_set: &mut ChangeSet,
    actor_addr: AccountAddress,
    state_tag: StructTag,
    state: Vec<u8>,
) -> PartialVMResult<()> {
    change_set
        .publish_resource(actor_addr, state_tag, state)
        .map_err(|err| partial_extension_error(format!("cannot publish actor state: {}", err)))
}

pub(crate) fn partial_extension_error(msg: impl ToString) -> PartialVMError {
    PartialVMError::new(StatusCode::VM_EXTENSION_ERROR).with_message(msg.to_string())
}

pub(crate) fn extension_error(msg: impl ToString) -> VMError {
    partial_extension_error(msg).finish(Location::Undefined)
}

fn async_extension_error(msg: impl ToString) -> AsyncError {
    AsyncError {
        error: extension_error(msg),
        gas_used: 0,
    }
}

fn vm_error_to_async(error: VMError) -> AsyncError {
    AsyncError { error, gas_used: 0 }
}

fn partial_vm_error_to_async(error: PartialVMError) -> AsyncError {
    vm_error_to_async(error.finish(Location::Undefined))
}

// ------------------------------------------------------------------------------------------
// Displaying

impl Display for AsyncError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.error)
    }
}

impl Error for AsyncError {}

impl Display for AsyncSuccess {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        let AsyncSuccess {
            change_set,
            events,
            messages,
            gas_used,
        } = self;
        write!(f, "change_set: {:?}", change_set)?;
        write!(f, ", events: {:?}", events)?;
        write!(f, ", messages: {:?}", messages)?;
        write!(f, ", gas: {}", gas_used)
    }
}