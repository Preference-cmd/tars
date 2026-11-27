use super::entry::{Entry, State};
use super::host::HostInner;
use super::types::*;
use crate::store::{CommitResolution, PreparedCommit, SavedSessionRecord};
use std::sync::Arc;

/// Running compaction values and history index for the next record.
pub(super) fn compaction_record(
    state: &State,
) -> (
    Option<bool>,
    Option<u8>,
    Vec<crate::compact::HistoryRecordEntry>,
) {
    let Some(runtime) = state.owner.as_ref().and_then(|owner| owner.compaction()) else {
        return (None, None, Vec::new());
    };
    let history = runtime.entries();
    let Some(definition) = state.definition.as_ref() else {
        return (None, None, history);
    };
    let (auto_compact, compact_threshold) = runtime.record_overrides(definition);
    (auto_compact, compact_threshold, history)
}

/// Title and last-activity time for the next record over `context`. A
/// prepared commit it replaces may already be stored, so its time is a floor.
pub(super) fn next_metadata(
    state: &State,
    context: &causa::kernel::Context,
) -> (Option<String>, u64) {
    let floor = state
        .pending
        .as_ref()
        .map_or(0, |pending| pending.record.updated_at_ms);
    let (derived_title, updated_at_ms) = state.saved.next(context, floor);
    let title = match &state.desired_title {
        Some(explicit) => explicit.clone(),
        None => derived_title,
    };
    (title, updated_at_ms)
}

pub(super) fn prepare(entry: &Entry, state: &mut State) -> Result<(), ApplicationError> {
    let context = state
        .owner
        .as_mut()
        .expect("assembled owner")
        .with_context(|context| {
            entry.cloned_snapshot();
            context.clone()
        })?;
    let current_mode = state.permissions.as_ref().map(|p| p.get().0);
    let (auto_compact, compact_threshold, history) = compaction_record(state);
    let usage = state.owner.as_ref().map(|o| o.session_usage());
    let (title, updated_at_ms) = next_metadata(state, &context);
    let record = SavedSessionRecord::new_with_compaction(
        entry.target.conversation_id.as_str().to_owned(),
        uuid::Uuid::now_v7().to_string(),
        state.revision,
        state.definition.clone().expect("assembled definition"),
        context,
        current_mode,
        auto_compact,
        compact_threshold,
        history,
        usage,
        title,
        updated_at_ms,
    )?;
    state.pending = Some(Arc::new(PreparedCommit::new(state.revision, record)?));
    if let Some(coord) = entry.coordinator.get() {
        let taken = coord.entry_inbox().take_consumed();
        state
            .pending_consumed
            .delivered_ids
            .extend(taken.delivered_ids);
        state.pending_consumed.dropped_ids.extend(taken.dropped_ids);
    }
    state.save_requested = true;
    state.pending_attempted = false;
    let coord_blocked = entry.coordinator.get().and_then(|c| c.save_blocked_error());
    if let Some(blocked_err) = coord_blocked {
        let commit_id = match &state.save {
            SaveStatus::SaveBlocked { commit_id, .. } => commit_id.clone(),
            _ => state
                .pending
                .as_ref()
                .map(|p| p.record.last_commit_id.clone())
                .unwrap_or_default(),
        };
        state.save = SaveStatus::SaveBlocked {
            error: blocked_err.clone(),
            commit_id,
            material_version: entry.version(state),
        };
        state.error = Some(ApplicationError::SaveBlocked(blocked_err));
    } else {
        state.save = SaveStatus::Dirty {
            base_revision: state.revision,
            material_version: entry.version(state),
        };
    }
    Ok(())
}

