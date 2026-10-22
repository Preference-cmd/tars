use super::handle::SessionHandle;
use super::state::{Shared, State};
use super::types::*;
use causa::kernel::{CallControl, Context, ToolBatch, TurnId};
use causa::runtime::{TurnRunOptions, TurnRunner};
use std::collections::HashMap;
use std::error::Error;
use std::future::Future;
use std::sync::{Arc, Mutex};

/// Exclusive lifecycle owner for one conversation's current materials.
pub struct Session {
    pub(super) shared: Arc<Shared>,
    standard_builtin_repair: bool,
    pub(crate) permissions: Option<Arc<crate::permission::PermissionHub>>,
    mcp: Option<Arc<crate::mcp::McpRuntime>>,
    #[cfg(test)]
    pub(crate) workspace_tools: Option<Arc<crate::tools::WorkspaceTools>>,
    #[cfg(test)]
    pub(crate) test_before_processors:
        Arc<std::sync::Mutex<Vec<Arc<dyn causa::kernel::ToolBatchProcessor>>>>,
}

impl Session {
    pub(crate) fn with_permissions(
        mut self,
        permissions: Arc<crate::permission::PermissionHub>,
    ) -> Self {
        self.permissions = Some(permissions);
        self
    }

    pub(crate) fn with_mcp(mut self, mcp: Arc<crate::mcp::McpRuntime>) -> Self {
        self.mcp = Some(mcp);
        self
    }

    /// Release the instance's MCP connections so the close path can await them
    /// without holding the application lock.
    pub(crate) fn take_mcp(&mut self) -> Option<Arc<crate::mcp::McpRuntime>> {
        self.mcp.take()
    }

    /// Snapshot of this instance's MCP servers, or an empty list without MCP.
    pub(crate) fn mcp_status(&self) -> Vec<crate::McpServerStatus> {
        self.mcp.as_ref().map_or_else(Vec::new, |mcp| mcp.status())
    }

    /// Read the instance's active permission mode and revision.
    ///
    /// Returns `None` for instances constructed without permission state (such as test fixtures).
    pub fn permission_mode(&self) -> Option<(crate::permission::PermissionMode, u64)> {
        self.permissions.as_ref().map(|p| p.get())
    }

    /// Change the instance's active permission mode, incrementing revision on actual change.
    ///
    /// Returns an error for instances constructed without permission state (such as test fixtures).
    pub fn set_permission_mode(
        &self,
        mode: crate::permission::PermissionMode,
    ) -> Result<(crate::permission::PermissionMode, u64), crate::application::ApplicationError>
    {
        self.permissions
            .as_ref()
            .map(|p| Ok(p.set(mode)))
            .unwrap_or(Err(
                crate::application::ApplicationError::PermissionUnavailable,
            ))
    }

    /// Connect the instance's approval channel receiver.
    ///
    /// At most one receiver is connected at any time; calling this again replaces
    /// any previously connected receiver. For instances constructed without permission
    /// state (such as test fixtures), this returns an immediately closed receiver.
    pub fn connect_approvals(&self) -> crate::permission::ApprovalReceiver {
        self.permissions.as_ref().map_or_else(
            || {
                let (_, rx) = tokio::sync::mpsc::channel(1);
                rx
            },
            |p| p.connect_approvals(),
        )
    }

    /// Append an extra before processor after the permission processor during test runs.
    #[cfg(test)]
    pub(crate) fn add_test_before_processor(
        &self,
        processor: Arc<dyn causa::kernel::ToolBatchProcessor>,
    ) {
        self.test_before_processors
            .lock()
            .expect("test processors lock")
            .push(processor);
    }
    /// Construct without assembly, worker creation, input edits or history recovery.
    pub fn new<F, Fut>(
        conversation_id: ConversationId,
        context: Context,
        assemble: F,
        config: SessionConfig,
    ) -> Self
    where
        F: Fn(WorkRef, CallControl) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<(Arc<TurnRunner>, TurnRunOptions), Box<dyn Error + Send + Sync>>>
            + Send
            + 'static,
    {
        Self::new_with_tools(
            conversation_id,
            context,
            move |work, control, _| assemble(work, control),
            config,
        )
    }

    /// Construct an owner whose per-work assembly receives tool observation and
    /// resource supervision tied to that accepted work.
    pub fn new_with_tools<F, Fut>(
        conversation_id: ConversationId,
        context: Context,
        assemble: F,
        config: SessionConfig,
    ) -> Self
    where
        F: Fn(WorkRef, CallControl, super::ToolReporter) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<(Arc<TurnRunner>, TurnRunOptions), Box<dyn Error + Send + Sync>>>
            + Send
            + 'static,
    {
        Self {
            standard_builtin_repair: false,
            permissions: None,
            mcp: None,
            #[cfg(test)]
            workspace_tools: None,
            #[cfg(test)]
            test_before_processors: Arc::new(std::sync::Mutex::new(Vec::new())),
            shared: Arc::new(Shared {
                conversation_id,
                state: Mutex::new(State {
                    context: Some(context),
                    active: None,
                    closed: false,
                    faulted: false,
                    records: HashMap::new(),
                    keys: HashMap::new(),
                    claims_revision: 0,
                }),
                assemble: Arc::new(move |work, control, reporter| {
                    Box::pin(assemble(work, control, reporter))
                }),
                config,
                candidate: Arc::new(|| TurnId::new(uuid::Uuid::now_v7().to_string())),
                progress: tokio::sync::broadcast::channel(super::progress::PROGRESS_CAPACITY).0,
            }),
        }
    }

