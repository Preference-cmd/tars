use super::entry::Entry;
use super::managed::ManagedSession;
use super::supervisor;
use super::types::*;
use crate::session::{ConversationId, WorkRef};
use crate::store::SessionStore;
use causa::kernel::ToolBatch;
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};
use tokio::sync::Semaphore;

/// Runtime configuration fields updatable in place on an active [`SessionHost`].
#[derive(Clone, Debug, Default)]
pub(super) struct HostRuntimeConfig {
    pub credentials_path: Option<PathBuf>,
    pub user_rules_path: Option<PathBuf>,
    pub model_profiles: BTreeMap<String, crate::agents::ModelProfile>,
}

/// Shared lifecycle owner. Dropping a view or waiter never closes a session.
pub struct SessionHost {
    pub(super) inner: Arc<HostInner>,
}
pub(super) struct HostInner {
    pub id: uuid::Uuid,
    pub entries: Mutex<HashMap<ConversationId, Arc<Entry>>>,
    pub store: Option<SessionStore>,
    pub options: HostOptions,
    pub runtime_config: RwLock<HostRuntimeConfig>,
    pub io: Arc<Semaphore>,
    pub tool_resources: Arc<crate::tools::ToolResources>,
    /// Process-wide external-tool check, when the entry point resolved one.
    pub external_tools: Option<crate::external_tools::ExternalTools>,
    pub snapshot_clones: Arc<std::sync::atomic::AtomicU64>,
    pub external_hosts: std::sync::atomic::AtomicUsize,
    #[cfg(test)]
    pub assemble: Mutex<Option<Arc<TestAssembly>>>,
    /// Test-only model gateway injection for the real assembly path.
    #[cfg(test)]
    pub gateway_override: Mutex<Option<Arc<dyn causa::kernel::ModelGateway>>>,
    /// Test-only per-agent gateway factory for deterministic multi-agent tests.
    #[cfg(test)]
    pub agent_gateway_override: Mutex<Option<Arc<TestAgentGatewayFactory>>>,
    /// Test-only command sandbox probe injection for the real assembly path.
    #[cfg(test)]
    pub command_sandbox:
        Mutex<Option<Result<crate::sandbox::Sandbox, crate::sandbox::SandboxUnavailable>>>,
}
#[cfg(test)]
pub(super) type TestAgentGatewayFactory = dyn Fn(&str, &crate::definition::SessionDefinition) -> Option<Arc<dyn causa::kernel::ModelGateway>>
    + Send
    + Sync;
#[cfg(test)]
pub(super) type TestAssembly = dyn Fn(
        &crate::definition::SessionDefinition,
        ConversationId,
        causa::kernel::Context,
    ) -> Result<crate::session::Session, ApplicationError>
    + Send
    + Sync;

impl HostInner {
    pub(super) fn credentials_path(&self) -> Option<PathBuf> {
        self.runtime_config
            .read()
            .expect("runtime config lock")
            .credentials_path
            .clone()
    }

    pub(super) fn user_rules_path(&self) -> Option<PathBuf> {
        self.runtime_config
            .read()
            .expect("runtime config lock")
            .user_rules_path
            .clone()
    }

    pub(super) fn model_profiles(&self) -> BTreeMap<String, crate::agents::ModelProfile> {
        self.runtime_config
            .read()
            .expect("runtime config lock")
            .model_profiles
            .clone()
    }
}

