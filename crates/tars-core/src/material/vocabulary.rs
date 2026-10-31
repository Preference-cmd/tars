//! Reads Context written by earlier Causa versions.
//!
//! Causa reads only the current form, so every read of stored Context goes
//! through [`decode`], which rewrites two older forms before decoding:
//!
//! - Tool-result statuses and artifact kinds written before they became
//!   snake_case (`"Succeeded"`, `"FullOutput"`).
//! - Message blocks written before the role became a fact of the block:
//!   `{"shape":"parts","value":[..]}` becomes
//!   `{"shape":"message","value":{"role":..,"parts":[..]}}`. The role is the
//!   one the renderer used to infer from `meta.source`: absent or
//!   `"assistant"` is `assistant`, `"system"` is `system`, anything else is
//!   `user`. TARS's own material repair note is `system`, as it is written
//!   now; the old rule showed it to the model as user input.
//!
//! Each rewrite is by position and leaves current data unchanged, so it
//! applies to every file regardless of age; the next write stores the current
//! form and the record format stays the same.

use serde::de::DeserializeOwned;
use serde_json::Value;

const STATUSES: [(&str, &str); 6] = [
    ("Succeeded", "succeeded"),
    ("Failed", "failed"),
    ("Rejected", "rejected"),
    ("Cancelled", "cancelled"),
    ("TimedOut", "timed_out"),
    ("UnknownOutcome", "unknown_outcome"),
];
/// The source tag of the note TARS appends after a material repair.
const REPAIR_NOTE_SOURCE: &str = "application:tars:material-repair";
const ARTIFACT_KINDS: [(&str, &str); 3] = [
    ("FullOutput", "full_output"),
    ("PipeCache", "pipe_cache"),
    ("Binary", "binary"),
];

/// Decode a stored Context, or a session record holding one under `context`.
pub(crate) fn decode<T: DeserializeOwned>(bytes: &[u8]) -> Result<T, serde_json::Error> {
    let mut value: Value = serde_json::from_slice(bytes)?;
    let context = match value.get_mut("context") {
        Some(context) => context,
        None => &mut value,
    };
    if let Some(blocks) = context.get_mut("blocks").and_then(Value::as_array_mut) {
        for block in blocks {
            upgrade_tool_result(block);
            upgrade_message(block);
        }
    }
    serde_json::from_value(value)
}

fn upgrade_tool_result(block: &mut Value) {
    let Some(content) = block.get_mut("content") else {
        return;
    };
    if content.get("shape").and_then(Value::as_str) != Some("tool_result") {
        return;
    }
    let Some(result) = content.get_mut("value") else {
        return;
    };
    if let Some(status) = result.get_mut("status") {
        rename(status, &STATUSES);
    }
    if let Some(kind) = result.pointer_mut("/output/artifact/kind") {
        rename(kind, &ARTIFACT_KINDS);
    }
}

fn upgrade_message(block: &mut Value) {
    if block.pointer("/content/shape").and_then(Value::as_str) != Some("parts") {
        return;
    }
    let role = match block.pointer("/meta/source").and_then(Value::as_str) {
        None | Some("assistant") => "assistant",
        Some("system" | REPAIR_NOTE_SOURCE) => "system",
        Some(_) => "user",
    };
    let Some(content) = block.get_mut("content") else {
        return;
    };
    let parts = content
        .get_mut("value")
        .map(Value::take)
        .unwrap_or_default();
    *content = serde_json::json!({
        "shape": "message",
        "value": {"role": role, "parts": parts},
    });
}

fn rename(value: &mut Value, names: &[(&str, &str)]) {
    let current = value
        .as_str()
        .and_then(|old| names.iter().find(|(before, _)| *before == old));
    if let Some((_, now)) = current {
        *value = Value::String((*now).into());
    }
}

#[cfg(test)]
mod tests {
    use causa::kernel::{
        ArtifactKind, ArtifactRef, BlockContent, BlockMeta, Context, ContextBlock, ToolOutput,
        ToolResultPayload, ToolResultStatus,
    };
    use serde_json::json;

    #[derive(serde::Deserialize)]
    struct Record {
        context: Context,
    }

