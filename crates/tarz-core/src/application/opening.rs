use super::entry::Entry;
use super::host::HostInner;
use super::saving;
use super::types::*;
use crate::store::{PreparedCommit, SavedSessionRecord};
use std::sync::Arc;

pub(super) async fn initialize(
    host: &Arc<HostInner>,
    entry: &Arc<Entry>,
) -> Result<(), ApplicationError> {
    let (prepared, persistent) = {
        let state = entry.lock();
        if state.prepared.is_some() {
            entry.cloned_snapshot();
        }
        (state.prepared.clone(), state.persistent)
    };
    let record = if persistent {
        let store = host
            .store
            .clone()
            .ok_or(ApplicationError::StoreUnavailable)?;
        let id = entry.target.conversation_id.as_str().to_owned();
        let writer = Arc::new(saving::io(host, move || store.acquire_writer(&id)).await?);
        entry.lock().writer = Some(writer.clone());
        saving::io(host, move || writer.read_latest()).await?
    } else {
        None
    };
    let (definition, context, revision) = match &prepared {
        Some(prepared) => {
            if record.is_some() {
                return Err(ApplicationError::CreationConflict);
            }
            {
                entry.cloned_snapshot();
                (prepared.definition.clone(), prepared.context.clone(), None)
            }
        }
        None => {
            let record = record.ok_or(ApplicationError::NotFound)?;
            (record.definition, record.context, Some(record.revision))
        }
    };
    definition.validate()?;
    let pending = if persistent && let Some(prepared) = &prepared {
        entry.cloned_snapshot();
        let record = SavedSessionRecord::new(
            entry.target.conversation_id.as_str().to_owned(),
            prepared.commit_id.clone(),
            None,
            definition.clone(),
            context.clone(),
        )?;
        Some(Arc::new(PreparedCommit::new(None, record)?))
    } else {
        None
    };
    #[cfg(test)]
    let fixture = host.assemble.lock().expect("test fixture").clone();
    #[cfg(test)]
    let owner = if let Some(assemble) = fixture {
        assemble(&definition, entry.target.conversation_id.clone(), context)?
    } else {
        crate::assembly::assemble(
            &definition,
            entry.target.conversation_id.clone(),
            context,
            host.tool_resources.clone(),
        )
        .map_err(|error| ApplicationError::Assembly(error.to_string()))?
    };
    #[cfg(not(test))]
    let owner = crate::assembly::assemble(
        &definition,
        entry.target.conversation_id.clone(),
        context,
        host.tool_resources.clone(),
    )
    .map_err(|error| ApplicationError::Assembly(error.to_string()))?;
    let mut state = entry.lock();
    state.handle = Some(owner.handle());
    state.owner = Some(owner);
    state.definition = Some(definition);
    state.revision = revision;
    state.pending = pending;
    if state.closing.is_some() {
        state
            .handle
            .as_ref()
            .expect("assembled handle")
            .begin_shutdown();
    }
    if state.pending.is_some() {
        state.save_requested = state.closing != Some(ClosePolicy::DiscardLocal);
    } else {
        state.initialized = true;
        if state.closing.is_none() {
            state.lifecycle = Lifecycle::Open;
        }
        state.save = match revision {
            Some(revision) => SaveStatus::Saved {
                revision,
                material_version: entry.version(&state),
            },
            None => SaveStatus::NotTracked,
        };
    }
    entry.changed();
    Ok(())
}
