//! Workspace filesystem tools with bounded operations and work-owned cleanup.

mod diff;
mod execute;
mod operations;
mod parameters;
mod resources;

#[cfg(test)]
pub use diff::DiffSummary;
pub use diff::compute_diff;
pub use execute::WorkspaceTools;
#[cfg(test)]
pub use resources::ToolMetrics;
pub use resources::ToolResources;

#[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
mod tests;
#[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
mod write_tests;
