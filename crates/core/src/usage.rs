//! In-memory tracking of model call counts and token usage across sessions and works.
//!
//! TARS counts all requests sent to model endpoints, separated by purpose ([`UsagePurpose::Conversation`]
//! vs [`UsagePurpose::Compaction`]). A counting gateway sits inside the retry layer and outside
//! the protocol gateway, ensuring every attempt (including transient retries) is counted accurately.

pub(crate) mod gateway;
pub(crate) mod hub;
mod types;

pub(crate) use gateway::CountingGateway;
pub(crate) use hub::UsageHub;
pub use types::{
    CallCounts, CallUsage, LastCall, PurposeUsage, SessionUsage, UsagePurpose, UsageTotals,
    WorkUsage,
};

#[cfg(test)]
mod tests;
