//! Entry-point spilling of oversized tool outputs via `ToolBatchProcessor::after`.
//!
//! When a tool result's text payload in a persistent session exceeds
//! [`SPILL_THRESHOLD_TOKENS`] (and is neither `recall` itself nor already
//! backed by an artifact), the full text is persisted atomically to the
//! session's [`ArtifactStore`], and the in-Context `ToolOutput` is replaced
//! in place with a middle-truncated head/tail preview pointing at
//! `recall id=artifact:<artifact_id>`. If persisting fails, the original output
//! is kept unchanged so tool execution is never disrupted.

use super::estimate;
use super::recall::TOOL_NAME as RECALL_TOOL_NAME;
use async_trait::async_trait;
use causa::kernel::{
    ArtifactHint, ArtifactKind, ArtifactStore, ProcessorContext, ProcessorError, ToolBatch,
    ToolBatchProcessor, Truncation,
};
use serde_json::Value;
use std::sync::Arc;

/// Estimated token threshold above which tool outputs are spilled at entry.
pub(crate) const SPILL_THRESHOLD_TOKENS: u64 = 4_000;

/// Maximum number of leading lines kept in a spilled preview.
pub(crate) const SPILL_PREVIEW_HEAD_LINES: usize = 40;

/// Maximum number of trailing lines kept in a spilled preview.
pub(crate) const SPILL_PREVIEW_TAIL_LINES: usize = 40;

/// Maximum estimated token budget for the spilled preview (`head + notice + tail`).
pub(crate) const SPILL_PREVIEW_MAX_TOKENS: u64 = 2_000;

/// Serialized JSON byte cap matching [`SPILL_PREVIEW_MAX_TOKENS`] at 3 bytes/token.
const MAX_PREVIEW_JSON_BYTES: usize = (SPILL_PREVIEW_MAX_TOKENS as usize) * 3;

/// Maximum serialized JSON byte size of sibling metadata fields for in-place object spilling.
const MAX_DOMINANT_SIBLINGS_JSON_BYTES: usize = 1024;

/// Post-execution processor that spills oversized tool outputs to an [`ArtifactStore`].
pub(crate) struct OutputSpillProcessor {
    store: Arc<dyn ArtifactStore>,
}

impl OutputSpillProcessor {
    /// Create a spill processor writing oversized outputs into `store`.
    pub(crate) fn new(store: Arc<dyn ArtifactStore>) -> Self {
        Self { store }
    }
}

#[async_trait]
impl ToolBatchProcessor for OutputSpillProcessor {
    async fn process(
        &self,
        batch: &mut ToolBatch,
        _ctx: &ProcessorContext<'_>,
    ) -> Result<(), ProcessorError> {
        for entry in batch.results_mut() {
            let tool_name = entry.call().input.tool_name.clone();
            if tool_name == RECALL_TOOL_NAME {
                continue;
            }
            let call_block_id = entry.call().call_block_id;
            let Some(output) = entry.output_mut() else {
                continue;
            };
            if output.artifact.is_some() {
                continue;
            }

            if let Value::Object(map) = &output.content
                && let Some(dominant) = find_dominant_text_field(map)
            {
                let whole_bytes =
                    serde_json::to_vec(&output.content).map_or(usize::MAX, |bytes| bytes.len());
                let original_tokens = estimate::estimate_bytes(whole_bytes as u64);
                if original_tokens <= SPILL_THRESHOLD_TOKENS {
                    continue;
                }
                let hint = ArtifactHint::new(tool_name, call_block_id, ArtifactKind::FullOutput);
                let Ok(artifact_ref) = self.store.persist(dominant.text.as_bytes(), hint).await
                else {
                    // Degrade gracefully: keep the original output without failing the tool call.
                    continue;
                };
                let max_value_json_bytes =
                    MAX_PREVIEW_JSON_BYTES.saturating_sub(dominant.shell_overhead);
                let preview = build_spill_preview_within(
                    dominant.text,
                    original_tokens,
                    &artifact_ref.id,
                    max_value_json_bytes,
                );
                let mut updated = map.clone();
                updated.insert(dominant.key.to_owned(), Value::String(preview));
                output.content = Value::Object(updated);
                output.truncation = Truncation::Middle;
                let meta = output.meta.get_or_insert_with(Default::default);
                meta.original_tokens = Some(usize::try_from(original_tokens).unwrap_or(usize::MAX));
                output.artifact = Some(artifact_ref);
                continue;
            }

            let Some(text) = extract_text(&output.content) else {
                continue;
            };
            let original_tokens = estimate::estimate_bytes(text.len() as u64);
            if original_tokens <= SPILL_THRESHOLD_TOKENS {
                continue;
            }

            let hint = ArtifactHint::new(tool_name, call_block_id, ArtifactKind::FullOutput);
            let Ok(artifact_ref) = self.store.persist(text.as_bytes(), hint).await else {
                // Degrade gracefully: keep the original output without failing the tool call.
                continue;
            };

            let preview = build_spill_preview(&text, original_tokens, &artifact_ref.id);
            output.content = Value::String(preview);
            output.truncation = Truncation::Middle;
            let meta = output.meta.get_or_insert_with(Default::default);
            meta.original_tokens = Some(usize::try_from(original_tokens).unwrap_or(usize::MAX));
            output.artifact = Some(artifact_ref);
        }
        Ok(())
    }
}

struct DominantTextField<'a> {
    key: &'a str,
    text: &'a str,
    shell_overhead: usize,
}