impl SessionHost {
    /// Construct a host with explicit optional local storage and default bounds.
    pub fn new(store_root: Option<PathBuf>) -> Self {
        Self::with_options(store_root, HostOptions::default()).expect("positive defaults")
    }
    /// Attach the process-wide external-tool check used to bind `search_text`.
    ///
    /// Entry points resolve it once from their loaded configuration. A host
    /// without it still creates sessions; search calls are then rejected as
    /// unconfigured instead of failing creation.
    #[must_use]
    pub fn with_external_tools(mut self, tools: crate::external_tools::ExternalTools) -> Self {
        Arc::get_mut(&mut self.inner)
            .expect("external tools are attached before the host is cloned")
            .external_tools = Some(tools);
        self
    }
    /// Attach an optional isolated credentials store file path (`credentials.toml`).
    ///
    /// By default no credentials file is read, keeping tests and unconfigured
    /// hosts isolated from `~/.tars/credentials.toml`.
    #[must_use]
    pub fn with_credentials_path(mut self, path: Option<PathBuf>) -> Self {
        let inner = Arc::get_mut(&mut self.inner)
            .expect("credentials path is attached before the host is cloned");
        inner.options.credentials_path = path.clone();
        inner
            .runtime_config
            .get_mut()
            .expect("runtime config lock")
            .credentials_path = path;
        self
    }
    /// Attach an optional user-level command permissions file path (`permissions.toml`).
    ///
    /// By default no user-level rules file is read, keeping tests and unconfigured
    /// hosts isolated from `~/.tars/permissions.toml`. Entry points pass
    /// `config.config_dir().map(|dir| dir.join("permissions.toml"))`.
    #[must_use]
    pub fn with_user_rules_path(mut self, path: Option<PathBuf>) -> Self {
        let inner = Arc::get_mut(&mut self.inner)
            .expect("user rules path is attached before the host is cloned");
        inner.options.user_rules_path = path.clone();
        inner
            .runtime_config
            .get_mut()
            .expect("runtime config lock")
            .user_rules_path = path;
        self
    }
    /// Attach authorized named model profiles selectable by child agents via `spawn_agent`.
    #[must_use]
    pub fn with_model_profiles(
        mut self,
        profiles: BTreeMap<String, crate::agents::ModelProfile>,
    ) -> Self {
        let inner = Arc::get_mut(&mut self.inner)
            .expect("model profiles are attached before the host is cloned");
        inner.options.model_profiles = profiles.clone();
        inner
            .runtime_config
            .get_mut()
            .expect("runtime config lock")
            .model_profiles = profiles;
        self
    }
    /// Update the runtime credentials path, user rules path, and child-agent
    /// model profiles in place without replacing the host's open session table.
    pub fn update_runtime_config(
        &self,
        credentials_path: Option<PathBuf>,
        user_rules_path: Option<PathBuf>,
        model_profiles: BTreeMap<String, crate::agents::ModelProfile>,
    ) {
        let mut config = self
            .inner
            .runtime_config
            .write()
            .expect("runtime config lock");
        config.credentials_path = credentials_path;
        config.user_rules_path = user_rules_path;
        config.model_profiles = model_profiles;
    }
    /// Construct with validated count limits; no directory or credential is read yet.
    pub fn with_options(
        store_root: Option<PathBuf>,
        options: HostOptions,
    ) -> Result<Self, ApplicationError> {
        if options.max_active_sessions == 0
            || options.max_storage_operations == 0
            || options.max_storage_operations > Semaphore::MAX_PERMITS
        {
            return Err(ApplicationError::InvalidOptions);
        }
        let runtime_config = HostRuntimeConfig {
            credentials_path: options.credentials_path.clone(),
            user_rules_path: options.user_rules_path.clone(),
            model_profiles: options.model_profiles.clone(),
        };
        Ok(Self {
            inner: Arc::new(HostInner {
                id: uuid::Uuid::now_v7(),
                entries: Mutex::new(HashMap::new()),
                store: store_root.map(SessionStore::new),
                io: Arc::new(Semaphore::new(options.max_storage_operations)),
                tool_resources: crate::tools::ToolResources::new(16),
                external_tools: None,
                options,
                runtime_config: RwLock::new(runtime_config),
                external_hosts: std::sync::atomic::AtomicUsize::new(1),
                snapshot_clones: Arc::new(std::sync::atomic::AtomicU64::new(0)),
                #[cfg(test)]
                assemble: Mutex::new(None),
                #[cfg(test)]
                gateway_override: Mutex::new(None),
                #[cfg(test)]
                agent_gateway_override: Mutex::new(None),
                #[cfg(test)]
                command_sandbox: Mutex::new(None),
            }),
        })
    }
    /// Register or join a fixed creation operation, then await initial confirmation.
    /// Cancellation after registration only abandons this caller's wait.
    pub async fn create(
        &self,
        prepared: PreparedCreate,
    ) -> Result<ManagedSession, ApplicationError> {
        prepared.definition.validate()?;
        if prepared.persistent && self.inner.store.is_none() {
            return Err(ApplicationError::StoreUnavailable);
        }
        let entry = self.register(prepared.conversation_id.clone(), Some(prepared))?;
        self.wait_ready(entry).await
    }
    /// Route the real assembly path through a scripted gateway during tests.
    #[cfg(test)]
    pub(crate) fn set_test_gateway(&self, gateway: Option<Arc<dyn causa::kernel::ModelGateway>>) {
        *self.inner.gateway_override.lock().expect("test gateway") = gateway;
    }

    /// Route per-agent assembly through a custom gateway factory during tests.
    #[cfg(test)]
    pub(crate) fn set_test_agent_gateway(&self, factory: Option<Arc<TestAgentGatewayFactory>>) {
        *self
            .inner
            .agent_gateway_override
            .lock()
            .expect("test agent gateway") = factory;
    }

    /// Override session assembly during tests to construct a fixture without permissions.
    #[cfg(test)]
    #[cfg_attr(not(any(target_os = "linux", target_os = "macos")), allow(dead_code))]
    pub(crate) fn set_test_assemble(&self, assemble: Option<Arc<TestAssembly>>) {
        *self.inner.assemble.lock().expect("test fixture") = assemble;
    }

    /// Bind the real assembly path's command runtime with an injected probe
    /// result instead of probing the platform backend during tests.
    #[cfg(test)]
    #[cfg_attr(not(unix), allow(dead_code))]
    pub(crate) fn set_test_command_sandbox(
        &self,
        sandbox: Option<Result<crate::sandbox::Sandbox, crate::sandbox::SandboxUnavailable>>,
    ) {
        *self
            .inner
            .command_sandbox
            .lock()
            .expect("test command sandbox") = sandbox;
    }

