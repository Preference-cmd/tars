//! Terminal presentation state and asynchronous adapters; business rules stay in core.
mod actions;
mod commands;
mod events;
mod jobs;
mod progress;
mod repair;
mod state;

pub(crate) use commands::{COMMANDS, Panel, matching_commands};
pub(crate) use state::Confirmation;
pub(crate) use state::{App, Entry};

#[cfg(test)]
mod tests;

mod tool_progress;
