//! Sandboxed read-only JavaScript orchestration (`run_code`) powered by `rquickjs`.
//!
//! Each `run_code` call executes a fresh, stateless ES2020 async script inside a
//! dedicated `rquickjs::AsyncRuntime` / `AsyncContext` hosted on a blocking thread
//! with a single-threaded Tokio runtime and `LocalSet`. Read-only subcalls bridge
//! back to the outer Tokio runtime and reuse the session's real
//! [`WorkspaceTools`](super::WorkspaceTools) and [`RecallTool`](crate::compact::RecallTool)
//! implementations without emitting `ToolReporter` snapshots or triggering inner
//! `OutputSpillProcessor` spills.

mod host;
mod sandbox;
mod tool;

pub(crate) use host::{BuiltinBindings, SubcallHost};
pub(crate) use tool::RunCodeTool;

#[cfg(test)]
mod tests;