    /// Join an existing opening or load a saved record without replaying old work.
    pub async fn open(&self, id: ConversationId) -> Result<ManagedSession, ApplicationError> {
        let entry = self.register(id, None)?;
        self.wait_ready(entry).await
    }
    fn register(
        &self,
        id: ConversationId,
        prepared: Option<PreparedCreate>,
    ) -> Result<Arc<Entry>, ApplicationError> {
        let runtime = tokio::runtime::Handle::try_current()
            .map_err(|_| crate::session::SessionError::RuntimeUnavailable)?;
        let mut entries = self
            .inner
            .entries
            .lock()
            .expect("directory has no callbacks");
        if let Some(entry) = entries.get(&id) {
            if let Some(candidate) = prepared {
                let mut state = entry.lock();
                let matching = state.prepared.as_ref().is_some_and(|original| {
                    original.commit_id == candidate.commit_id
                        && original.definition == candidate.definition
                        && original.persistent == candidate.persistent
                        && original.context.blocks() == candidate.context.blocks()
                });
                if !matching {
                    return Err(ApplicationError::CreationConflict);
                }
                if state.lifecycle == Lifecycle::InitializationBlocked && !state.save_requested {
                    state.save_requested = true;
                    state.error = None;
                    state.lifecycle = Lifecycle::Opening;
                    entry.notify();
                }
            }
            return Ok(entry.clone());
        }
        if prepared.is_none() && self.inner.store.is_none() {
            return Err(ApplicationError::StoreUnavailable);
        }
        if entries.len() >= self.inner.options.max_active_sessions {
            return Err(ApplicationError::Capacity {
                capacity: self.inner.options.max_active_sessions,
            });
        }
        let target = SessionTarget {
            host: self.inner.id,
            generation: uuid::Uuid::now_v7(),
            conversation_id: id.clone(),
        };
        let entry = Arc::new(Entry::new(
            target,
            prepared,
            self.inner.snapshot_clones.clone(),
            self.inner.store.clone(),
            self.inner.io.clone(),
            self.inner.model_profiles(),
        ));
        // Registration and supervision are synchronous; no caller-owned await gap.
        entries.insert(id, entry.clone());
        let host = self.inner.clone();
        let task_entry = entry.clone();
        runtime.spawn(async move {
            supervisor::supervise(host, task_entry).await;
        });
        Ok(entry)
    }
    async fn wait_ready(&self, entry: Arc<Entry>) -> Result<ManagedSession, ApplicationError> {
        loop {
            let changed = entry.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            {
                let state = entry.lock();
                match state.lifecycle {
                    Lifecycle::Open => {
                        return Ok(ManagedSession {
                            entry: entry.clone(),
                        });
                    }
                    Lifecycle::Closing => return Err(ApplicationError::Closing),
                    Lifecycle::Closed => return Err(ApplicationError::Closed),
                    Lifecycle::InitializationBlocked | Lifecycle::Faulted | Lifecycle::Failed => {
                        return Err(state.error.clone().unwrap_or_else(|| {
                            ApplicationError::Faulted("opening failed".into())
                        }));
                    }
                    Lifecycle::Opening => {}
                }
            }
            changed.await;
        }
    }
    /// List complete final records and individual corruption/lock observations.
    /// This does not resolve credentials or construct sessions.
    pub async fn list(&self) -> Result<Vec<SessionListing>, ApplicationError> {
        let store = self
            .inner
            .store
            .clone()
            .ok_or(ApplicationError::StoreUnavailable)?;
        let listed = super::saving::io(&self.inner, move || store.list()).await?;
        Ok(listed
            .into_iter()
            .map(|item| {
                let id: ConversationId = item.conversation_id.into();
                let created_at_ms = crate::time::created_at_from_id(id.as_str());
                SessionListing {
                    local: self.lookup(&id),
                    conversation_id: id,
                    record: item
                        .record
                        .map(|record| SavedSessionSummary {
                            block_count: record.context.blocks().len(),
                            definition: record.definition,
                            revision: record.revision,
                            title: record.title,
                            created_at_ms,
                            updated_at_ms: record.updated_at_ms,
                        })
                        .map_err(Arc::new),
                    active: item.active.map_err(Arc::new),
                }
            })
            .collect())
    }

    /// Resolve a session selector (`<full-id>`, unique `<prefix>`, or `"latest"`)
    /// read-only against the persistent store without constructing sessions.
    pub async fn resolve_session_selector(
        &self,
        selector: &str,
    ) -> Result<ConversationId, SessionSelectorError> {
        if selector.trim().is_empty() {
            return Err(SessionSelectorError::EmptySelector);
        }
        let listings = self.list().await.map_err(|err| match err {
            ApplicationError::StoreUnavailable => SessionSelectorError::StoreUnavailable,
            ApplicationError::Store(store_err) => SessionSelectorError::Store(store_err),
            other => SessionSelectorError::Store(Arc::new(
                crate::store::StoreError::InvalidDatabase(other.to_string()),
            )),
        })?;
        resolve_session_selector_from_listings(selector, &listings)
    }

    /// Import a legacy `tars-session-v2.4` JSON record (plus its sibling
    /// `.history/*.json` and `.artifacts/*` files) into SQLite without modifying
    /// any source files.
    pub async fn import_legacy_record(
        &self,
        record_path: &std::path::Path,
    ) -> Result<crate::store::LegacyImportSummary, ApplicationError> {
        let store = self
            .inner
            .store
            .clone()
            .ok_or(ApplicationError::StoreUnavailable)?;
        let path = record_path.to_path_buf();
        let result = super::saving::io(&self.inner, move || store.import_legacy_v2_4(&path)).await;
        match result {
            Ok(summary) => Ok(summary),
            Err(ApplicationError::Store(err))
                if matches!(&*err, crate::store::StoreError::AlreadyExists(_)) =>
            {
                Err(ApplicationError::CreationConflict)
            }
            Err(other) => Err(other),
        }
    }

