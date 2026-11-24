//! Multi-agent session coordinator, event-driven scheduling loop, and public request/outcome types.

mod engine;
mod types;

pub(super) use engine::SessionCoordinator;
pub use types::{
    CoordinatorConfig, CoordinatorError, MessageAgentRequest, OperationOutcome, OperationReceipt,
    ReadAgentResult, SpawnAgentOutcome, SpawnAgentRequest,
};
