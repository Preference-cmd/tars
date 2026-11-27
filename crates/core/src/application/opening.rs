use super::coordinator::SessionCoordinator;
use super::entry::Entry;
use super::host::HostInner;
use super::saving;
use super::types::*;
use crate::agent::{AgentDescriptor, AgentUnit};
use crate::events::{AgentInteraction, DEFAULT_NOTIFICATION_INLINE_MAX_BYTES, InboxDrainHandle};
use crate::permission::ApprovalsHub;
use crate::store::{PreparedCommit, SavedSessionRecord};
use std::collections::HashMap;
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
    let (record, restored_agents, restored_operations, restored_pending_events) = if persistent {
        let store = host
            .store
            .clone()
            .ok_or(ApplicationError::StoreUnavailable)?;
        let id = entry.target.conversation_id.as_str().to_owned();
        let writer = Arc::new(saving::io(host, move || store.acquire_writer(&id)).await?);
        entry.lock().writer = Some(writer.clone());
        let writer_for_read = writer.clone();
        let record = saving::io(host, move || writer_for_read.read_latest()).await?;
        let (restored_agents, restored_operations, restored_pending_events) = if record.is_some() {
            let now_ms = crate::time::current_time_ms();
            let writer_for_reconcile = writer.clone();
            saving::io(host, move || {
                writer_for_reconcile.reconcile_on_reopen(now_ms)
            })
            .await?;
            let store = host
                .store
                .clone()
                .ok_or(ApplicationError::StoreUnavailable)?;
            let id = entry.target.conversation_id.as_str().to_owned();
            saving::io(host, move || {
                let agents = store.list_agents(&id)?;
                let operations = store.list_operations(&id)?;
                let mut pending = HashMap::new();
                for agent in &agents {
                    let events = store.list_pending_inbox_events(&id, &agent.agent_id)?;
                    pending.insert(agent.agent_id.clone(), events);
                }
                Ok((agents, operations, pending))
            })
            .await?
        } else {
            (Vec::new(), Vec::new(), HashMap::new())
        };
        // Under the write lock, drop record files the record does not
        // reference: an interrupted compaction leaves an orphan behind.
        let store = host
            .store
            .clone()
            .ok_or(ApplicationError::StoreUnavailable)?;
        let history = record
            .as_ref()
            .map(|record| record.history.clone())
            .unwrap_or_default();
        let saved_context = record.as_ref().map(|record| record.context.clone());
        let id = entry.target.conversation_id.as_str().to_owned();
        let extra_cleanups: Vec<(
            String,
            Vec<crate::compact::HistoryRecordEntry>,
            Option<causa::kernel::Context>,
        )> = restored_agents
            .iter()
            .filter(|a| a.agent_id != id)
            .map(|a| {
                let hist = serde_json::from_str(&a.history_index_json).unwrap_or_default();
                let ctx = crate::material::decode_stored(a.context_json.as_bytes()).ok();
                (a.agent_id.clone(), hist, ctx)
            })
            .collect();
        let media_vault = entry.media_vault.clone();
        let writer_for_media = writer.clone();
        let record_for_media = record.clone();
        saving::io(host, move || {
            crate::compact::history::cleanup_records(store.root(), &id, &history)?;
            crate::compact::history::cleanup_artifacts(
                store.root(),
                &id,
                &history,
                saved_context.as_ref(),
            )?;
            for (agent_id, hist, ctx) in &extra_cleanups {
                crate::compact::history::cleanup_records_for_agent(
                    store.root(),
                    &id,
                    agent_id,
                    hist,
                )?;
                crate::compact::history::cleanup_artifacts_for_agent(
                    store.root(),
                    &id,
                    agent_id,
                    hist,
                    ctx.as_ref(),
                )?;
            }
            if let Some(rec) = &record_for_media {
                let blobs = writer_for_media.load_managed_media_blobs(rec)?;
                for (reference, bytes) in blobs {
                    media_vault
                        .load_verified_blob(&reference, bytes)
                        .map_err(|err| {
                            crate::store::StoreError::References(Arc::new(
                                crate::material::MaterialError::ReferenceUnavailable {
                                    block_id: causa::kernel::BlockId::new(uuid::Uuid::nil()),
                                    source: std::io::Error::new(
                                        std::io::ErrorKind::InvalidData,
                                        err.to_string(),
                                    ),
                                },
                            ))
                        })?;
                }
            }
            Ok(())
        })
        .await?;
        (
            record,
            restored_agents,
            restored_operations,
            restored_pending_events,
        )
    } else {
        (None, Vec::new(), Vec::new(), HashMap::new())
    };
    let (
        definition,
        context,
        revision,
        current_mode,
        mut open_report,
        history,
        auto_compact,
        compact_threshold,
        usage,
        saved,
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
            for (reference, bytes) in &prepared.media_blobs {
                entry
                    .media_vault
                    .load_verified_blob(reference, bytes.clone())?;
            }
            for reference in crate::media::collect_context_managed_media_refs(&prepared.context) {
                if !entry.media_vault.contains(&reference) {
                    return Err(crate::media::MediaError::MissingMedia { reference }.into());
                }
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
                None,
                crate::store::RecordMetadata::unsaved(entry.target.conversation_id.as_str()),
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
            let usage = record.session_usage();
            let saved = crate::store::RecordMetadata::of(&record);
            (
                record.definition,
                record.context,
                Some(record.revision),
                mode,
                OpenReport {
                    permission_downgraded,
                    ..OpenReport::default()
                },
                record.history,
                record.auto_compact,
                record.compact_threshold,
                Some(usage),
                saved,
            )
        }
    };
    definition.validate()?;
    let loaded_rules = load_command_rules(host, &definition).await?;
    open_report.workspace_rules = loaded_rules.report;
    let workspace_rules_snapshot = loaded_rules.snapshot;
    let command_rules = loaded_rules.rule_set;
    let workspace_rules_backend = loaded_rules.backend;
    let loaded_agents = load_workspace_agents(host, &definition).await?;
    open_report.workspace_agents = loaded_agents.report;
    let workspace_agents_snapshot = loaded_agents.snapshot;
    let trusted_workspace_agents = loaded_agents.trusted_presets;
    let initial_agent_registry = if definition.prelude.agents_enabled {
        let cfg_dir = definition.prelude.config_dir.clone();
        let agent_dirs = definition.prelude.agent_dirs.clone();
        let ws_presets = trusted_workspace_agents.clone();
        saving::io_semaphore(&host.io, move || {
            Ok::<_, ApplicationError>(
                crate::agents::AgentDiscovery::discover(
                    cfg_dir.as_deref(),
                    &agent_dirs,
                    &ws_presets,
                )
                .registry,
            )
        })
        .await?
    } else {
        crate::agents::AgentRegistry::new()
    };
    *entry.agent_registry.write().expect("agent registry lock") = initial_agent_registry;
    let pending = if persistent && let Some(prepared) = &prepared {
        entry.cloned_snapshot();
        let (title, updated_at_ms) = saved.next(&context, 0);
        let record = SavedSessionRecord::new(
            entry.target.conversation_id.as_str().to_owned(),
            prepared.commit_id.clone(),
            None,
            definition.clone(),
            context.clone(),
            definition.permission_mode,
            title,
            updated_at_ms,
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
    let entry_id = entry.target.conversation_id.as_str().to_owned();
    let entry_pending_events = restored_pending_events
        .get(&entry_id)
        .cloned()
        .unwrap_or_default();
    let entry_inbox = Arc::new(InboxDrainHandle::with_initial(
        entry_pending_events.clone(),
        DEFAULT_NOTIFICATION_INLINE_MAX_BYTES,
    ));
    let shared_approvals = Arc::new(ApprovalsHub::new());
    let entry_context_snapshot = context.clone();
    let entry_history_snapshot = history.clone();
    let entry_usage_snapshot = usage.unwrap_or_default();
    let coord_command_rules = command_rules.clone();
    let coord_workspace_rules_backend = workspace_rules_backend.clone();
    let extra_tools = if definition.prelude.agents_enabled {
        crate::collaboration::collaboration_tools(
            crate::collaboration::CollaborationContext::for_entry(
                Arc::downgrade(entry),
                entry_id.clone(),
            ),
        )
    } else {
        Vec::new()
    };

    #[cfg(test)]
    let (owner, permissions) = if let Some(assemble) = fixture {
        (
            assemble(&definition, entry.target.conversation_id.clone(), context)?,
            None,
        )
    } else {
        let assembled_session = assembled(
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
            usage,
            prepared.as_ref().and_then(|p| p.skill_registry.clone()),
            command_rules,
            workspace_rules_backend.clone(),
            Some(entry_inbox.clone()),
            Some(shared_approvals.clone()),
            extra_tools.clone(),
            Some(entry.media_vault.clone()),
        )
        .await?;
        (
            assembled_session.session,
            Some(assembled_session.permissions),
        )
    };
    #[cfg(not(test))]
    let (owner, permissions) = {
        let assembled_session = assembled(
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
            usage,
            prepared.as_ref().and_then(|p| p.skill_registry.clone()),
            command_rules,
            workspace_rules_backend,
            Some(entry_inbox.clone()),
            Some(shared_approvals.clone()),
            extra_tools,
            Some(entry.media_vault.clone()),
        )
        .await?;
        (
            assembled_session.session,
            Some(assembled_session.permissions),
        )
    };
    if let Some(history_store) = owner.compaction().and_then(|c| c.history()) {
        entry.media_vault.bind_history_store(history_store);
    }

    let created_at_ms = crate::time::created_at_from_id(&entry_id).unwrap_or(saved.updated_at_ms);
    let entry_descriptor = AgentDescriptor {
        agent_id: entry_id.clone(),
        name: "main".to_owned(),
        preset_name: None,
        interaction: AgentInteraction::Reusable,
        definition: definition.clone(),
        role_prompt: None,
        created_at_ms,
    };
    let store_root = persistent
        .then(|| host.store.as_ref().map(|s| s.root().to_path_buf()))
        .flatten();
    let mut entry_unit = AgentUnit::new(
        &entry_id,
        entry_descriptor,
        0,
        revision.unwrap_or(0),
        entry_context_snapshot,
        entry_history_snapshot,
        entry_usage_snapshot,
        entry_pending_events,
        0,
        store_root.as_deref(),
    );
    entry_unit.inbox = entry_inbox;
    entry_unit.permissions = permissions.clone();

    let coordinator = SessionCoordinator::new(
        entry_id,
        host.options.coordinator.clone(),
        persistent,
        host.clone(),
        Arc::downgrade(entry),
        shared_approvals,
        entry.writer_gate.clone(),
        entry_unit,
        restored_agents,
        restored_operations,
        restored_pending_events,
        coord_command_rules,
        coord_workspace_rules_backend,
    );
    let _ = entry.coordinator.set(coordinator.clone());
    if let Ok(runtime) = tokio::runtime::Handle::try_current() {
        coordinator.start_scheduler(&runtime);
    }

    let mut state = entry.lock();
    state.handle = Some(owner.handle());
    state.owner = Some(owner);
    state.permissions = permissions;
    state.definition = Some(definition);
    state.revision = revision;
    state.pending = pending;
    state.open_report = open_report;
    state.workspace_rules_snapshot = workspace_rules_snapshot;
    state.workspace_agents_snapshot = workspace_agents_snapshot;
    state.trusted_workspace_agents = trusted_workspace_agents;
    state.saved = saved;
    let (def_auto, def_thresh) = {
        let def_ref = state.definition.as_ref().expect("published definition");
        (def_ref.auto_compact, def_ref.compact_threshold)
    };
    state.saved_effective_mode = Some(current_mode);
    state.saved_compaction_settings = Some((
        auto_compact.unwrap_or(def_auto),
        compact_threshold.unwrap_or(def_thresh),
    ));
    if state.closing.is_some() {
        state
            .handle
            .as_ref()
            .expect("assembled handle")
            .begin_shutdown();
        coordinator.begin_session_shutdown();
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
    let is_open = state.lifecycle == Lifecycle::Open;
    drop(state);
    if is_open {
        coordinator.publish_lifecycle(Lifecycle::Open);
    }
    entry.changed();
    Ok(())
}

struct LoadedCommandRules {
    rule_set: crate::permission::CommandRuleSet,
    report: WorkspaceRulesReport,
    snapshot: Option<super::entry::WorkspaceRulesSnapshot>,
    backend: Option<crate::permission::WorkspaceRulesBackend>,
}

async fn load_command_rules(
    host: &Arc<HostInner>,
    definition: &crate::definition::SessionDefinition,
) -> Result<LoadedCommandRules, ApplicationError> {
    let user_rules = match host.options.user_rules_path.clone() {
        Some(path) => saving::io_semaphore(&host.io, move || {
            crate::permission::rules::load_rules_file(&path)
        })
        .await?
        .map(|loaded| loaded.rules)
        .unwrap_or_default(),
        None => Vec::new(),
    };

    let Some(root) = definition.workspace_root.clone() else {
        return Ok(LoadedCommandRules {
            rule_set: crate::permission::CommandRuleSet::with_layers(
                user_rules,
                Vec::new(),
                Vec::new(),
            ),
            report: WorkspaceRulesReport {
                can_trust: host.store.is_some(),
                trusted: true,
                ..WorkspaceRulesReport::default()
            },
            snapshot: None,
            backend: None,
        });
    };

    let store = host.store.clone();
    let ws_loaded = saving::io_semaphore(&host.io, move || {
        let canonical_root = match std::fs::canonicalize(&root) {
            Ok(path) => path,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok::<_, ApplicationError>(None);
            }
            Err(error) => {
                return Err(ApplicationError::PermissionRules(
                    crate::permission::rules::RulesFileError::Io {
                        path: root.join(".tars").join("permissions.toml"),
                        kind: error.kind(),
                    },
                ));
            }
        };
        let rules_path = canonical_root.join(".tars").join("permissions.toml");
        let loaded = crate::permission::rules::load_rules_file(&rules_path)?;
        let trusted_copy = match (&loaded, &store) {
            (Some(_), Some(store)) => store.read_workspace_rules_trust(&canonical_root)?,
            _ => None,
        };
        Ok(Some((canonical_root, rules_path, loaded, trusted_copy)))
    })
    .await?;

    let (canonical_root, rules_path, loaded, trusted_copy) = match ws_loaded {
        Some((canonical_root, rules_path, Some(loaded), trusted_copy)) => {
            (canonical_root, rules_path, loaded, trusted_copy)
        }
        Some((canonical_root, rules_path, None, _)) => {
            let backend =
                host.store
                    .as_ref()
                    .map(|store| crate::permission::WorkspaceRulesBackend {
                        workspace_root: canonical_root,
                        store: store.clone(),
                        trusted: true,
                        trusted_raw_text: None,
                    });
            return Ok(LoadedCommandRules {
                rule_set: crate::permission::CommandRuleSet::with_layers(
                    user_rules,
                    Vec::new(),
                    Vec::new(),
                ),
                report: WorkspaceRulesReport {
                    file_path: Some(rules_path),
                    can_trust: host.store.is_some(),
                    trusted: true,
                    ..WorkspaceRulesReport::default()
                },
                snapshot: None,
                backend,
            });
        }
        None => {
            let file_path = definition
                .workspace_root
                .as_ref()
                .expect("root")
                .join(".tars")
                .join("permissions.toml");
            return Ok(LoadedCommandRules {
                rule_set: crate::permission::CommandRuleSet::with_layers(
                    user_rules,
                    Vec::new(),
                    Vec::new(),
                ),
                report: WorkspaceRulesReport {
                    file_path: Some(file_path),
                    can_trust: host.store.is_some(),
                    trusted: true,
                    ..WorkspaceRulesReport::default()
                },
                snapshot: None,
                backend: None,
            });
        }
    };

    let is_trusted = trusted_copy
        .as_ref()
        .is_some_and(|previous| previous.as_bytes() == loaded.raw_text.as_bytes());
    let (active_ws_rules, report) = if is_trusted {
        (
            loaded.rules.clone(),
            WorkspaceRulesReport {
                allow_rules: Vec::new(),
                file_path: Some(rules_path),
                previous_trusted_text: None,
                diff: None,
                can_trust: true,
                trusted: true,
            },
        )
    } else {
        let active = loaded
            .rules
            .iter()
            .filter(|rule| rule.decision != crate::permission::RuleDecision::Allow)
            .cloned()
            .collect();
        let allow_rules = loaded
            .rules
            .iter()
            .filter(|rule| rule.decision == crate::permission::RuleDecision::Allow)
            .cloned()
            .collect();
        let diff = trusted_copy.as_deref().map(|previous| {
            crate::permission::rules::workspace_rules_diff(previous, &loaded.raw_text)
        });
        (
            active,
            WorkspaceRulesReport {
                allow_rules,
                file_path: Some(rules_path),
                previous_trusted_text: trusted_copy,
                diff,
                can_trust: host.store.is_some(),
                trusted: false,
            },
        )
    };

    let backend = host
        .store
        .as_ref()
        .map(|store| crate::permission::WorkspaceRulesBackend {
            workspace_root: canonical_root.clone(),
            store: store.clone(),
            trusted: is_trusted,
            trusted_raw_text: is_trusted.then(|| loaded.raw_text.clone()),
        });

    let snapshot = Some(super::entry::WorkspaceRulesSnapshot {
        workspace_root: canonical_root,
        raw_text: loaded.raw_text,
        rules: loaded.rules,
    });

    Ok(LoadedCommandRules {
        rule_set: crate::permission::CommandRuleSet::with_layers(
            user_rules,
            active_ws_rules,
            Vec::new(),
        ),
        report,
        snapshot,
        backend,
    })
}

struct LoadedWorkspaceAgents {
    report: crate::agents::WorkspaceAgentsReport,
    snapshot: Option<crate::agents::WorkspaceAgentsSnapshot>,
    trusted_presets: Vec<crate::agents::AgentPreset>,
}

async fn load_workspace_agents(
    host: &Arc<HostInner>,
    definition: &crate::definition::SessionDefinition,
) -> Result<LoadedWorkspaceAgents, ApplicationError> {
    let Some(root) = definition.workspace_root.clone() else {
        return Ok(LoadedWorkspaceAgents {
            report: crate::agents::WorkspaceAgentsReport {
                can_trust: host.store.is_some(),
                trusted: true,
                ..Default::default()
            },
            snapshot: None,
            trusted_presets: Vec::new(),
        });
    };

    if !definition.prelude.agents_enabled || !definition.prelude.include_workspace_agents {
        let agents_dir = std::fs::canonicalize(&root)
            .unwrap_or(root)
            .join(".tars")
            .join("agents");
        return Ok(LoadedWorkspaceAgents {
            report: crate::agents::WorkspaceAgentsReport {
                agents_dir: Some(agents_dir),
                can_trust: host.store.is_some(),
                trusted: true,
                ..Default::default()
            },
            snapshot: None,
            trusted_presets: Vec::new(),
        });
    }

    let store = host.store.clone();
    let can_trust = store.is_some();
    let loaded = saving::io_semaphore(&host.io, move || {
        let snapshot = crate::agents::scan_workspace_agents(&root);
        let trusted_copy = match (&store, snapshot.manifest_text.is_empty()) {
            (Some(store), false) => store.read_workspace_agents_trust(&snapshot.workspace_root)?,
            _ => None,
        };
        Ok::<_, ApplicationError>((snapshot, trusted_copy))
    })
    .await?;

    let (snapshot, trusted_copy) = loaded;
    if snapshot.manifest_text.is_empty() {
        return Ok(LoadedWorkspaceAgents {
            report: crate::agents::WorkspaceAgentsReport {
                agents_dir: Some(snapshot.agents_dir),
                untrusted_agents: Vec::new(),
                trusted: true,
                can_trust,
                diagnostics: snapshot.diagnostics,
            },
            snapshot: None,
            trusted_presets: Vec::new(),
        });
    }

    let is_trusted = trusted_copy
        .as_deref()
        .is_some_and(|prev| prev == snapshot.manifest_text);
    if is_trusted {
        Ok(LoadedWorkspaceAgents {
            report: crate::agents::WorkspaceAgentsReport {
                agents_dir: Some(snapshot.agents_dir),
                untrusted_agents: Vec::new(),
                trusted: true,
                can_trust,
                diagnostics: snapshot.diagnostics,
            },
            snapshot: None,
            trusted_presets: snapshot.presets,
        })
    } else {
        let mut untrusted_agents: Vec<String> =
            snapshot.presets.iter().map(|p| p.name.clone()).collect();
        untrusted_agents.sort();
        Ok(LoadedWorkspaceAgents {
            report: crate::agents::WorkspaceAgentsReport {
                agents_dir: Some(snapshot.agents_dir.clone()),
                untrusted_agents,
                trusted: false,
                can_trust,
                diagnostics: snapshot.diagnostics.clone(),
            },
            snapshot: Some(snapshot),
            trusted_presets: Vec::new(),
        })
    }
}

async fn start_mcp(
    definition: &crate::definition::SessionDefinition,
) -> Result<Option<Arc<crate::mcp::McpRuntime>>, ApplicationError> {
    crate::mcp::McpRuntime::start(definition)
        .await
        .map_err(|error| ApplicationError::Assembly(error.into()))
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
    history: Vec<crate::compact::HistoryRecordEntry>,
    auto_compact: Option<bool>,
    compact_threshold: Option<u8>,
    usage: Option<crate::usage::SessionUsage>,
    initial_skills: Option<crate::skills::SkillRegistry>,
    command_rules: crate::permission::CommandRuleSet,
    workspace_rules_backend: Option<crate::permission::WorkspaceRulesBackend>,
    inbox: Option<Arc<InboxDrainHandle>>,
    shared_approvals: Option<Arc<ApprovalsHub>>,
    extra_tools: Vec<Arc<dyn causa::kernel::Tool>>,
    media_vault: Option<Arc<crate::media::MediaVault>>,
) -> Result<crate::assembly::AssembledSession, ApplicationError> {
    let registry = match initial_skills {
        Some(registry) => registry,
        None if definition.prelude.skills_enabled => {
            crate::skills::SkillDiscovery::discover(
                &definition.prelude.skill_dirs,
                definition.workspace_root.as_deref(),
                definition.prelude.include_workspace_skills,
            )
            .registry
        }
        None => crate::skills::SkillRegistry::new(),
    };
    let mut base = crate::assembly::SessionResources::new(host.tool_resources.clone())
        .with_storage(host.store.as_ref().map(|store| store.root().to_path_buf()))
        .with_compaction(history, auto_compact, compact_threshold)
        .with_usage(usage)
        .with_skills(Arc::new(std::sync::RwLock::new(registry)))
        .with_command_rules(command_rules)
        .with_workspace_rules_backend(workspace_rules_backend)
        .persistent(persistent);
    if let Some(inbox) = inbox {
        base = base.with_inbox(inbox);
    }
    if let Some(approvals) = shared_approvals {
        base = base.with_shared_approvals(approvals);
    }
    if !extra_tools.is_empty() {
        base = base.with_extra_tools(extra_tools);
    }
    if let Some(media_vault) = media_vault {
        base = base.with_media_vault(media_vault);
    }
    let base = match host.external_tools.clone() {
        Some(tools) => base.with_external_tools(tools),
        None => base,
    };
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
        let host_gw = host.gateway_override.lock().expect("test gateway").clone();
        let gateway = host_gw.or_else(|| {
            host.agent_gateway_override
                .lock()
                .expect("test agent gateway")
                .as_ref()
                .and_then(|f| f("main", definition))
        });
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
        Ok(assembled) => Ok(assembled),
        Err(error) => {
            // Assembly owns nothing yet, so close the connections it cannot use.
            if let Some(mcp) = mcp {
                mcp.close().await;
            }
            Err(ApplicationError::Assembly(error))
        }
    }
}