    /// Inspect one saved session record read-only without resolving credentials,
    /// locking, or mutating any Context, history, or artifact files.
    pub async fn inspect(
        &self,
        id: &ConversationId,
    ) -> Result<SavedSessionInspection, ApplicationError> {
        let store = self
            .inner
            .store
            .clone()
            .ok_or(ApplicationError::StoreUnavailable)?;
        let raw_id = id.as_str().to_owned();
        let created_at_ms = crate::time::created_at_from_id(id.as_str());
        let inspection = super::saving::io(&self.inner, move || {
            let Some(record) = store.read_saved(&raw_id)? else {
                return Ok(None);
            };
            let bytes = crate::compact::estimate::context_bytes(&record.context);
            let estimate_tokens = crate::compact::estimate::estimate_bytes(bytes);
            let context_usage = ContextUsage {
                estimate_tokens,
                context_window: record.definition.context_window,
                auto_compact: record
                    .auto_compact
                    .unwrap_or(record.definition.auto_compact),
                compact_threshold: record
                    .compact_threshold
                    .unwrap_or(record.definition.compact_threshold),
            };
            let work_capacity = WorkCapacity {
                used: 0,
                capacity: record.definition.retained_work_capacity,
            };
            let skills_count = if record.definition.prelude.skills_enabled {
                crate::skills::SkillDiscovery::discover(
                    &record.definition.prelude.skill_dirs,
                    record.definition.workspace_root.as_deref(),
                    record.definition.prelude.include_workspace_skills,
                )
                .registry
                .len()
            } else {
                0
            };
            let dynamic_context_file_exists = match &record.definition.prelude.dynamic_custom {
                Some(crate::prelude::DynamicCustomSlot::File(path)) => {
                    Some(std::fs::metadata(path).is_ok_and(|meta| meta.is_file()))
                }
                _ => None,
            };
            let usage = record.session_usage();
            let agent_rows = store.list_agents(&raw_id)?;
            let op_rows = store.list_operations(&raw_id)?;
            let mut agents = Vec::with_capacity(agent_rows.len());
            for agent in agent_rows {
                let context: causa::kernel::Context =
                    crate::material::decode_stored(agent.context_json.as_bytes())?;
                let history: Vec<crate::compact::HistoryRecordEntry> =
                    serde_json::from_str(&agent.history_index_json)
                        .map_err(crate::store::StoreError::from)?;
                let operation_count = op_rows
                    .iter()
                    .filter(|op| op.target_agent_id == agent.agent_id)
                    .count();
                let pending_events = store
                    .list_pending_inbox_events(&raw_id, &agent.agent_id)?
                    .len();
                agents.push(SavedAgentSummary {
                    is_entry: agent.agent_id == raw_id,
                    agent_id: agent.agent_id,
                    name: agent.name,
                    preset_name: agent.preset_name,
                    interaction: agent.interaction,
                    material_revision: agent.material_revision,
                    block_count: context.blocks().len(),
                    history_records: history.len(),
                    operation_count,
                    pending_events,
                    created_at_ms: agent.created_at_ms,
                    updated_at_ms: agent.updated_at_ms,
                });
            }
            Ok(Some(SavedSessionInspection {
                conversation_id: record.conversation_id.into(),
                definition: record.definition,
                revision: record.revision,
                block_count: record.context.blocks().len(),
                context_usage,
                work_capacity,
                history_records: record.history.len(),
                usage,
                title: record.title,
                created_at_ms,
                updated_at_ms: record.updated_at_ms,
                skills_count,
                dynamic_context_file_exists,
                agents,
            }))
        })
        .await?
        .ok_or(ApplicationError::NotFound)?;
        Ok(inspection)
    }

    /// Inspect one persisted agent in a saved session without resolving credentials,
    /// locking, or mutating any Context, history, or database files.
    ///
    /// `agent_selector` matches an exact `agent_id` first, then a unique `name`
    /// within the session (returning [`ApplicationError::AmbiguousAgent`] when multiple
    /// agents share that name, or [`ApplicationError::NotFound`] when absent).
    pub async fn inspect_agent(
        &self,
        id: &ConversationId,
        agent_selector: &str,
    ) -> Result<SavedAgentInspection, ApplicationError> {
        let store = self
            .inner
            .store
            .clone()
            .ok_or(ApplicationError::StoreUnavailable)?;
        let raw_id = id.as_str().to_owned();
        let selector = agent_selector.to_owned();
        super::saving::io_semaphore(
            &self.inner.io,
            move || -> Result<SavedAgentInspection, ApplicationError> {
                let Some(record) = store.read_saved(&raw_id)? else {
                    return Err(ApplicationError::NotFound);
                };
                let agent_rows = store.list_agents(&raw_id)?;
                let agent = resolve_agent_row(&agent_rows, &raw_id, Some(&selector))?;
                let is_entry = agent.agent_id == raw_id;
                let definition: crate::definition::SessionDefinition =
                    serde_json::from_str(&agent.definition_json)
                        .map_err(crate::store::StoreError::from)?;
                let context: causa::kernel::Context =
                    crate::material::decode_stored(agent.context_json.as_bytes())
                        .map_err(crate::store::StoreError::from)?;
                let history: Vec<crate::compact::HistoryRecordEntry> =
                    serde_json::from_str(&agent.history_index_json)
                        .map_err(crate::store::StoreError::from)?;
                let bytes = crate::compact::estimate::context_bytes(&context);
                let estimate_tokens = crate::compact::estimate::estimate_bytes(bytes);
                let context_usage = ContextUsage {
                    estimate_tokens,
                    context_window: definition.context_window,
                    auto_compact: if is_entry {
                        record.auto_compact.unwrap_or(definition.auto_compact)
                    } else {
                        definition.auto_compact
                    },
                    compact_threshold: if is_entry {
                        record
                            .compact_threshold
                            .unwrap_or(definition.compact_threshold)
                    } else {
                        definition.compact_threshold
                    },
                };
                let usage = match agent.usage_json.as_deref() {
                    Some(raw) => serde_json::from_str::<crate::store::WireSessionUsage>(raw)
                        .map_err(crate::store::StoreError::from)?
                        .to_session_usage(),
                    None if is_entry => record.session_usage(),
                    None => crate::usage::SessionUsage::default(),
                };
                let op_rows = store.list_operations(&raw_id)?;
                let operations = op_rows
                    .into_iter()
                    .filter(|op| op.target_agent_id == agent.agent_id)
                    .map(|op| SavedOperationSummary {
                        operation_id: op.operation_id,
                        parent_operation_id: op.parent_operation_id,
                        parent_agent_id: op.parent_agent_id,
                        execution_mode: op.execution_mode,
                        on_parent_complete: op.on_parent_complete,
                        depth: op.depth,
                        status: op.status,
                        result_bytes: op.result_bytes,
                        created_at_ms: op.created_at_ms,
                        finished_at_ms: op.finished_at_ms,
                    })
                    .collect();
                let pending_events = store
                    .list_pending_inbox_events(&raw_id, &agent.agent_id)?
                    .len();
                Ok(SavedAgentInspection {
                    conversation_id: record.conversation_id.into(),
                    agent_id: agent.agent_id.clone(),
                    name: agent.name.clone(),
                    preset_name: agent.preset_name.clone(),
                    interaction: agent.interaction,
                    is_entry,
                    definition,
                    material_revision: agent.material_revision,
                    block_count: context.blocks().len(),
                    context_usage,
                    history_records: history.len(),
                    usage,
                    operations,
                    pending_events,
                    created_at_ms: agent.created_at_ms,
                    updated_at_ms: agent.updated_at_ms,
                })
            },
        )
        .await
    }