    fn result_block(status: ToolResultStatus, kind: Option<ArtifactKind>) -> ContextBlock {
        // The content repeats the old strings; only the two fields may change.
        let mut output = ToolOutput::new(json!({"status": "Succeeded", "kind": "FullOutput"}));
        output.artifact = kind.map(|kind| ArtifactRef::new("asset", 3, kind, true));
        ContextBlock::new(
            causa::runtime::new_block_id(),
            BlockContent::ToolResult(ToolResultPayload::new(
                causa::runtime::new_block_id(),
                status,
                output,
            )),
            BlockMeta::default(),
        )
    }

    #[test]
    fn earlier_statuses_and_artifact_kinds_read_as_the_current_ones() {
        let cases = [
            (ToolResultStatus::Succeeded, "Succeeded", None),
            (ToolResultStatus::Failed, "Failed", None),
            (ToolResultStatus::Rejected, "Rejected", None),
            (ToolResultStatus::Cancelled, "Cancelled", None),
            (
                ToolResultStatus::TimedOut,
                "TimedOut",
                Some((ArtifactKind::PipeCache, "PipeCache")),
            ),
            (
                ToolResultStatus::UnknownOutcome,
                "UnknownOutcome",
                Some((ArtifactKind::FullOutput, "FullOutput")),
            ),
            (
                ToolResultStatus::Succeeded,
                "Succeeded",
                Some((ArtifactKind::Binary, "Binary")),
            ),
        ];
        for (status, old_status, artifact) in cases {
            let block = result_block(status, artifact.as_ref().map(|(kind, _)| kind.clone()));
            let current = json!({"blocks": [block]});
            let mut earlier = current.clone();
            let result = &mut earlier["blocks"][0]["content"]["value"];
            assert!(result["status"].is_string());
            result["status"] = json!(old_status);
            if let Some((_, old_kind)) = artifact {
                assert!(result["output"]["artifact"]["kind"].is_string());
                result["output"]["artifact"]["kind"] = json!(old_kind);
            }
            assert!(serde_json::from_value::<Context>(earlier.clone()).is_err());

            let expected: Context = serde_json::from_value(current.clone()).unwrap();
            for stored in [&earlier, &current] {
                let bytes = serde_json::to_vec(stored).unwrap();
                let plain: Context = super::decode(&bytes).unwrap();
                assert_eq!(plain.blocks(), expected.blocks());
                assert_eq!(serde_json::to_value(&plain).unwrap(), current);
                // A session record holds the same Context under `context`.
                let bytes = serde_json::to_vec(&json!({"context": stored})).unwrap();
                let record: Record = super::decode(&bytes).unwrap();
                assert_eq!(record.context.blocks(), expected.blocks());
            }
        }
    }

    #[test]
    fn earlier_message_blocks_read_with_the_role_the_renderer_inferred() {
        use causa::kernel::{ContentPart, Role, TextPayload};
        let cases = [
            (None, Role::Assistant),
            (Some("assistant"), Role::Assistant),
            (Some("system"), Role::System),
            (Some("user"), Role::User),
            (Some("inject"), Role::User),
            (Some(super::REPAIR_NOTE_SOURCE), Role::System),
        ];
        for (source, role) in cases {
            let meta = source.map_or_else(BlockMeta::default, BlockMeta::from_source);
            let block = ContextBlock::new(
                causa::runtime::new_block_id(),
                BlockContent::Message {
                    role,
                    parts: vec![ContentPart::Text(TextPayload::new("parts"))],
                },
                meta,
            );
            let current = json!({"blocks": [block]});
            let mut earlier = current.clone();
            let parts = earlier["blocks"][0]["content"]["value"]["parts"].clone();
            earlier["blocks"][0]["content"] = json!({"shape": "parts", "value": parts});
            assert!(serde_json::from_value::<Context>(earlier.clone()).is_err());

            let expected: Context = serde_json::from_value(current.clone()).unwrap();
            for stored in [&earlier, &current] {
                let bytes = serde_json::to_vec(stored).unwrap();
                let plain: Context = super::decode(&bytes).unwrap();
                assert_eq!(plain.blocks(), expected.blocks(), "source {source:?}");
                assert_eq!(serde_json::to_value(&plain).unwrap(), current);
                let bytes = serde_json::to_vec(&json!({"context": stored})).unwrap();
                let record: Record = super::decode(&bytes).unwrap();
                assert_eq!(record.context.blocks(), expected.blocks());
            }
        }
    }
}
