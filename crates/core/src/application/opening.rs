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
        saving::io(host, move || {
            crate::compact::history::cleanup_records(store.directory(), &id, &history)?;
            crate::compact::history::cleanup_artifacts(
                store.directory(),
                &id,
                &history,
                saved_context.as_ref(),
            )
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
                    workspace_rules: WorkspaceRulesReport::default(),
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
            usage,
            prepared.as_ref().and_then(|p| p.skill_registry.clone()),
            command_rules,
            workspace_rules_backend.clone(),
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
        usage,
        prepared.as_ref().and_then(|p| p.skill_registry.clone()),
        command_rules,
        workspace_rules_backend,
    )
    .await?;
    let mut state = entry.lock();
    state.handle = Some(owner.handle());
    state.owner = Some(owner);
    state.definition = Some(definition);
    state.revision = revision;
    state.pending = pending;
    state.open_report = open_report;
    state.workspace_rules_snapshot = workspace_rules_snapshot;
    state.saved = saved;
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
    history: Vec<crate::compact::HistoryRecordEntry>,
    auto_compact: Option<bool>,
    compact_threshold: Option<u8>,
    usage: Option<crate::usage::SessionUsage>,
    initial_skills: Option<crate::skills::SkillRegistry>,
    command_rules: crate::permission::CommandRuleSet,
    workspace_rules_backend: Option<crate::permission::WorkspaceRulesBackend>,
) -> Result<crate::session::Session, ApplicationError> {
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
    let base = crate::assembly::SessionResources::new(host.tool_resources.clone())
        .with_storage(
            host.store
                .as_ref()
                .map(|store| store.directory().to_path_buf()),
        )
        .with_compaction(history, auto_compact, compact_threshold)
        .with_usage(usage)
        .with_skills(Arc::new(std::sync::RwLock::new(registry)))
        .with_command_rules(command_rules)
        .with_workspace_rules_backend(workspace_rules_backend)
        .persistent(persistent);
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