    /// Expand the saved conversation history of one persisted agent without resolving
    /// credentials, acquiring the writer OS lock, or mutating any files.
    ///
    /// When `agent_selector` is `None`, the root entry agent (`id.as_str()`) is expanded.
    /// When `Some`, `agent_selector` matches an exact `agent_id` first, then a unique `name`.
    pub async fn read_saved_history(
        &self,
        id: &ConversationId,
        agent_selector: Option<&str>,
        last_n: Option<usize>,
    ) -> Result<crate::compact::ExpandedConversation, ApplicationError> {
        let store = self
            .inner
            .store
            .clone()
            .ok_or(ApplicationError::StoreUnavailable)?;
        let raw_id = id.as_str().to_owned();
        let selector = agent_selector.map(str::to_owned);
        super::saving::io_semaphore(
            &self.inner.io,
            move || -> Result<crate::compact::ExpandedConversation, ApplicationError> {
                if store.read_saved(&raw_id)?.is_none() {
                    return Err(ApplicationError::NotFound);
                }
                let agent_rows = store.list_agents(&raw_id)?;
                let agent = resolve_agent_row(&agent_rows, &raw_id, selector.as_deref())?;
                let context: causa::kernel::Context =
                    crate::material::decode_stored(agent.context_json.as_bytes())
                        .map_err(crate::store::StoreError::from)?;
                let history: Vec<crate::compact::HistoryRecordEntry> =
                    serde_json::from_str(&agent.history_index_json)
                        .map_err(crate::store::StoreError::from)?;
                let history_store = crate::compact::history::HistoryStore::open_read_only_for_agent(
                    store.root(),
                    &raw_id,
                    &agent.agent_id,
                );
                Ok(crate::compact::expand_conversation(
                    context.blocks(),
                    &history,
                    last_n,
                    |record_id| history_store.read(record_id),
                ))
            },
        )
        .await
    }
    /// Rename (set or clear) a session's title (`WF01`).
    ///
    /// If `id` is currently open in this host, delegates to [`ManagedSession::rename`]
    /// and awaits settlement if idle or saving. Otherwise updates the store record
    /// atomically under its OS advisory lock.
    pub async fn rename(
        &self,
        id: ConversationId,
        title: Option<String>,
    ) -> Result<Option<String>, ApplicationError> {
        let validated = crate::store::validate_session_title(title.as_deref())
            .map_err(map_workflow_store_error)?;
        let open_entry = {
            let entries = self
                .inner
                .entries
                .lock()
                .expect("directory has no callbacks");
            entries.get(&id).cloned()
        };
        if let Some(entry) = open_entry {
            let managed = ManagedSession {
                entry: entry.clone(),
            };
            let updated = managed.rename(validated)?;
            let should_wait = {
                let state = entry.lock();
                state.persistent
                    && state.active.is_none()
                    && !state.compacting
                    && (state.save_requested || state.save_in_flight)
            };
            if should_wait {
                managed.wait_saved().await?;
            }
            return Ok(updated);
        }
        let store = self
            .inner
            .store
            .clone()
            .ok_or(ApplicationError::StoreUnavailable)?;
        let raw_id = id.as_str().to_owned();
        let record = super::saving::io_semaphore(&self.inner.io, move || {
            store
                .rename_session(&raw_id, validated.as_deref())
                .map_err(map_workflow_store_error)
        })
        .await?;
        Ok(record.title)
    }

    /// Permanently delete an inactive saved session (`WF02`).
    ///
    /// Refuses with [`ApplicationError::SessionActive`] if `id` is currently open
    /// in this host, or [`ApplicationError::SessionLocked`] if another process holds
    /// its OS advisory lock. Preserves `workspace_trust`.
    pub async fn delete(&self, id: ConversationId) -> Result<(), ApplicationError> {
        {
            let entries = self
                .inner
                .entries
                .lock()
                .expect("directory has no callbacks");
            if entries.contains_key(&id) {
                return Err(ApplicationError::SessionActive(id.as_str().to_owned()));
            }
        }
        let store = self
            .inner
            .store
            .clone()
            .ok_or(ApplicationError::StoreUnavailable)?;
        let raw_id = id.as_str().to_owned();
        super::saving::io_semaphore(&self.inner.io, move || {
            store
                .delete_session(&raw_id)
                .map_err(map_workflow_store_error)
        })
        .await
    }

    /// Return whether `workspace_root` has been trusted at the folder level.
    ///
    /// Returns `Ok(false)` when this host has no persistent store (`store = None`),
    /// when `<store_root>/tars.db` does not exist yet, when the database is still on
    /// schema v1, or when `workspace_root` does not exist on disk.
    pub async fn is_workspace_folder_trusted(
        &self,
        workspace_root: &Path,
    ) -> Result<bool, ApplicationError> {
        let Some(store) = self.inner.store.clone() else {
            return Ok(false);
        };
        let path = workspace_root.to_path_buf();
        super::saving::io_semaphore(&self.inner.io, move || {
            store
                .is_workspace_folder_trusted(&path)
                .map_err(map_workflow_store_error)
        })
        .await
    }