/// Identify a single oversized `Value::String` field inside a structured object
/// whose remaining sibling fields are small metadata (`<= 1024` JSON bytes).
fn find_dominant_text_field(map: &serde_json::Map<String, Value>) -> Option<DominantTextField<'_>> {
    let (key, text) = map
        .iter()
        .filter_map(|(k, v)| v.as_str().map(|s| (k.as_str(), s)))
        .max_by_key(|(k, s)| (s.len(), *k))?;

    if estimate::estimate_bytes(text.len() as u64) <= SPILL_THRESHOLD_TOKENS {
        return None;
    }

    let mut siblings = serde_json::Map::with_capacity(map.len().saturating_sub(1));
    for (k, v) in map {
        if k != key {
            siblings.insert(k.clone(), v.clone());
        }
    }
    let siblings_bytes = serde_json::to_vec(&siblings).ok()?.len();
    if siblings_bytes > MAX_DOMINANT_SIBLINGS_JSON_BYTES {
        return None;
    }

    siblings.insert(key.to_owned(), Value::String(String::new()));
    let shell_overhead = serde_json::to_vec(&siblings).ok()?.len().saturating_sub(2);
    if shell_overhead + 256 > MAX_PREVIEW_JSON_BYTES {
        return None;
    }

    Some(DominantTextField {
        key,
        text,
        shell_overhead,
    })
}

fn extract_text(content: &Value) -> Option<String> {
    match content {
        Value::String(text) => Some(text.clone()),
        Value::Object(_) | Value::Array(_) => {
            Some(serde_json::to_string_pretty(content).unwrap_or_else(|_| content.to_string()))
        }
        Value::Null | Value::Bool(_) | Value::Number(_) => None,
    }
}

fn floor_char_boundary(text: &str, max_bytes: usize) -> usize {
    let mut end = text.len().min(max_bytes);
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    end
}

fn ceil_char_boundary(text: &str, min_index: usize) -> usize {
    let mut start = min_index.min(text.len());
    while start < text.len() && !text.is_char_boundary(start) {
        start += 1;
    }
    start
}

/// Byte end of at most `max_lines` leading lines, bounded by `max_bytes`.
fn head_line_end(text: &str, max_lines: usize, max_bytes: usize) -> usize {
    let mut lines = 0usize;
    for (idx, ch) in text.char_indices() {
        if idx > max_bytes {
            break;
        }
        if ch == '\n' {
            lines += 1;
            if lines >= max_lines {
                return idx;
            }
        }
    }
    floor_char_boundary(text, max_bytes)
}

/// Byte start of at most `max_lines` trailing lines, at or after `min_start`
/// and keeping the slice within `max_bytes`.
fn tail_line_start(text: &str, max_lines: usize, max_bytes: usize, min_start: usize) -> usize {
    let trimmed = text.strip_suffix('\n').unwrap_or(text);
    let byte_floor = text.len().saturating_sub(max_bytes).max(min_start);
    let mut lines = 0usize;
    for (idx, ch) in trimmed.char_indices().rev() {
        if idx < byte_floor {
            break;
        }
        if ch == '\n' {
            lines += 1;
            if lines >= max_lines {
                return idx + 1;
            }
        }
    }
    ceil_char_boundary(text, byte_floor)
}

fn json_string_len(text: &str) -> usize {
    serde_json::to_vec(text).map_or(usize::MAX, |bytes| bytes.len())
}

/// Build `head (<=40 lines) + notice + tail (<=40 lines)` bounded within [`SPILL_PREVIEW_MAX_TOKENS`].
pub(crate) fn build_spill_preview(text: &str, original_tokens: u64, artifact_id: &str) -> String {
    build_spill_preview_within(text, original_tokens, artifact_id, MAX_PREVIEW_JSON_BYTES)
}

/// Build `head (<=40 lines) + notice + tail (<=40 lines)` whose serialized JSON
/// string fits within `max_json_bytes` (and [`SPILL_PREVIEW_MAX_TOKENS`]).
fn build_spill_preview_within(
    text: &str,
    original_tokens: u64,
    artifact_id: &str,
    max_json_bytes: usize,
) -> String {
    let effective_max_json_bytes = max_json_bytes.min(MAX_PREVIEW_JSON_BYTES);
    let notice = format!(
        "\n[... output spilled: {} bytes (~{} tokens); full output: recall id={} ...]\n",
        text.len(),
        original_tokens,
        artifact_id,
    );
    let notice_json = json_string_len(&notice).saturating_sub(2);
    let available = effective_max_json_bytes
        .saturating_sub(2)
        .saturating_sub(notice_json);
    let head_budget = available / 2;
    let tail_budget = available - head_budget;

    let mut head_end = head_line_end(text, SPILL_PREVIEW_HEAD_LINES, head_budget);
    let mut tail_start = tail_line_start(text, SPILL_PREVIEW_TAIL_LINES, tail_budget, head_end);

    loop {
        let candidate = format!("{}{}{}", &text[..head_end], notice, &text[tail_start..]);
        let json_len = json_string_len(&candidate);
        let raw_tokens = estimate::estimate_bytes(candidate.len() as u64);
        if json_len <= effective_max_json_bytes && raw_tokens <= SPILL_PREVIEW_MAX_TOKENS {
            return candidate;
        }
        if head_end == 0 && tail_start >= text.len() {
            return candidate;
        }
        // Trim proportionally when JSON escaping (quotes, newlines, controls)
        // expanded the serialized byte size past the budget.
        let excess = json_len
            .saturating_sub(effective_max_json_bytes)
            .max(candidate.len().saturating_sub(effective_max_json_bytes))
            .max(1);
        let trim_each = (excess / 2).max(16);
        if head_end > 0 {
            head_end = floor_char_boundary(text, head_end.saturating_sub(trim_each));
        }
        if tail_start < text.len() {
            tail_start =
                ceil_char_boundary(text, tail_start.saturating_add(trim_each)).max(head_end);
        }
    }
}
