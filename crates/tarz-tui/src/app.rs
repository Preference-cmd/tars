//! Terminal presentation state and asynchronous adapters; business rules stay in core.
mod actions;
mod events;
mod jobs;
mod state;

pub(crate) use state::App;
pub(crate) use state::Confirmation;

#[cfg(test)]
mod tests;