    /// Persist folder-level trust for `workspace_root`.
    ///
    /// Canonicalizes `workspace_root`, verifies it is a directory, and stores its
    /// hash and canonical path in `trusted_workspaces`. Does not trust `.tars/`
    /// rules or agent definitions. Fails with [`ApplicationError::StoreUnavailable`]
    /// when this host has no persistent store.
    pub async fn trust_workspace_folder(
        &self,
        workspace_root: &Path,
    ) -> Result<(), ApplicationError> {
        let store = self
            .inner
            .store
            .clone()
            .ok_or(ApplicationError::StoreUnavailable)?;
        let path = workspace_root.to_path_buf();
        super::saving::io_semaphore(&self.inner.io, move || {
            store
                .trust_workspace_folder(&path)
                .map_err(map_workflow_store_error)
        })
        .await
    }

    /// List all persisted trusted workspace folders ordered by most recently trusted first.
    ///
    /// Returns an empty list when this host has no persistent store (`store = None`),
    /// when `<store_root>/tars.db` does not exist yet, or when the database is on schema v1.
    pub async fn list_trusted_workspace_folders(
        &self,
    ) -> Result<Vec<super::TrustedWorkspaceFolder>, ApplicationError> {
        let Some(store) = self.inner.store.clone() else {
            return Ok(Vec::new());
        };
        super::saving::io_semaphore(&self.inner.io, move || {
            store
                .list_trusted_workspace_folders()
                .map_err(map_workflow_store_error)
        })
        .await
    }

    /// Revoke folder-level trust for `workspace_root`, returning whether a record was removed.
    ///
    /// Never creates `<store_root>/tars.db` or upgrades a schema v1 database, and never
    /// touches `sessions` or `workspace_trust`. Fails with
    /// [`ApplicationError::StoreUnavailable`] when this host has no persistent store.
    pub async fn revoke_workspace_folder_trust(
        &self,
        workspace_root: &Path,
    ) -> Result<bool, ApplicationError> {
        let store = self
            .inner
            .store
            .clone()
            .ok_or(ApplicationError::StoreUnavailable)?;
        let path = workspace_root.to_path_buf();
        super::saving::io_semaphore(&self.inner.io, move || {
            store
                .revoke_workspace_folder_trust(&path)
                .map_err(map_workflow_store_error)
        })
        .await
    }

    /// Fork a cleanly idle, saved session into a physically isolated new session (`WF03`).
    ///
    /// When `source_id` is open in this host, refuses if it is busy, compacting,
    /// transient, save-blocked, has unsaved changes/renames, active child operations,
    /// or unpaired declarations needing repair. When `source_id` is not open in this
    /// host, fails closed with [`ApplicationError::SessionLocked`] if another process
    /// holds its OS advisory lock.
    pub async fn fork(
        &self,
        source_id: ConversationId,
        title: Option<String>,
    ) -> Result<ConversationId, ApplicationError> {
        let validated_title = crate::store::validate_session_title(title.as_deref())
            .map_err(map_workflow_store_error)?;
        let store = self
            .inner
            .store
            .clone()
            .ok_or(ApplicationError::StoreUnavailable)?;
        let open_entry = {
            let entries = self
                .inner
                .entries
                .lock()
                .expect("directory has no callbacks");
            entries.get(&source_id).cloned()
        };
        let (_writer_guard, _fork_guard, require_source_unlocked) = if let Some(entry) = open_entry
        {
            let writer_guard = entry.writer_gate.clone().try_lock_owned().map_err(|_| {
                ApplicationError::ForkNotReady("session writer is currently busy".into())
            })?;
            let (context, history, artifacts) = {
                let mut state = entry.lock();
                verify_fork_ready(&entry, &mut state, &source_id)?;
                let owner = state.owner.as_mut().ok_or(ApplicationError::InstanceGone)?;
                let history = owner.history_entries();
                let artifacts = owner.compaction().and_then(|c| c.history());
                let context = owner.with_context(|ctx| ctx.clone())?;
                (context, history, artifacts)
            };
            super::repair::admission_check_with_media(
                &context,
                &history,
                artifacts.as_deref(),
                Some(&entry.media_vault),
            )
            .map_err(|err| ApplicationError::ForkNotReady(err.to_string()))?;
            let fork_guard = {
                let mut state = entry.lock();
                verify_fork_ready(&entry, &mut state, &source_id)?;
                state.forking = state
                    .forking
                    .checked_add(1)
                    .ok_or_else(|| ApplicationError::Faulted("fork counter overflow".into()))?;
                ForkGuard {
                    entry: entry.clone(),
                }
            };
            (Some(writer_guard), Some(fork_guard), false)
        } else {
            (None, None, true)
        };

        let new_id = ConversationId::from(uuid::Uuid::now_v7().to_string());
        let src = source_id.as_str().to_owned();
        let dst = new_id.as_str().to_owned();
        super::saving::io_semaphore(&self.inner.io, move || {
            store
                .fork_session_with_lock_check(
                    &src,
                    &dst,
                    validated_title.as_deref(),
                    require_source_unlocked,
                )
                .map_err(map_workflow_store_error)
        })
        .await?;
        Ok(new_id)
    }

