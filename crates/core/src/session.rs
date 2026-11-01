//! Single-owner conversation materials with retained, supervised work records.
//!
//! Capacity bounds record count, not memory: the current context, original input
//! copies, terminal failure payloads, unclaimed tool batches and external result
//! `Arc`s all retain memory independently. No history or crash recovery is implied.

mod failure;
mod handle;
mod owner;
mod progress;
mod state;
mod tool_progress;
mod types;
mod worker;

pub use failure::ModelFailure;
pub use handle::SessionHandle;
pub use owner::Session;
pub use tool_progress::{ToolPhase, ToolProgress, ToolProgressSnapshot, ToolReporter};
pub use types::{
    ConversationId, ProgressUpdate, SessionConfig, SessionError, SubmitRejection, SubmitRequest,
    WorkObservation, WorkProgress, WorkReceipt, WorkRef, WorkResult, WorkState, WorkerFault,
};

#[cfg(test)]
mod tests;

#[cfg(test)]
pub(crate) mod progress_tests;
