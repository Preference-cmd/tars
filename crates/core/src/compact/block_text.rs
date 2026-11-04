//! Shared deterministic block-to-text conversion for Context blocks.
//!
//! Used by history artifact storage and recall operations to produce a
//! readable, deterministic plain text representation of any Context block.

use causa::kernel::{BlockContent, ContentPart, ContextBlock};

/// Render a single Context block into its deterministic plain text representation.
pub(crate) fn block_to_text(block: &ContextBlock) -> String {
    match block.content() {
        BlockContent::Message { parts, .. } => {
            let mut pieces: Vec<String> = Vec::new();
            for part in parts {
                match part {
                    ContentPart::Text(text) => pieces.push(text.0.clone()),
                    ContentPart::Media(media) => {
                        let name_or_ref = media.name.as_deref().unwrap_or(&media.reference);
                        pieces.push(format!("[media {} {name_or_ref}]", media.media_type));
                    }
                    ContentPart::Sealed(_) => {}
                }
            }
            pieces.join("\n")
        }
        BlockContent::ToolCall(call) => {
            let args = serde_json::to_string_pretty(&call.arguments)
                .unwrap_or_else(|_| call.arguments.to_string());
            format!("call {}\n{args}", call.tool_name)
        }
        BlockContent::ToolResult(result) => {
            let mut text = match &result.output.content {
                serde_json::Value::String(s) => s.clone(),
                other => serde_json::to_string_pretty(other).unwrap_or_else(|_| other.to_string()),
            };
            for media in &result.media {
                let name_or_ref = media.name.as_deref().unwrap_or(&media.reference);
                if !text.is_empty() && !text.ends_with('\n') {
                    text.push('\n');
                }
                text.push_str(&format!("[media {} {name_or_ref}]", media.media_type));
            }
            text
        }
        BlockContent::ToolChange(change) => {
            let added_names: Vec<&str> = change.added.iter().map(|t| t.name.as_str()).collect();
            let removed_names: Vec<&str> = change.removed.iter().map(|s| s.as_str()).collect();
            match (added_names.is_empty(), removed_names.is_empty()) {
                (true, true) => "tools unchanged".to_string(),
                (false, true) => format!("tools added: {}", added_names.join(", ")),
                (true, false) => format!("tools removed: {}", removed_names.join(", ")),
                (false, false) => format!(
                    "tools added: {}; removed: {}",
                    added_names.join(", "),
                    removed_names.join(", ")
                ),
            }
        }
    }
}