    /// Export a session's detached Context and any referenced `media:sha256:*`
    /// sidecar blobs without modifying the source session.
    ///
    /// Works for both an open idle session in this host and a saved closed
    /// session in the store. Fails closed if any referenced media blob is
    /// missing or corrupt.
    pub async fn export_session_materials(
        &self,
        id: &ConversationId,
        path: impl AsRef<std::path::Path>,
    ) -> Result<crate::material::ExportedMaterials, ApplicationError> {
        let output_path = path.as_ref().to_path_buf();
        let open_entry = {
            let entries = self
                .inner
                .entries
                .lock()
                .expect("directory has no callbacks");
            entries.get(id).cloned()
        };
        if let Some(entry) = open_entry {
            let managed = ManagedSession { entry };
            let snapshot = managed.read_materials()?;
            let detached = snapshot.detached_context();
            let refs = crate::media::collect_context_managed_media_refs(&detached);
            let blobs = managed.media_vault().snapshot_blobs_for_refs(&refs)?;
            let blob_map: std::collections::BTreeMap<String, Vec<u8>> = blobs
                .into_iter()
                .map(|(reference, bytes)| (reference, bytes.to_vec()))
                .collect();
            return super::saving::io_semaphore(&self.inner.io, move || {
                Ok::<_, ApplicationError>(crate::material::save_new_with_media(
                    &output_path,
                    &detached,
                    &blob_map,
                )?)
            })
            .await;
        }

        let store = self
            .inner
            .store
            .clone()
            .ok_or(ApplicationError::StoreUnavailable)?;
        let raw_id = id.as_str().to_owned();
        super::saving::io_semaphore(&self.inner.io, move || {
            let Some(record) = store.read_saved(&raw_id)? else {
                return Err(ApplicationError::NotFound);
            };
            let detached =
                crate::material::detach_history_references(&record.context, &record.history);
            let refs = crate::media::collect_context_managed_media_refs(&detached);
            let blobs = store.load_managed_media_blobs(&raw_id, &record)?;
            let blob_map: std::collections::BTreeMap<String, Vec<u8>> = blobs
                .into_iter()
                .filter(|(reference, _)| refs.contains(reference))
                .collect();
            Ok::<_, ApplicationError>(crate::material::save_new_with_media(
                &output_path,
                &detached,
                &blob_map,
            )?)
        })
        .await
    }

    /// Locate an opening or active instance without returning mutable access.
    pub fn lookup(&self, id: &ConversationId) -> Option<SessionStatus> {
        self.inner
            .entries
            .lock()
            .expect("directory has no callbacks")
            .get(id)
            .map(|entry| entry.status())
    }
    /// Claim the original owned uncommitted batch atomically, once per retained work.
    /// The caller is responsible for deciding who is authorized to invoke this method.
    pub fn take_uncommitted_tool_batch(
        &self,
        target: &SessionTarget,
        work: &WorkRef,
    ) -> Result<Option<ToolBatch>, ApplicationError> {
        let entries = self
            .inner
            .entries
            .lock()
            .expect("directory has no callbacks");
        let entry = self.inner.resolve(&entries, target)?;
        let mut state = entry.lock();
        if state.lifecycle == Lifecycle::Closed {
            return Err(ApplicationError::InstanceGone);
        }
        state
            .owner
            .as_mut()
            .ok_or(ApplicationError::Saving)?
            .take_uncommitted_tool_batch(work)
            .map_err(Into::into)
    }
    /// Stop new admission and supervise cancellation, persistence and lock release.
    /// A dropped wait does not abandon the close. Same-policy concurrent calls join.
    pub async fn close(
        &self,
        target: &SessionTarget,
        policy: ClosePolicy,
    ) -> Result<CloseOutcome, ApplicationError> {
        let entry = {
            let entries = self
                .inner
                .entries
                .lock()
                .expect("directory has no callbacks");
            let entry = self.inner.resolve(&entries, target)?.clone();
            {
                let mut state = entry.lock();
                if state.lifecycle == Lifecycle::Closed {
                    return Ok(state.close_outcome.clone().unwrap_or_else(|| CloseOutcome {
                        policy,
                        save: state.save.clone(),
                        pending_commit: None,
                        discarded_unconfirmed: !matches!(
                            state.save,
                            SaveStatus::Saved { .. } | SaveStatus::NotTracked
                        ),
                    }));
                }
                if state.lifecycle == Lifecycle::Failed {
                    return Err(state
                        .error
                        .clone()
                        .unwrap_or(ApplicationError::InstanceGone));
                }
                if state.closing.is_some_and(|current| current != policy) && state.close_in_flight {
                    return Err(ApplicationError::ClosePolicyConflict);
                }
                state.closing = Some(policy);
                state.close_in_flight = true;
                state.lifecycle = Lifecycle::Closing;
                state.error = None;
                if let Some(handle) = &state.handle {
                    handle.begin_shutdown();
                }
                if policy == ClosePolicy::Save && state.pending.is_some() {
                    state.save_requested = true;
                }
                entry.notify();
            }
            if let Some(coord) = entry.coordinator.get() {
                coord.begin_session_shutdown();
            }
            entry
        };
        loop {
            let changed = entry.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            {
                let state = entry.lock();
                if state.lifecycle == Lifecycle::Closed {
                    return Ok(state.close_outcome.clone().unwrap_or_else(|| CloseOutcome {
                        policy,
                        save: state.save.clone(),
                        pending_commit: None,
                        discarded_unconfirmed: !matches!(
                            state.save,
                            SaveStatus::Saved { .. } | SaveStatus::NotTracked
                        ),
                    }));
                }
                if !state.close_in_flight
                    && let Some(error) = &state.error
                {
                    return Err(error.clone());
                }
            }
            changed.await;
        }
    }
}
impl HostInner {
    pub fn resolve<'a>(
        &self,
        entries: &'a HashMap<ConversationId, Arc<Entry>>,
        target: &SessionTarget,
    ) -> Result<&'a Arc<Entry>, ApplicationError> {
        if target.host != self.id {
            return Err(ApplicationError::StaleInstance);
        }
        let entry = entries
            .get(&target.conversation_id)
            .ok_or(ApplicationError::InstanceGone)?;
        if entry.target != *target {
            return Err(ApplicationError::StaleInstance);
        }
        Ok(entry)
    }
}

