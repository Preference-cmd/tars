use super::coordinator::SessionCoordinator;
use super::types::*;
use crate::agents::{AgentPreset, AgentRegistry, ModelProfile, WorkspaceAgentsSnapshot};
use crate::definition::SessionDefinition;
use crate::events::ConsumedInboxEvents;
use crate::session::{Session, SessionHandle, WorkRef};
use crate::store::{PreparedCommit, SessionStore, Writer};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, RwLock};
use tokio::sync::{Notify, Semaphore};

#[derive(Clone, Debug)]
pub(crate) struct WorkspaceRulesSnapshot {
    pub workspace_root: PathBuf,
    pub raw_text: String,
    pub rules: Vec<crate::permission::CommandRule>,
}

pub(crate) struct Entry {
    pub target: SessionTarget,
    pub state: Mutex<State>,
    pub changed: Notify,
    pub wake: Notify,
    pub store: Option<SessionStore>,
    pub io: Arc<Semaphore>,
    pub writer_gate: Arc<tokio::sync::Mutex<()>>,
    pub coordinator: OnceLock<Arc<SessionCoordinator>>,
    pub agent_registry: Arc<RwLock<AgentRegistry>>,
    pub model_profiles: Arc<BTreeMap<String, ModelProfile>>,
    #[cfg(test)]
    pub panic_owner: Notify,
    #[cfg(test)]
    pub before_close: Mutex<Option<(Arc<Notify>, Arc<Notify>)>>,
    pub snapshot_clones: Arc<std::sync::atomic::AtomicU64>,
}
pub(crate) struct State {
    pub prepared: Option<PreparedCreate>,
    pub definition: Option<SessionDefinition>,
    pub persistent: bool,
    pub owner: Option<Session>,
    pub permissions: Option<Arc<crate::permission::PermissionHub>>,
    pub handle: Option<SessionHandle>,
    pub writer: Option<Arc<Writer>>,
    pub revision: Option<u64>,
    pub generation: u64,
    pub lifecycle: Lifecycle,
    pub save: SaveStatus,
    pub error: Option<ApplicationError>,
    pub fault: Option<ApplicationError>,
    pub active: Option<WorkRef>,
    /// A manual compaction is between its material snapshot and its edit.
    pub compacting: bool,
    pub pending: Option<Arc<PreparedCommit>>,
    pub pending_consumed: ConsumedInboxEvents,
    pub save_requested: bool,
    pub pending_attempted: bool,
    pub initialized: bool,
    pub closing: Option<ClosePolicy>,
    pub close_in_flight: bool,
    pub close_outcome: Option<CloseOutcome>,
    pub open_report: OpenReport,
    pub workspace_rules_snapshot: Option<WorkspaceRulesSnapshot>,
    pub workspace_agents_snapshot: Option<WorkspaceAgentsSnapshot>,
    pub trusted_workspace_agents: Vec<AgentPreset>,
    /// Title and last-activity time of the last stored record.
    pub saved: crate::store::RecordMetadata,
    /// Non-fatal prelude diagnostics recorded during creation or last submit.
    pub prelude_diagnostics: Vec<crate::prelude::PreludeDiagnostic>,
}
impl Entry {
    pub fn new(
        target: SessionTarget,
        prepared: Option<PreparedCreate>,
        snapshot_clones: Arc<std::sync::atomic::AtomicU64>,
        store: Option<SessionStore>,
        io: Arc<Semaphore>,
        model_profiles: BTreeMap<String, ModelProfile>,
    ) -> Self {
        let persistent = prepared.as_ref().is_none_or(|p| p.persistent);
        let version = MaterialVersion {
            target: target.clone(),
            generation: 0,
        };
        let saved = crate::store::RecordMetadata::unsaved(target.conversation_id.as_str());
        let prelude_diagnostics = prepared
            .as_ref()
            .map(|p| p.prelude_diagnostics.clone())
            .unwrap_or_default();
        Self {
            target,
            store,
            io,
            writer_gate: Arc::new(tokio::sync::Mutex::new(())),
            coordinator: OnceLock::new(),
            agent_registry: Arc::new(RwLock::new(AgentRegistry::new())),
            model_profiles: Arc::new(model_profiles),
            snapshot_clones,
            state: Mutex::new(State {
                definition: prepared.as_ref().map(|p| p.definition.clone()),
                prepared,
                persistent,
                owner: None,
                permissions: None,
                handle: None,
                writer: None,
                revision: None,
                generation: 0,
                lifecycle: Lifecycle::Opening,
                save: if persistent {
                    SaveStatus::Dirty {
                        base_revision: None,
                        material_version: version,
                    }
                } else {
                    SaveStatus::NotTracked
                },
                error: None,
                fault: None,
                active: None,
                compacting: false,
                pending: None,
                pending_consumed: ConsumedInboxEvents::default(),
                save_requested: false,
                pending_attempted: false,
                initialized: false,
                closing: None,
                close_in_flight: false,
                close_outcome: None,
                open_report: OpenReport::default(),
                workspace_rules_snapshot: None,
                workspace_agents_snapshot: None,
                trusted_workspace_agents: Vec::new(),
                saved,
                prelude_diagnostics,
            }),
            changed: Notify::new(),
            wake: Notify::new(),
            #[cfg(test)]
            panic_owner: Notify::new(),
            #[cfg(test)]
            before_close: Mutex::new(None),
        }
    }
    pub fn lock(&self) -> MutexGuard<'_, State> {
        match self.state.lock() {
            Ok(state) => state,
            Err(poisoned) => {
                let mut state = poisoned.into_inner();
                self.state.clear_poison();
                state.lifecycle = Lifecycle::Faulted;
                state.error = Some(ApplicationError::Faulted(
                    "owner coordination panicked".into(),
                ));
                state.fault = state.error.clone();
                state
            }
        }
    }
    pub fn version(&self, state: &State) -> MaterialVersion {
        MaterialVersion {
            target: self.target.clone(),
            generation: state.generation,
        }
    }
    pub fn status(&self) -> SessionStatus {
        let state = self.lock();
        SessionStatus {
            target: self.target.clone(),
            lifecycle: state.lifecycle,
            save: state.save.clone(),
            active_work: state.active.clone(),
            error: state.error.clone(),
        }
    }
    pub fn cloned_snapshot(&self) {
        self.snapshot_clones
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }
    pub fn changed(&self) {
        self.changed.notify_waiters();
    }
    pub fn notify(&self) {
        self.wake.notify_one();
        self.changed();
    }
    pub fn new_mutation(&self, state: &State) -> Result<(), ApplicationError> {
        match state.lifecycle {
            Lifecycle::Closing => return Err(ApplicationError::Closing),
            Lifecycle::Closed => return Err(ApplicationError::Closed),
            Lifecycle::Faulted => {
                return Err(state
                    .error
                    .clone()
                    .unwrap_or_else(|| ApplicationError::Faulted("worker lost materials".into())));
            }
            Lifecycle::Failed => {
                return Err(state
                    .error
                    .clone()
                    .unwrap_or(ApplicationError::InstanceGone));
            }
            Lifecycle::Opening | Lifecycle::InitializationBlocked => {
                return Err(ApplicationError::Saving);
            }
            Lifecycle::Open => {}
        }
        match &state.save {
            SaveStatus::SaveBlocked { error, .. } => {
                return Err(ApplicationError::SaveBlocked(error.clone()));
            }
            SaveStatus::Saving { .. } | SaveStatus::Dirty { .. } if state.active.is_none() => {
                return Err(ApplicationError::Saving);
            }
            _ => {}
        }
        if state.compacting {
            return Err(ApplicationError::Compacting);
        }
        if let Some(work) = &state.active {
            // The application barrier deliberately survives low Finished.
            let handle = state.handle.as_ref().expect("active owner");
            return match handle.observe(work)?.state {
                crate::session::WorkState::Finished(_) => Err(ApplicationError::Saving),
                _ => Err(crate::session::SessionError::Busy { work: work.clone() }.into()),
            };
        }
        Ok(())
    }
}
