//! Shared instance permission state, views, and control handles.

use super::approval::{ApprovalReceiver, ApprovalsHub};
use super::rules::{
    CommandRule, CommandRuleSet, RuleDecision, WorkspaceAppendError, append_workspace_rules_file,
};
use crate::policy::PermissionMode;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, RwLock};

struct PermissionStateInner {
    mode: PermissionMode,
    revision: u64,
}

/// Read-only snapshot access to an instance's active permission mode, revision, and command rules.
#[derive(Clone)]
pub struct PermissionView {
    state: Arc<RwLock<PermissionStateInner>>,
    rules: Arc<RwLock<CommandRuleSet>>,
}

impl PermissionView {
    /// Read the current permission mode and revision together consistently.
    pub fn get(&self) -> (PermissionMode, u64) {
        let guard = self.state.read().expect("permission lock");
        (guard.mode, guard.revision)
    }

    /// Read the current permission mode.
    pub fn mode(&self) -> PermissionMode {
        self.get().0
    }

    /// Read the current revision number.
    pub fn revision(&self) -> u64 {
        self.get().1
    }

    /// Snapshot the currently active command rule layers.
    pub fn command_rules(&self) -> CommandRuleSet {
        self.rules.read().expect("command rules lock").clone()
    }
}

/// Backend state for safely appending rules to `<workspace_root>/.tars/permissions.toml`.
#[derive(Clone)]
pub(crate) struct WorkspaceRulesBackend {
    /// Canonical workspace root path.
    pub(crate) workspace_root: PathBuf,
    /// Persistent Store used to synchronize workspace trust records.
    pub(crate) store: crate::store::SessionStore,
    /// Whether the workspace rules file is currently trusted by the user.
    pub(crate) trusted: bool,
    /// Exact raw UTF-8 text of `.tars/permissions.toml` last trusted by this session,
    /// or `None` when the file did not exist when trusted/opened.
    pub(crate) trusted_raw_text: Option<String>,
}

/// Instance-level hub managing permission mode state, revisions, rules, and approvals.
pub(crate) struct PermissionHub {
    state: Arc<RwLock<PermissionStateInner>>,
    approvals: Arc<ApprovalsHub>,
    rules: Arc<RwLock<CommandRuleSet>>,
    workspace_backend: Arc<Mutex<Option<WorkspaceRulesBackend>>>,
}

impl PermissionHub {
    #[cfg(test)]
    pub(crate) fn new(initial_mode: PermissionMode) -> Self {
        Self::with_rules_and_backend(initial_mode, CommandRuleSet::new(), None)
    }

    pub(crate) fn with_rules_and_backend(
        initial_mode: PermissionMode,
        rules: CommandRuleSet,
        workspace_backend: Option<WorkspaceRulesBackend>,
    ) -> Self {
        Self::with_shared_approvals(
            initial_mode,
            rules,
            workspace_backend,
            Arc::new(ApprovalsHub::new()),
        )
    }

    pub(crate) fn with_shared_approvals(
        initial_mode: PermissionMode,
        rules: CommandRuleSet,
        workspace_backend: Option<WorkspaceRulesBackend>,
        approvals: Arc<ApprovalsHub>,
    ) -> Self {
        Self {
            state: Arc::new(RwLock::new(PermissionStateInner {
                mode: initial_mode,
                revision: 0,
            })),
            approvals,
            rules: Arc::new(RwLock::new(rules)),
            workspace_backend: Arc::new(Mutex::new(workspace_backend)),
        }
    }

    pub(crate) fn view(&self) -> PermissionView {
        PermissionView {
            state: self.state.clone(),
            rules: self.rules.clone(),
        }
    }

    pub(crate) fn can_save_workspace_rule(&self) -> bool {
        self.workspace_backend
            .lock()
            .expect("workspace rules backend lock")
            .as_ref()
            .is_some_and(|backend| backend.trusted)
    }

    pub(crate) fn trust_workspace_rules(&self, rules: Vec<CommandRule>, raw_text: String) {
        let mut backend = self
            .workspace_backend
            .lock()
            .expect("workspace rules backend lock");
        if let Some(backend) = backend.as_mut() {
            backend.trusted = true;
            backend.trusted_raw_text = Some(raw_text);
        }
        self.rules
            .write()
            .expect("command rules lock")
            .set_workspace_rules(rules);
    }

    pub(crate) fn add_session_rules(&self, prefixes: &[Vec<String>], outside_sandbox: bool) {
        let mut guard = self.rules.write().expect("command rules lock");
        for prefix in prefixes {
            guard.add_session_rule(CommandRule {
                prefix: prefix.clone(),
                decision: RuleDecision::Allow,
                outside_sandbox,
            });
        }
    }

    pub(crate) fn remove_session_rule(&self, index: usize) -> bool {
        self.rules
            .write()
            .expect("command rules lock")
            .remove_session_rule(index)
            .is_some()
    }

    pub(crate) fn clear_session_rules(&self) -> usize {
        let mut guard = self.rules.write().expect("command rules lock");
        let count = guard.session_rules().len();
        guard.clear_session_rules();
        count
    }

    pub(crate) fn append_workspace_rules(
        &self,
        prefixes: &[Vec<String>],
        outside_sandbox: bool,
    ) -> Result<(), WorkspaceAppendError> {
        let mut backend_guard = self
            .workspace_backend
            .lock()
            .expect("workspace rules backend lock");
        let Some(backend) = backend_guard.as_mut() else {
            return Err(WorkspaceAppendError::Failed);
        };
        if !backend.trusted {
            return Err(WorkspaceAppendError::Failed);
        }
        let candidate_rules: Vec<CommandRule> = prefixes
            .iter()
            .map(|prefix| CommandRule {
                prefix: prefix.clone(),
                decision: RuleDecision::Allow,
                outside_sandbox,
            })
            .collect();
        let outcome = append_workspace_rules_file(
            &backend.workspace_root,
            &backend.store,
            backend.trusted_raw_text.as_deref(),
            &candidate_rules,
        )?;
        backend.trusted_raw_text = Some(outcome.new_raw_text);
        let mut rules_guard = self.rules.write().expect("command rules lock");
        for rule in outcome.added_rules {
            rules_guard.add_workspace_rule(rule);
        }
        Ok(())
    }

    pub(crate) fn command_rules(&self) -> CommandRuleSet {
        self.rules.read().expect("command rules lock").clone()
    }

    #[cfg(test)]
    pub(crate) fn set_rules(&self, rules: CommandRuleSet) {
        *self.rules.write().expect("command rules lock") = rules;
    }

    pub(crate) fn approvals(&self) -> Arc<ApprovalsHub> {
        self.approvals.clone()
    }

    pub(crate) fn get(&self) -> (PermissionMode, u64) {
        let guard = self.state.read().expect("permission lock");
        (guard.mode, guard.revision)
    }

    pub(crate) fn set(&self, mode: PermissionMode) -> (PermissionMode, u64) {
        let mut guard = self.state.write().expect("permission lock");
        if guard.mode != mode {
            guard.mode = mode;
            guard.revision += 1;
        }
        (guard.mode, guard.revision)
    }

    pub(crate) fn connect_approvals(&self) -> ApprovalReceiver {
        self.approvals.connect()
    }
}
