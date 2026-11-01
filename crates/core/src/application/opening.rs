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
        let record = saving::io(host, move || writer.read_latest()).await?;
        // Under the write lock, drop segment files the record does not
        // reference: an interrupted compaction leaves an orphan behind.
        let store = host
            .store
            .clone()
            .ok_or(ApplicationError::StoreUnavailable)?;
        let history = record
            .as_ref()
            .map(|record| record.history.clone())
            .unwrap_or_default();
        let id = entry.target.conversation_id.as_str().to_owned();
        saving::io(host, move || {
            crate::compact::history::cleanup_segments(store.directory(), &id, &history)
        })
        .await?;
        record
    } else {
        None
    };
    let (
        definition,
        context,
        revision,
        current_mode,
        open_report,
        history,
        auto_compact,
        compact_threshold,
    ) = match &prepared {
        Some(prepared) => {
            if record.is_some() {
                return Err(ApplicationError::CreationConflict);
            }
            entry.cloned_snapshot();
            let mut definition = prepared.definition.clone();
            if definition.permission_mode == Some(crate::permission::PermissionMode::Ask) {
                definition.permission_mode = None;
            }
            let initial = definition.initial_permission_mode();
            (
                definition,
                prepared.context.clone(),
                None,
                initial,
                OpenReport::default(),
                Vec::new(),
                None,
                None,
            )
        }
        None => {
            let record = record.ok_or(ApplicationError::NotFound)?;
            let initial_mode = record.definition.initial_permission_mode();
            let mut mode = record.permission_mode.unwrap_or(initial_mode);
            let permission_downgraded = record.permission_mode
                == Some(crate::permission::PermissionMode::FullAccess)
                && initial_mode != crate::permission::PermissionMode::FullAccess;
            if permission_downgraded {
                mode = crate::permission::PermissionMode::Ask;
            }
            (
                record.definition,
                record.context,
                Some(record.revision),
                mode,
                OpenReport {
                    permission_downgraded,
                },
                record.history,
                record.auto_compact,
                record.compact_threshold,
            )
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
            definition.permission_mode,
        )?;
        Some(Arc::new(PreparedCommit::new(None, record)?))
    } else {
        None
    };
    #[cfg(test)]
    let fixture = host.assemble.lock().expect("test fixture").clone();
    // A test fixture injects its own Session and never starts servers.
    #[cfg(test)]
    let connect = fixture.is_none() && host.options.connect_mcp_servers;
    #[cfg(not(test))]
    let connect = host.options.connect_mcp_servers;
    let mcp = if connect {
        start_mcp(&definition).await?
    } else {
        None
    };
    #[cfg(test)]
    let owner = if let Some(assemble) = fixture {
        assemble(&definition, entry.target.conversation_id.clone(), context)?
    } else {
        assembled(
            host,
            &definition,
            entry.target.conversation_id.clone(),
            context,
            current_mode,
            mcp,
            // Creating must fail closed without a backend; reopening may keep
            // the saved command tool and reject each call at runtime.
            prepared.is_none(),
            persistent,
            history,
            auto_compact,
            compact_threshold,
        )
        .await?
    };
    #[cfg(not(test))]
    let owner = assembled(
        host,
        &definition,
        entry.target.conversation_id.clone(),
        context,
        current_mode,
        mcp,
        prepared.is_none(),
        persistent,
        history,
        auto_compact,
        compact_threshold,
    )
    .await?;
    let mut state = entry.lock();
    state.handle = Some(owner.handle());
    state.owner = Some(owner);
    state.definition = Some(definition);
    state.revision = revision;
    state.pending = pending;
    state.open_report = open_report;
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

async fn start_mcp(
    definition: &crate::definition::SessionDefinition,
) -> Result<Option<Arc<crate::mcp::McpRuntime>>, ApplicationError> {
    crate::mcp::McpRuntime::start(definition)
        .await
        .map_err(|error| ApplicationError::Assembly(error.to_string()))
}

#[allow(clippy::too_many_arguments)]
async fn assembled(
    host: &Arc<HostInner>,
    definition: &crate::definition::SessionDefinition,
    conversation_id: crate::session::ConversationId,
    context: causa::kernel::Context,
    current_mode: crate::permission::PermissionMode,
    mcp: Option<Arc<crate::mcp::McpRuntime>>,
    allow_unavailable_command: bool,
    persistent: bool,
    history: Vec<crate::compact::HistorySegment>,
    auto_compact: Option<bool>,
    compact_threshold: Option<u8>,
) -> Result<crate::session::Session, ApplicationError> {
    let base = crate::assembly::SessionResources::new(host.tool_resources.clone())
        .with_storage(
            host.store
                .as_ref()
                .map(|store| store.directory().to_path_buf()),
        )
        .with_compaction(history, auto_compact, compact_threshold)
        .persistent(persistent);
    #[cfg(test)]
    let base = {
        let sandbox = host
            .command_sandbox
            .lock()
            .expect("test command sandbox")
            .take();
        match sandbox {
            Some(sandbox) => base.with_test_command_sandbox(sandbox),
            None => base,
        }
    };
    #[cfg(test)]
    let resources = {
        let gateway = host.gateway_override.lock().expect("test gateway").clone();
        match gateway {
            Some(gateway) => base.with_test_gateway(gateway),
            None => base,
        }
    };
    #[cfg(not(test))]
    let resources = base;
    match crate::assembly::assemble(
        definition,
        conversation_id,
        context,
        resources,
        current_mode,
        mcp.clone(),
        allow_unavailable_command,
    ) {
        Ok(owner) => Ok(owner),
        Err(error) => {
            // Assembly owns nothing yet, so close the connections it cannot use.
            if let Some(mcp) = mcp {
                mcp.close().await;
            }
            Err(ApplicationError::Assembly(error.to_string()))
        }
    }
}