    /// Clone a handle with submission and retained work access only.
    pub fn handle(&self) -> SessionHandle {
        SessionHandle {
            shared: self.shared.clone(),
        }
    }

    // Only the reviewed application assembly grants this provenance. Public
    // arbitrary runner/processor constructors must leave it disabled.
    pub(crate) fn allow_builtin_repair(mut self) -> Self {
        self.standard_builtin_repair = true;
        self
    }

    #[cfg(test)]
    pub(crate) fn workspace_tools(&self) -> Option<Arc<crate::tools::WorkspaceTools>> {
        self.workspace_tools.clone()
    }

    pub(crate) fn claims_revision(&self) -> u64 {
        self.shared.lock().claims_revision
    }

    pub(crate) fn inspect_uncommitted<R>(
        &self,
        work: &WorkRef,
        inspect: impl FnOnce(bool, &WorkResult, Option<&ToolBatch>) -> R,
    ) -> Result<R, SessionError> {
        let state = self.shared.lock();
        let record = self.shared.record(&state, work)?;
        let WorkState::Finished(result) = record.snapshot().state else {
            return Err(SessionError::NotFinished { work: work.clone() });
        };
        Ok(inspect(
            self.standard_builtin_repair,
            &result,
            state
                .records
                .get(&work.turn_id)
                .expect("retained record")
                .uncommitted
                .as_ref(),
        ))
    }

    /// Access idle materials under the admission lock.
    ///
    /// The callback must be short, synchronous and non-reentrant. Successful edits
    /// are not rolled back by its return value or a panic. A panic poisons the
    /// session and prevents fresh execution. Closure captures may be dropped even
    /// when this method rejects access without invoking the callback.
    pub fn with_context<R>(
        &mut self,
        f: impl FnOnce(&mut Context) -> R,
    ) -> Result<R, SessionError> {
        let mut state = self.shared.lock();
        if let Some(work) = &state.active {
            return Err(SessionError::Busy { work: work.clone() });
        }
        let context = state
            .context
            .as_mut()
            .ok_or(SessionError::ContextUnavailable)?;
        // Preserve ContextUnavailable as the precise lost-worker diagnosis.
        if self.shared.state.is_poisoned() {
            return Err(SessionError::Faulted);
        }
        Ok(f(context))
    }

    /// Claim the original uncommitted batch once, independently of result Arc counts.
    pub fn take_uncommitted_tool_batch(
        &mut self,
        work: &WorkRef,
    ) -> Result<Option<ToolBatch>, SessionError> {
        let mut state = self.shared.lock();
        let record = self.shared.record(&state, work)?;
        if !matches!(record.snapshot().state, WorkState::Finished(_)) {
            return Err(SessionError::NotFinished { work: work.clone() });
        }
        if state
            .records
            .get(&work.turn_id)
            .expect("record located")
            .uncommitted
            .is_some()
        {
            state.claims_revision = state
                .claims_revision
                .checked_add(1)
                .ok_or(SessionError::Faulted)?;
        }
        Ok(state
            .records
            .get_mut(&work.turn_id)
            .expect("record located")
            .uncommitted
            .take())
    }

    /// Explicitly release a terminal record, its key, original input and unclaimed batch.
    ///
    /// Captured observations and waits retain the original terminal value; a later
    /// submission with this key is independent and cannot rebind them.
    pub fn forget(&mut self, work: &WorkRef) -> Result<(), SessionError> {
        let mut state = self.shared.lock();
        let record = self.shared.record(&state, work)?;
        if !matches!(record.snapshot().state, WorkState::Finished(_)) {
            return Err(SessionError::NotFinished { work: work.clone() });
        }
        state.claims_revision = state
            .claims_revision
            .checked_add(1)
            .ok_or(SessionError::Faulted)?;
        let entry = state.records.remove(&work.turn_id).expect("record located");
        state.keys.remove(&entry.request_key);
        drop(state);
        // Error payloads may have application-defined destructors.
        drop(entry);
        Ok(())
    }

    /// Close new admission, cooperatively cancel active work and await supervision.
    ///
    /// Retained keys still replay. This does not forget records or discard returned
    /// materials. It requires the runtime to remain alive for the worker to finish.
    pub async fn shutdown(&mut self) {
        let active = {
            let mut state = self.shared.lock();
            state.closed = true;
            state
                .active
                .as_ref()
                .and_then(|work| self.shared.record(&state, work).ok())
        };
        if let Some(record) = active {
            record.control.cancellation_token().cancel();
            record.wait_finished().await;
        }
    }
}
impl Drop for Session {
    fn drop(&mut self) {
        let mut state = self.shared.lock();
        state.closed = true;
        if let Some(work) = &state.active
            && let Ok(record) = self.shared.record(&state, work)
        {
            record.control.cancellation_token().cancel();
        }
    }
}

#[cfg(test)]
impl Session {
    pub(super) fn set_candidate(&mut self, candidate: impl Fn() -> TurnId + Send + Sync + 'static) {
        Arc::get_mut(&mut self.shared)
            .expect("inject before handles")
            .candidate = Arc::new(candidate);
    }
}
