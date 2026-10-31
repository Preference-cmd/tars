//! MCP servers attached to one open session.
//!
//! A frozen session definition names the MCP servers to connect when the
//! session opens. Each connection becomes a dynamic tool source for the
//! model; the servers' tools are filtered against the frozen allow/exclude
//! lists and bounded to the model-facing naming and result limits before the
//! model sees them. Closing the session closes those connections.
//!
//! MCP servers run as separate processes or remote endpoints with the user's
//! authority. TARS's workspace boundary does not constrain them.

mod runtime;
mod source;
mod status;
mod stderr;

// The tests start a stdio fixture process and probe it with `kill`.
#[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
mod tests;

pub(crate) use runtime::McpRuntime;
pub use status::{McpServerStatus, McpSkippedTool};
