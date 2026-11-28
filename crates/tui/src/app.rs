//! Terminal presentation state and asynchronous adapters; business rules stay in core.
mod actions;
pub(crate) mod clock;
mod commands;
mod events;
pub(crate) mod history;
mod jobs;
mod progress;
pub(crate) mod provider;
mod repair;
mod startup;
mod state;

#[allow(unused_imports)]
pub(crate) use clock::Clock;
#[allow(unused_imports)]
pub(crate) use commands::TRUST_PANEL_TITLE;
pub(crate) use commands::{COMMANDS, InfoKind, Panel, matching_commands};
pub(crate) use startup::Startup;
#[cfg(test)]
pub(crate) use state::CTRL_C_EXIT_GRACE_WINDOW;
#[allow(unused_imports)]
pub(crate) use state::FindState;
pub(crate) use state::{
    App, AtPopupState, CONFIRM_GUARD_WINDOW, Confirmation, Entry, GuardWindow, QueuedPrompt,
    WorkspaceTrustLine, WorkspaceTrustLineKind,
};

#[cfg(test)]
mod tests;

mod tool_progress;
