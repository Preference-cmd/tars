use super::entry::Entry;
use super::managed::ManagedSession;
use super::supervisor;
use super::types::*;
use crate::session::{ConversationId, WorkRef};
use crate::store::SessionStore;
use causa::kernel::ToolBatch;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use tokio::sync::Semaphore;

/// Shared lifecycle owner. Dropping a view or waiter never closes a session.
pub struct SessionHost {
    pub(super) inner: Arc<HostInner>,
}
pub(super) struct HostInner {
    pub id: uuid::Uuid,
    pub entries: Mutex<HashMap<ConversationId, Arc<Entry>>>,
    pub store: Option<SessionStore>,
    pub options: HostOptions,
    pub io: Arc<Semaphore>,
    pub tool_resources: Arc<crate::tools::ToolResources>,
    pub snapshot_clones: Arc<std::sync::atomic::AtomicU64>,
    pub external_hosts: std::sync::atomic::AtomicUsize,
    #[cfg(test)]
    pub assemble: Mutex<Option<Arc<TestAssembly>>>,
    /// Test-only model gateway injection for the real assembly path.
    #[cfg(test)]
    pub gateway_override: Mutex<Option<Arc<dyn causa::kernel::ModelGateway>>>,
}
#[cfg(test)]
pub(super) type TestAssembly = dyn Fn(
        &crate::definition::SessionDefinition,
        ConversationId,
        causa::kernel::Context,
    ) -> Result<crate::session::Session, ApplicationError>
    + Send
    + Sync;

impl SessionHost {
    /// Construct a host with explicit optional local storage and default bounds.
    pub fn new(store_root: Option<PathBuf>) -> Self {
        Self::with_options(store_root, HostOptions::default()).expect("positive defaults")
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
        Ok(Self {
            inner: Arc::new(HostInner {
                id: uuid::Uuid::now_v7(),
                entries: Mutex::new(HashMap::new()),
                store: store_root.map(SessionStore::new),
                io: Arc::new(Semaphore::new(options.max_storage_operations)),
                tool_resources: crate::tools::ToolResources::new(16),
                options,
                external_hosts: std::sync::atomic::AtomicUsize::new(1),
                snapshot_clones: Arc::new(std::sync::atomic::AtomicU64::new(0)),
                #[cfg(test)]
                assemble: Mutex::new(None),
                #[cfg(test)]
                gateway_override: Mutex::new(None),
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
                SessionListing {
                    local: self.lookup(&id),
                    conversation_id: id,
                    record: item
                        .record
                        .map(|record| SavedSessionSummary {
                            block_count: record.context.blocks().len(),
                            definition: record.definition,
                            revision: record.revision,
                        })
                        .map_err(Arc::new),
                    active: item.active.map_err(Arc::new),
                }
            })
            .collect())
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
        }
    }
}