impl Clone for SessionHost {
    fn clone(&self) -> Self {
        self.inner
            .external_hosts
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Self {
            inner: self.inner.clone(),
        }
    }
}
impl Drop for SessionHost {
    fn drop(&mut self) {
        if self
            .inner
            .external_hosts
            .fetch_sub(1, std::sync::atomic::Ordering::AcqRel)
            != 1
        {
            return;
        }
        // Application-owner disposal requests supervised cleanup; unlike view
        // disposal, no external authority remains to explicitly close these slots.
        let entries = self
            .inner
            .entries
            .lock()
            .expect("directory has no callbacks");
        for entry in entries.values() {
            let mut state = entry.lock();
            if matches!(state.lifecycle, Lifecycle::Closed | Lifecycle::Failed) {
                continue;
            }
            state.closing = Some(ClosePolicy::DiscardLocal);
            state.close_in_flight = true;
            state.lifecycle = Lifecycle::Closing;
            if let Some(handle) = &state.handle {
                handle.begin_shutdown();
            }
            entry.notify();
            drop(state);
            if let Some(coord) = entry.coordinator.get() {
                coord.begin_session_shutdown();
            }
        }
    }
}

struct ForkGuard {
    entry: Arc<Entry>,
}

impl Drop for ForkGuard {
    fn drop(&mut self) {
        let mut state = self.entry.lock();
        state.forking = state.forking.saturating_sub(1);
        drop(state);
        self.entry.notify();
    }
}

fn verify_fork_ready(
    entry: &Entry,
    state: &mut super::entry::State,
    source_id: &ConversationId,
) -> Result<(), ApplicationError> {
    if state.lifecycle != Lifecycle::Open {
        return Err(match state.lifecycle {
            Lifecycle::Closing => ApplicationError::Closing,
            Lifecycle::Closed => ApplicationError::Closed,
            Lifecycle::Faulted => state
                .fault
                .clone()
                .or_else(|| state.error.clone())
                .unwrap_or_else(|| ApplicationError::Faulted("session is faulted".into())),
            _ => ApplicationError::ForkNotReady(format!(
                "session {} is not open",
                source_id.as_str()
            )),
        });
    }
    if state.closing.is_some() {
        return Err(ApplicationError::Closing);
    }
    if !state.persistent {
        return Err(ApplicationError::ForkNotReady(
            "transient sessions cannot be forked".into(),
        ));
    }
    if let SaveStatus::SaveBlocked { error, .. } = &state.save {
        return Err(ApplicationError::SaveBlocked(error.clone()));
    }
    if let Some(coord) = entry.coordinator.get()
        && let Some(err) = coord.save_blocked_error()
    {
        return Err(ApplicationError::SaveBlocked(err));
    }
    if state.active.is_some() {
        return Err(ApplicationError::ForkNotReady(
            "session has active work in progress".into(),
        ));
    }
    if state.compacting {
        return Err(ApplicationError::ForkNotReady(
            "session is currently compacting".into(),
        ));
    }
    if state.forking > 0 {
        return Err(ApplicationError::ForkNotReady(
            "session is currently being forked".into(),
        ));
    }
    if state.pending.is_some()
        || state.save_in_flight
        || state.save_requested
        || !matches!(state.save, SaveStatus::Saved { .. })
    {
        return Err(ApplicationError::ForkNotReady(
            "session has unsaved changes or a save in flight".into(),
        ));
    }
    if state.has_uncommitted_metadata_or_settings() {
        return Err(ApplicationError::ForkNotReady(
            "session has uncommitted title or settings changes".into(),
        ));
    }
    if let Some(coord) = entry.coordinator.get()
        && (coord.has_pending_child_saves() || coord.has_active_execution_with_entry(state))
    {
        return Err(ApplicationError::ForkNotReady(
            "session has active child operations or pending child saves".into(),
        ));
    }
    Ok(())
}

pub(super) fn map_workflow_store_error(err: crate::store::StoreError) -> ApplicationError {
    match err {
        crate::store::StoreError::NotFound(_) => ApplicationError::NotFound,
        crate::store::StoreError::Occupied(id) => ApplicationError::SessionLocked(id),
        crate::store::StoreError::InvalidTitle(msg) => ApplicationError::InvalidTitle(msg),
        other => ApplicationError::Store(Arc::new(other)),
    }
}

fn resolve_agent_row<'a>(
    agents: &'a [crate::store::StoredAgentRow],
    entry_agent_id: &str,
    selector: Option<&str>,
) -> Result<&'a crate::store::StoredAgentRow, ApplicationError> {
    let Some(selector) = selector else {
        return agents
            .iter()
            .find(|agent| agent.agent_id == entry_agent_id)
            .ok_or(ApplicationError::NotFound);
    };
    if let Some(exact) = agents.iter().find(|agent| agent.agent_id == selector) {
        return Ok(exact);
    }
    let named: Vec<&crate::store::StoredAgentRow> = agents
        .iter()
        .filter(|agent| agent.name == selector)
        .collect();
    match named.as_slice() {
        [] => Err(ApplicationError::NotFound),
        [single] => Ok(single),
        many => Err(ApplicationError::AmbiguousAgent {
            name: selector.to_owned(),
            candidates: many.iter().map(|agent| agent.agent_id.clone()).collect(),
        }),
    }
}
