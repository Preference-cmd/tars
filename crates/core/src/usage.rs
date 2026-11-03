//! In-memory tracking of model call counts and token usage across sessions and works.
//!
//! TARS counts all requests sent to model endpoints, separated by purpose ([`UsagePurpose::Conversation`]
//! vs [`UsagePurpose::Compaction`]). A counting gateway sits inside the retry layer and outside
//! the protocol gateway, ensuring every attempt (including transient retries) is counted accurately.

mod display;
pub(crate) mod gateway;
pub(crate) mod hub;
mod types;

pub use display::session_usage_lines;
pub(crate) use gateway::CountingGateway;
pub(crate) use hub::UsageHub;
pub use types::{
    CallCounts, CallUsage, LastCall, PurposeUsage, SessionUsage, UsageFigures, UsagePurpose,
    UsageTotals, WorkUsage,
};
pub(crate) use types::{from_unix_millis, unix_millis};

#[cfg(test)]
mod tests;