pub(super) async fn save(host: &Arc<HostInner>, entry: &Arc<Entry>) {
    let _writer_guard = entry.writer_gate.lock().await;
    let (writer, pending, consumed, attempted, history_store, entry_agent_id, media_vault) = {
        let mut state = entry.lock();
        state.save_requested = false;
        state.save_in_flight = true;
        let attempted = state.pending_attempted;
        state.pending_attempted = true;
        let mut pending = state.pending.clone().expect("pending save");
        if !attempted && pending.expected_revision != state.revision {
            let mut updated_record = pending.record.clone();
            updated_record.revision = state.revision.map_or(1, |r| r.saturating_add(1));
            if let Ok(rebuilt) = PreparedCommit::new(state.revision, updated_record) {
                pending = Arc::new(rebuilt);
                state.pending = Some(pending.clone());
            }
        }
        let consumed = state.pending_consumed.clone();
        if entry
            .coordinator
            .get()
            .and_then(|c| c.save_blocked_error())
            .is_none()
        {
            state.save = SaveStatus::Saving {
                commit_id: pending.record.last_commit_id.clone(),
                base_revision: pending.expected_revision,
                material_version: entry.version(&state),
            };
        }
        entry.changed();
        let history_store = state
            .owner
            .as_ref()
            .and_then(|o| o.compaction())
            .and_then(|c| c.history());
        (
            state.writer.clone().expect("persistent writer"),
            pending,
            consumed,
            attempted,
            history_store,
            entry.target.conversation_id.as_str().to_owned(),
            entry.media_vault.clone(),
        )
    };
    // Both the supervisor and blocking closure own the writer. Dropping a wait
    // cannot release the OS lock while serialization/replacement may still run.
    let result = io(host, move || {
        if attempted {
            match writer.resolve_commit(&pending)? {
                CommitResolution::Applied(record) => return Ok(*record),
                CommitResolution::NotApplied => {}
            }
        }
        check_artifact_references(
            &pending.record,
            history_store.as_deref(),
            Some(&media_vault),
        )?;
        let media_refs = crate::media::collect_context_managed_media_refs(&pending.record.context);
        let media_blobs = media_vault
            .snapshot_blobs_for_refs(&media_refs)
            .map_err(|err| {
                let block_id = first_block_for_media_refs(&pending.record.context, &media_refs);
                crate::store::StoreError::References(Arc::new(
                    crate::material::MaterialError::ReferenceUnavailable {
                        block_id,
                        source: std::io::Error::new(std::io::ErrorKind::NotFound, err.to_string()),
                    },
                ))
            })?;
        writer.commit_with_consumed_events_and_media(
            &pending,
            &entry_agent_id,
            &consumed.delivered_ids,
            &consumed.dropped_ids,
            &media_blobs,
        )
    })
    .await;
    let mut just_opened = false;
    let mut sync_after_save = None;
    {
        let mut state = entry.lock();
        state.save_in_flight = false;
        match result {
            Ok(record) => {
                let was_initialized = state.initialized;
                state.revision = Some(record.revision);
                state.saved = crate::store::RecordMetadata::of(&record);
                let initial_mode = state
                    .definition
                    .as_ref()
                    .map(crate::definition::SessionDefinition::initial_permission_mode)
                    .unwrap_or(crate::policy::PermissionMode::Ask);
                state.saved_effective_mode = Some(record.permission_mode.unwrap_or(initial_mode));
                if let Some(definition) = &state.definition {
                    state.saved_compaction_settings = Some((
                        record.auto_compact.unwrap_or(definition.auto_compact),
                        record
                            .compact_threshold
                            .unwrap_or(definition.compact_threshold),
                    ));
                }
                state.pending = None;
                let committed_consumed = std::mem::take(&mut state.pending_consumed);
                state.save_requested = false;
                let coord_blocked = entry.coordinator.get().and_then(|c| c.save_blocked_error());
                if let Some(blocked_err) = coord_blocked {
                    let commit_id = match &state.save {
                        SaveStatus::SaveBlocked { commit_id, .. } => commit_id.clone(),
                        _ => record.last_commit_id.clone(),
                    };
                    state.save = SaveStatus::SaveBlocked {
                        error: blocked_err.clone(),
                        commit_id,
                        material_version: entry.version(&state),
                    };
                    state.error = Some(ApplicationError::SaveBlocked(blocked_err));
                } else {
                    state.save = SaveStatus::Saved {
                        revision: record.revision,
                        material_version: entry.version(&state),
                    };
                    state.error = None;
                }
                state.active = None;
                state.initialized = true;
                if state.closing.is_none() {
                    state.lifecycle = Lifecycle::Open;
                    if !was_initialized {
                        just_opened = true;
                    }
                }
                sync_after_save = Some((
                    Some(record.revision),
                    state.generation,
                    record.context.clone(),
                    record.history.clone(),
                    record.session_usage(),
                    committed_consumed,
                ));
                if state.persistent
                    && state.active.is_none()
                    && state.closing != Some(ClosePolicy::DiscardLocal)
                    && state.has_uncommitted_metadata_or_settings()
                {
                    if let Err(error) = prepare(entry, &mut state) {
                        state.lifecycle = Lifecycle::Faulted;
                        state.error = Some(error);
                        state.fault = state.error.clone();
                    } else {
                        entry.wake.notify_one();
                    }
                }
            }
            Err(error) => {
                let pending_commit_id = state
                    .pending
                    .as_ref()
                    .expect("failed save retained")
                    .record
                    .last_commit_id
                    .clone();
                let error = match error {
                    ApplicationError::Store(error) => error,
                    other => {
                        state.lifecycle = Lifecycle::Faulted;
                        state.error = Some(other);
                        state.fault = state.error.clone();
                        state.close_in_flight = state.closing == Some(ClosePolicy::DiscardLocal);
                        entry.changed();
                        return;
                    }
                };
                if matches!(&*error, crate::store::StoreError::PendingCommit)
                    && state.writer.as_ref().is_none_or(|w| {
                        w.pending_kind() != crate::store::PendingWriterKind::RootCommit
                    })
                {
                    // Commit was rejected before touching `WriterState::pending` (e.g. a child
                    // outbox was pending). Keep `pending_attempted = false` so retry refreshes
                    // `expected_revision` once the blocking outbox clears.
                    state.pending_attempted = false;
                }
                state.save = SaveStatus::SaveBlocked {
                    error: error.clone(),
                    commit_id: pending_commit_id,
                    material_version: entry.version(&state),
                };
                state.error = Some(ApplicationError::SaveBlocked(error));
                if state.closing == Some(ClosePolicy::Save) {
                    state.close_in_flight = false;
                } else if !state.initialized {
                    state.lifecycle = Lifecycle::InitializationBlocked;
                }
            }
        }
    }
    if let Some((rev, gen_id, ctx, hist, usage, committed_consumed)) = sync_after_save
        && let Some(coord) = entry.coordinator.get()
    {
        coord.sync_entry_materials_after_save(
            rev,
            gen_id,
            Some(ctx),
            Some(hist),
            Some(usage),
            &committed_consumed,
        );
        if just_opened {
            coord.publish_lifecycle(Lifecycle::Open);
        }
    }
    entry.changed();
}

