//! Workspace filesystem tools with bounded operations and work-owned cleanup.

mod diff;
mod execute;
pub(crate) mod operations;
pub(crate) mod parameters;
pub(crate) mod resources;

#[cfg(test)]
pub use diff::DiffSummary;
pub use diff::compute_diff;
pub(crate) use execute::PreparedWrite;
pub use execute::WorkspaceTools;
pub(crate) use execute::result;
#[cfg(test)]
pub use resources::ToolMetrics;
pub use resources::ToolResources;

#[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
mod tests;
#[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
mod write_tests;
