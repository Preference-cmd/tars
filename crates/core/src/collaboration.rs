//! Pure event-driven 4-tool multi-agent collaboration surface.
//!
//! Exposes `spawn_agent`, `message_agent`, `read_agent`, and `cancel_agent`
//! backed directly by the application `SessionCoordinator`, with trusted
//! runtime caller binding and zero polling tools (`wait_agent`/`agent_catalog`/`agent_list`).

mod definitions;
mod tools;

#[cfg(test)]
mod tests;

pub use definitions::{
    CANCEL_AGENT_TOOL_NAME, MESSAGE_AGENT_TOOL_NAME, READ_AGENT_TOOL_NAME, SPAWN_AGENT_TOOL_NAME,
    collaboration_tool_definitions,
};
pub(crate) use tools::{CollaborationContext, collaboration_tools};
