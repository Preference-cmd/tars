//! Workspace filesystem tools with bounded operations and work-owned cleanup.

mod command;
mod diff;
mod execute;
pub(crate) mod operations;
pub(crate) mod parameters;
pub(crate) mod resources;
mod search;

pub(crate) use command::CommandRuntime;
pub(crate) use search::SearchRuntime;

#[cfg(test)]
#[cfg_attr(not(unix), allow(unused_imports))]
pub use diff::DiffSummary;
pub use diff::compute_diff;
pub(crate) use execute::PreparedWrite;
#[cfg(test)]
#[cfg_attr(not(unix), allow(unused_imports))]
pub(crate) use execute::TestGate;
pub use execute::WorkspaceTools;
pub(crate) use execute::result;
#[cfg(test)]
pub use resources::ToolMetrics;
pub use resources::ToolResources;

#[cfg(all(test, target_os = "macos"))]
mod command_acceptance_tests;
#[cfg(test)]
mod command_runtime_tests;
#[cfg(test)]
mod command_tool_tests;
#[cfg(all(test, target_os = "macos"))]
mod search_acceptance_tests;
#[cfg(test)]
mod search_tool_tests;
#[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
mod tests;
#[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
mod write_tests;
