//! Terminal presentation state and asynchronous adapters; business rules stay in core.
mod actions;
pub(crate) mod clock;
mod commands;
mod events;
pub(crate) mod history;
mod jobs;
mod progress;
mod repair;
mod state;

#[allow(unused_imports)]
pub(crate) use clock::Clock;
pub(crate) use commands::{COMMANDS, Panel, matching_commands};
pub(crate) use state::Confirmation;
pub(crate) use state::{App, Entry};

#[cfg(test)]
mod tests;

mod tool_progress;
