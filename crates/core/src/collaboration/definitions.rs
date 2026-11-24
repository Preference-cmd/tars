use crate::events::MAX_READ_AGENT_PAGE_BYTES;
use causa::kernel::ToolDefinition;
use serde_json::json;

/// Tool name for spawning a narrowed child subagent from a discovered preset.
pub const SPAWN_AGENT_TOOL_NAME: &str = "spawn_agent";

/// Tool name for sending follow-up work to an existing idle `reusable` agent.
pub const MESSAGE_AGENT_TOOL_NAME: &str = "message_agent";

/// Tool name for reading paginated archived output from a fixed `operation_id`.
pub const READ_AGENT_TOOL_NAME: &str = "read_agent";

/// Tool name for cooperatively cancelling an operation and its active descendants.
pub const CANCEL_AGENT_TOOL_NAME: &str = "cancel_agent";

/// Returns the static `ToolDefinition` schemas for the 4 collaboration tools.
pub fn collaboration_tool_definitions() -> Vec<ToolDefinition> {
    vec![
        spawn_agent_definition(),
        message_agent_definition(),
        read_agent_definition(),
        cancel_agent_definition(),
    ]
}

fn spawn_agent_definition() -> ToolDefinition {
    ToolDefinition::new(
        SPAWN_AGENT_TOOL_NAME,
        "Spawn a narrowed subagent from an available preset in <available_agents>. \
         Use background=false (default) to await the result directly (supports parallel calls), \
         or background=true to receive an immediate operation_id and be notified via <task-notification>.",
        json!({
            "type": "object",
            "properties": {
                "agent": {
                    "type": "string",
                    "minLength": 1,
                    "description": "Exact preset name from <available_agents>."
                },
                "prompt": {
                    "type": "string",
                    "minLength": 1,
                    "description": "Task instructions for the child agent."
                },
                "model": {
                    "type": "object",
                    "description": "Optional model profile or generation parameter overrides.",
                    "properties": {
                        "profile": { "type": "string", "minLength": 1 },
                        "id": { "type": "string", "minLength": 1 },
                        "reasoning_effort": { "type": "string", "minLength": 1 },
                        "temperature": { "type": "number", "minimum": 0.0, "maximum": 2.0 },
                        "max_tokens": { "type": "integer", "minimum": 1 }
                    },
                    "additionalProperties": false
                },
                "background": {
                    "type": "boolean",
                    "default": false,
                    "description": "If true, admit and return immediately with status='running'."
                },
                "reusable": {
                    "type": "boolean",
                    "default": false,
                    "description": "Override whether the spawned agent retains state for message_agent."
                },
                "on_parent_complete": {
                    "type": "string",
                    "enum": ["continue", "cancel"],
                    "default": "continue",
                    "description": "Whether a background child continues or is cancelled when the parent turn finishes normally."
                },
                "timeout_secs": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": 86400,
                    "description": "Optional execution timeout in seconds (cannot exceed parent limit)."
                },
                "max_rounds": {
                    "type": "integer",
                    "minimum": 1,
                    "description": "Optional model round limit (cannot exceed parent or preset limit)."
                }
            },
            "required": ["agent", "prompt"],
            "additionalProperties": false
        }),
    )
}

fn message_agent_definition() -> ToolDefinition {
    ToolDefinition::new(
        MESSAGE_AGENT_TOOL_NAME,
        "Send follow-up instructions to an existing idle reusable subagent by agent_id.",
        json!({
            "type": "object",
            "properties": {
                "agent_id": {
                    "type": "string",
                    "minLength": 1,
                    "description": "Target reusable agent_id returned by spawn_agent."
                },
                "prompt": {
                    "type": "string",
                    "minLength": 1,
                    "description": "Follow-up task instructions."
                },
                "background": {
                    "type": "boolean",
                    "default": false,
                    "description": "If true, admit and return immediately with status='running'."
                },
                "on_parent_complete": {
                    "type": "string",
                    "enum": ["continue", "cancel"],
                    "default": "continue"
                },
                "timeout_secs": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": 86400
                },
                "max_rounds": {
                    "type": "integer",
                    "minimum": 1
                }
            },
            "required": ["agent_id", "prompt"],
            "additionalProperties": false
        }),
    )
}

fn read_agent_definition() -> ToolDefinition {
    ToolDefinition::new(
        READ_AGENT_TOOL_NAME,
        "Read the current status and a UTF-8 safe output page for an operation_id without re-executing.",
        json!({
            "type": "object",
            "properties": {
                "operation_id": {
                    "type": "string",
                    "minLength": 1,
                    "description": "Target operation_id to inspect."
                },
                "offset_bytes": {
                    "type": "integer",
                    "minimum": 0,
                    "description": "Byte offset to start reading from (default 0)."
                },
                "limit_bytes": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": MAX_READ_AGENT_PAGE_BYTES,
                    "description": "Maximum UTF-8 bytes to return (capped at 64 KiB)."
                }
            },
            "required": ["operation_id"],
            "additionalProperties": false
        }),
    )
}

fn cancel_agent_definition() -> ToolDefinition {
    ToolDefinition::new(
        CANCEL_AGENT_TOOL_NAME,
        "Cooperatively cancel a running or creating operation and all active descendants.",
        json!({
            "type": "object",
            "properties": {
                "operation_id": {
                    "type": "string",
                    "minLength": 1,
                    "description": "Target operation_id to cancel."
                }
            },
            "required": ["operation_id"],
            "additionalProperties": false
        }),
    )
}
