use crate::app::{CONFIRM_GUARD_WINDOW, GuardWindow};
use std::{path::PathBuf, time::Instant};

/// Interactive folder-trust wizard shown before creating or loading workspace context.
pub(crate) struct FolderTrustWizard {
    /// Resolved target workspace directory awaiting folder-level trust confirmation.
    pub workspace_root: PathBuf,
    /// Highlighted decision (`0 = Yes, trust this folder`, `1 = No, exit`).
    pub selected: usize,
    /// Optional error message displayed when reading or persisting trust fails.
    pub error: String,
    /// Whether a persistence job is currently in flight.
    pub saving: bool,
    /// Accidental-confirmation guard window armed when the wizard appears.
    pub guard: GuardWindow,
}

impl FolderTrustWizard {
    /// Create a wizard for `workspace_root`, armed with [`CONFIRM_GUARD_WINDOW`].
    pub(crate) fn new(workspace_root: PathBuf, now: Instant) -> Self {
        let mut guard = GuardWindow::new();
        guard.arm(now, CONFIRM_GUARD_WINDOW);
        Self {
            workspace_root,
            selected: 0,
            error: String::new(),
            saving: false,
            guard,
        }
    }

    /// Return whether accidental confirmation is currently blocked by the guard window.
    pub(crate) fn is_guarded(&self, now: Instant) -> bool {
        self.guard.is_guarded(now)
    }

    /// Toggle the highlighted choice between `Yes` (`0`) and `No` (`1`).
    pub(crate) fn toggle(&mut self) {
        self.selected = usize::from(self.selected == 0);
    }
}
