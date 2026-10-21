//! Shared instance permission state, views, and control handles.

use super::approval::{ApprovalReceiver, ApprovalsHub};
use super::mode::PermissionMode;
use std::sync::{Arc, RwLock};

struct PermissionStateInner {
    mode: PermissionMode,
    revision: u64,
}

/// Read-only snapshot access to an instance's active permission mode and revision.
#[derive(Clone)]
pub struct PermissionView {
    state: Arc<RwLock<PermissionStateInner>>,
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
}

/// Instance-level hub managing permission mode state, revisions, and approvals.
pub(crate) struct PermissionHub {
    state: Arc<RwLock<PermissionStateInner>>,
    approvals: Arc<ApprovalsHub>,
}

impl PermissionHub {
    pub(crate) fn new(initial_mode: PermissionMode) -> Self {
        Self {
            state: Arc::new(RwLock::new(PermissionStateInner {
                mode: initial_mode,
                revision: 0,
            })),
            approvals: Arc::new(ApprovalsHub::new()),
        }
    }

    pub(crate) fn view(&self) -> PermissionView {
        PermissionView {
            state: self.state.clone(),
        }
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
