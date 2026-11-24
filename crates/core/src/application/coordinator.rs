//! Multi-agent session coordinator, event-driven scheduling loop, and public request/outcome types.

mod engine;
mod types;

pub(super) use engine::SessionCoordinator;
pub use types::{
    CoordinatorConfig, CoordinatorError, DEFAULT_OPERATION_TIMEOUT_SECS, MessageAgentRequest,
    OperationOutcome, OperationReceipt, ReadAgentResult, SpawnAgentOutcome, SpawnAgentRequest,
};
