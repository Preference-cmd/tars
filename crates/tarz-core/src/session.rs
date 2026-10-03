//! Single-owner conversation materials with retained, supervised work records.
//!
//! Capacity bounds record count, not memory: the current context, original input
//! copies, terminal failure payloads, unclaimed tool batches and external result
//! `Arc`s all retain memory independently. No history or crash recovery is implied.

mod handle;
mod owner;
mod state;
mod types;
mod worker;

pub use handle::SessionHandle;
pub use owner::Session;
pub use types::{
    ConversationId, SessionConfig, SessionError, SubmitRejection, SubmitRequest, WorkObservation,
    WorkReceipt, WorkRef, WorkResult, WorkState, WorkerFault,
};

#[cfg(test)]
mod tests;
