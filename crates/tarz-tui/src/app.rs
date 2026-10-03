//! Terminal presentation state and asynchronous adapters; business rules stay in core.
mod actions;
mod commands;
mod events;
mod jobs;
mod state;

pub(crate) use commands::{COMMANDS, Panel, matching_commands};
pub(crate) use state::App;
pub(crate) use state::Confirmation;

#[cfg(test)]
mod tests;