pub(super) async fn io<T: Send + 'static>(
    host: &Arc<HostInner>,
    operation: impl FnOnce() -> Result<T, crate::store::StoreError> + Send + 'static,
) -> Result<T, ApplicationError> {
    io_semaphore(&host.io, operation).await
}

pub(super) async fn io_semaphore<T: Send + 'static, E: Into<ApplicationError> + Send + 'static>(
    semaphore: &Arc<tokio::sync::Semaphore>,
    operation: impl FnOnce() -> Result<T, E> + Send + 'static,
) -> Result<T, ApplicationError> {
    let permit = semaphore
        .clone()
        .acquire_owned()
        .await
        .expect("host semaphore remains open");
    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        operation()
    })
    .await
    .map_err(|error| ApplicationError::Faulted(format!("storage task failed: {error}")))?
    .map_err(Into::into)
}

/// Deep check of the record's history, spilled artifact, and media references before
/// it is written: history references must name a record in the record's own
/// index whose file still exists and holds that original block; spilled artifact
/// references (`artifact:<id>`) must exist and be readable in SQLite `artifacts`;
/// managed `media:sha256:<64hex>` references must be present in `media_vault`.
/// Runs on the blocking pool.
fn check_artifact_references(
    record: &SavedSessionRecord,
    history: Option<&crate::compact::history::HistoryStore>,
    media_vault: Option<&crate::media::MediaVault>,
) -> Result<(), crate::store::StoreError> {
    let check_media = |block_id: causa::kernel::BlockId,
                       media: &causa::kernel::MediaRef|
     -> Result<(), crate::store::StoreError> {
        let refused = |source: std::io::Error| {
            crate::store::StoreError::References(Arc::new(
                crate::material::MaterialError::ReferenceUnavailable { block_id, source },
            ))
        };
        crate::media::validate_managed_media_ref(media).map_err(|err| {
            refused(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                err.to_string(),
            ))
        })?;
        if crate::media::is_managed_media_ref(&media.reference) {
            if let Some(vault) = media_vault
                && !vault.contains(&media.reference)
            {
                return Err(refused(std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    format!("missing managed media blob {}", media.reference),
                )));
            }
            return Ok(());
        }
        if let Ok(artifact_id) =
            crate::compact::history::parse_spilled_artifact_id(&media.reference)
        {
            let history = history.ok_or_else(|| {
                refused(std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    format!(
                        "media artifact reference {}: this session keeps no artifact store",
                        media.reference
                    ),
                ))
            })?;
            history
                .verify_spilled_artifact(artifact_id)
                .map_err(|error| match error {
                    crate::store::StoreError::Io(source) => refused(std::io::Error::new(
                        source.kind(),
                        format!("media artifact reference {}: {source}", media.reference),
                    )),
                    other => refused(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        format!("media artifact reference {}: {other}", media.reference),
                    )),
                })?;
        }
        Ok(())
    };

    for block in record.context.blocks() {
        match block.content() {
            causa::kernel::BlockContent::Message { parts, .. } => {
                for part in parts {
                    if let causa::kernel::ContentPart::Media(media) = part {
                        check_media(block.id(), media)?;
                    }
                }
            }
            causa::kernel::BlockContent::ToolResult(result) => {
                for media in &result.media {
                    check_media(block.id(), media)?;
                }
                let Some(artifact) = &result.output.artifact else {
                    continue;
                };
                let refused = |source: std::io::Error| {
                    crate::store::StoreError::References(Arc::new(
                        crate::material::MaterialError::ReferenceUnavailable {
                            block_id: block.id(),
                            source,
                        },
                    ))
                };
                let invalid = |reason: String| {
                    refused(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        format!("history artifact reference {}: {reason}", artifact.id),
                    ))
                };
                if artifact
                    .id
                    .starts_with(crate::compact::history::ARTIFACT_ID_PREFIX)
                {
                    let artifact_id =
                        crate::compact::history::parse_spilled_artifact_id(&artifact.id)
                            .map_err(|reason| invalid(reason.into()))?;
                    let history = history.ok_or_else(|| {
                        invalid("this session keeps no artifact directory".into())
                    })?;
                    history
                        .verify_spilled_artifact(artifact_id)
                        .map_err(|error| match error {
                            crate::store::StoreError::Io(source) => refused(std::io::Error::new(
                                source.kind(),
                                format!("artifact reference {}: {source}", artifact.id),
                            )),
                            other => invalid(other.to_string()),
                        })?;
                    continue;
                }
                let (record_id, block_id) =
                    crate::compact::history::parse_artifact_id(&artifact.id)
                        .map_err(|reason| invalid(reason.into()))?;
                if !record
                    .history
                    .iter()
                    .any(|entry| entry.record_id == record_id)
                {
                    return Err(invalid(
                        "the record is not in this session's history index".into(),
                    ));
                }
                let history = history
                    .ok_or_else(|| invalid("this session keeps no history records".into()))?;
                history
                    .verify_artifact_reference(record_id, block_id)
                    .map_err(|error| match error {
                        crate::store::StoreError::Io(source) => refused(std::io::Error::new(
                            source.kind(),
                            format!("history artifact reference {}: {source}", artifact.id),
                        )),
                        other => invalid(other.to_string()),
                    })?;
            }
            causa::kernel::BlockContent::ToolCall(_)
            | causa::kernel::BlockContent::ToolChange(_) => {}
        }
    }
    Ok(())
}

fn first_block_for_media_refs(
    context: &causa::kernel::Context,
    refs: &std::collections::BTreeSet<String>,
) -> causa::kernel::BlockId {
    for block in context.blocks() {
        match block.content() {
            causa::kernel::BlockContent::Message { parts, .. } => {
                if parts.iter().any(|part| {
                    matches!(part, causa::kernel::ContentPart::Media(m) if refs.contains(&m.reference))
                }) {
                    return block.id();
                }
            }
            causa::kernel::BlockContent::ToolResult(result) => {
                if result.media.iter().any(|m| refs.contains(&m.reference)) {
                    return block.id();
                }
            }
            causa::kernel::BlockContent::ToolCall(_) | causa::kernel::BlockContent::ToolChange(_) => {}
        }
    }
    causa::kernel::BlockId::new(uuid::Uuid::nil())
}
