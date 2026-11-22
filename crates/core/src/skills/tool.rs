use super::discovery::{BoundedReadError, collect_resources, read_bounded_file, sha256_hex};
use super::frontmatter::parse_skill_document;
use super::types::{SkillEntry, SkillRegistry, SkillResources};
use crate::definition::BuiltinTool;
use crate::policy::escape_untrusted_text;
use crate::session::ToolReporter;
use async_trait::async_trait;
use causa::kernel::{
    BlockContent, BlockId, BlockMeta, CallControl, ContentPart, ContextBlock, ProcessorContext,
    ProcessorError, Role, Tool, ToolBatch, ToolBatchProcessor, ToolCallContext, ToolDefinition,
    ToolOutput, ToolOutputMeta, ToolResultPayload, ToolResultStatus,
};
use serde::Deserialize;
use serde_json::Value;
use std::collections::HashMap;
use std::fs;
use std::sync::{Arc, RwLock};
use std::time::Duration;
use tokio_util::sync::CancellationToken;

/// `BlockMeta.source` and `ToolOutput.meta.extra["source"]` tag for loaded skill content.
pub const SKILL_SOURCE: &str = "tars:skill";

/// Name of the built-in read-only skill loading tool.
pub(crate) const LOAD_SKILL_TOOL_NAME: &str = BuiltinTool::LoadSkill.name();

/// Hard upper bound in bytes for a single `SKILL.md` read by `load_skill` (64 KiB).
pub(crate) const MAX_LOAD_SKILL_BYTES: usize = 64 * 1024;

const SKILL_FILE_NAME: &str = "SKILL.md";

/// Returns the purely static model-facing [`ToolDefinition`] for `load_skill`.
///
/// The parameter schema deliberately omits any skill list `enum` so adding,
/// editing, or removing skills at runtime does not mutate tool declarations or
/// invalidate provider prompt caches.
pub(crate) fn load_skill_tool_definition() -> ToolDefinition {
    ToolDefinition::new(
        LOAD_SKILL_TOOL_NAME,
        "Load the full instructions for an available skill by its exact name from <available_skills>.",
        serde_json::json!({
            "type": "object",
            "properties": {
                "name": {
                    "type": "string",
                    "description": "Exact skill name from <available_skills>."
                }
            },
            "required": ["name"],
            "additionalProperties": false
        }),
    )
}

/// Snapshot of one loaded skill's active block and content hash in `Context`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct LoadedSkillRecord {
    pub(crate) sha256: String,
    pub(crate) block_id: BlockId,
}

/// Session-shared view of skills currently present (un-summarized) in `Context`.
#[derive(Debug, Default)]
pub(crate) struct LoadedSkillsState {
    entries: RwLock<HashMap<String, LoadedSkillRecord>>,
}

impl LoadedSkillsState {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Rebuilds the view from `blocks`, keeping each skill's newest active block.
    pub(crate) fn sync_from_context(&self, blocks: &[ContextBlock]) {
        let mut next = HashMap::new();
        for block in blocks {
            if let Some((name, sha256)) = parse_standalone_skill_block(block) {
                next.insert(
                    name,
                    LoadedSkillRecord {
                        sha256,
                        block_id: block.id(),
                    },
                );
            } else if let Some((name, sha256, _)) = parse_tool_result_skill(block) {
                next.insert(
                    name,
                    LoadedSkillRecord {
                        sha256,
                        block_id: block.id(),
                    },
                );
            }
        }
        *self.entries.write().expect("loaded skills lock") = next;
    }

    pub(crate) fn get(&self, name: &str) -> Option<LoadedSkillRecord> {
        self.entries
            .read()
            .expect("loaded skills lock")
            .get(name)
            .cloned()
    }

    pub(crate) fn record(
        &self,
        name: impl Into<String>,
        sha256: impl Into<String>,
        block_id: BlockId,
    ) {
        self.entries.write().expect("loaded skills lock").insert(
            name.into(),
            LoadedSkillRecord {
                sha256: sha256.into(),
                block_id,
            },
        );
    }

    /// Atomically checks whether `name` is already recorded with `sha256`.
    ///
    /// Returns `Some(existing)` when the same hash is already present; otherwise
    /// inserts `(sha256, block_id)` and returns `None`.
    pub(crate) fn record_if_changed(
        &self,
        name: &str,
        sha256: &str,
        block_id: BlockId,
    ) -> Option<LoadedSkillRecord> {
        let mut guard = self.entries.write().expect("loaded skills lock");
        if let Some(existing) = guard.get(name)
            && existing.sha256 == sha256
        {
            return Some(existing.clone());
        }
        guard.insert(
            name.to_owned(),
            LoadedSkillRecord {
                sha256: sha256.to_owned(),
                block_id,
            },
        );
        None
    }
}

/// Post-execution processor updating [`LoadedSkillsState`] with confirmed `ToolResult` block IDs.
pub(crate) struct LoadedSkillTrackerProcessor {
    loaded: Arc<LoadedSkillsState>,
}

impl LoadedSkillTrackerProcessor {
    pub(crate) fn new(loaded: Arc<LoadedSkillsState>) -> Self {
        Self { loaded }
    }
}

#[async_trait]
impl ToolBatchProcessor for LoadedSkillTrackerProcessor {
    async fn process(
        &self,
        batch: &mut ToolBatch,
        _ctx: &ProcessorContext<'_>,
    ) -> Result<(), ProcessorError> {
        for entry in batch.results() {
            let Some((result_block_id, payload)) = entry.result() else {
                continue;
            };
            if payload.status != ToolResultStatus::Succeeded {
                continue;
            }
            if let Some((name, sha256, _)) = extract_skill_meta_and_text(&payload.output) {
                self.loaded.record(name, sha256, *result_block_id);
            }
        }
        Ok(())
    }
}

/// Returns `true` when `output` carries `tars:skill` provenance metadata.
pub(crate) fn is_skill_tool_output(output: &ToolOutput) -> bool {
    output
        .meta
        .as_ref()
        .and_then(|meta| meta.extra.as_ref())
        .and_then(|extra| extra.get("source"))
        .and_then(Value::as_str)
        == Some(SKILL_SOURCE)
}

/// Extracts `(name, sha256, raw_text)` from a `load_skill` [`ToolOutput`].
fn extract_skill_meta_and_text(output: &ToolOutput) -> Option<(String, String, &str)> {
    let extra = output.meta.as_ref()?.extra.as_ref()?;
    if extra.get("source").and_then(Value::as_str) != Some(SKILL_SOURCE) {
        return None;
    }
    let name = extra.get("skill").and_then(Value::as_str)?.to_owned();
    let sha256 = extra.get("sha256").and_then(Value::as_str)?.to_owned();
    let text = output.content.as_str()?;
    Some((name, sha256, text))
}

/// Extracts `(name, sha256, promoted_text)` if `block` is a full `load_skill` `ToolResult`.
pub(crate) fn parse_tool_result_skill(block: &ContextBlock) -> Option<(String, String, String)> {
    let BlockContent::ToolResult(result) = block.content() else {
        return None;
    };
    if result.status != ToolResultStatus::Succeeded {
        return None;
    }
    let (name, sha256, raw_text) = extract_skill_meta_and_text(&result.output)?;
    let promoted = promote_skill_content_text(raw_text, &name, &sha256);
    Some((name, sha256, promoted))
}

/// Extracts `(name, sha256)` if `block` is a standalone `Role::User` `tars:skill` block.
pub(crate) fn parse_standalone_skill_block(block: &ContextBlock) -> Option<(String, String)> {
    if block.meta().source.as_deref() != Some(SKILL_SOURCE) {
        return None;
    }
    let BlockContent::Message {
        role: Role::User,
        parts,
    } = block.content()
    else {
        return None;
    };
    let mut text = String::new();
    for part in parts {
        if let ContentPart::Text(payload) = part {
            text.push_str(&payload.0);
        }
    }
    parse_skill_content_header(&text)
}

/// Extracts the skill name if `block` is a standalone `Role::User` `tars:skill` block.
pub fn skill_block_name(block: &ContextBlock) -> Option<String> {
    parse_standalone_skill_block(block).map(|(name, _)| name)
}

/// Builds a standalone `Role::User` `tars:skill` block from promoted `<skill_content>` text.
pub(crate) fn make_standalone_skill_block(promoted_text: String) -> ContextBlock {
    ContextBlock::new(
        BlockId::new(uuid::Uuid::now_v7()),
        BlockContent::text(Role::User, promoted_text),
        BlockMeta::from_source(SKILL_SOURCE),
    )
}

fn promote_skill_content_text(raw_text: &str, name: &str, sha256: &str) -> String {
    let plain_open = format!("<skill_content name=\"{name}\">");
    let full_open = format!("<skill_content name=\"{name}\" sha256=\"{sha256}\">");
    if let Some(rest) = raw_text.strip_prefix(&plain_open) {
        format!("{full_open}{rest}")
    } else if raw_text.starts_with(&full_open) {
        raw_text.to_owned()
    } else {
        format!("{full_open}\n{raw_text}\n</skill_content>")
    }
}

fn parse_skill_content_header(text: &str) -> Option<(String, String)> {
    let first_line = text.lines().next()?.trim();
    let rest = first_line.strip_prefix("<skill_content ")?;
    let attrs = rest.strip_suffix('>')?;
    let name = extract_attr(attrs, "name")?;
    let sha256 = extract_attr(attrs, "sha256")?;
    Some((name.to_owned(), sha256.to_owned()))
}

fn extract_attr<'a>(attrs: &'a str, key: &str) -> Option<&'a str> {
    let needle = format!("{key}=\"");
    let start = attrs.find(&needle)? + needle.len();
    let end = attrs[start..].find('"')? + start;
    Some(&attrs[start..end])
}

fn format_skill_content(name: &str, body: &str, resources: &SkillResources) -> String {
    let mut out = format!("<skill_content name=\"{name}\">\n");
    let clean_body = neutralize_closing_tag(&escape_untrusted_text(body.trim()), "</skill_content");
    if !clean_body.is_empty() {
        out.push_str(&clean_body);
        out.push('\n');
    }
    if !resources.is_empty() {
        out.push_str("\n<skill_resources>\n");
        for path in &resources.scripts {
            out.push_str(&format!("- {}\n", escape_untrusted_text(path)));
        }
        for path in &resources.references {
            out.push_str(&format!("- {}\n", escape_untrusted_text(path)));
        }
        for path in &resources.assets {
            out.push_str(&format!("- {}\n", escape_untrusted_text(path)));
        }
        out.push_str("</skill_resources>\n");
    }
    out.push_str("</skill_content>");
    out
}

fn neutralize_closing_tag(input: &str, needle_lower: &str) -> String {
    let lower = input.to_ascii_lowercase();
    if !lower.contains(needle_lower) {
        return input.to_owned();
    }
    let mut out = String::with_capacity(input.len() + 8);
    let mut pos = 0usize;
    while let Some(rel) = lower[pos..].find(needle_lower) {
        let abs = pos + rel;
        out.push_str(&input[pos..abs]);
        out.push_str("&lt;");
        out.push_str(&input[abs + 1..abs + needle_lower.len()]);
        pos = abs + needle_lower.len();
    }
    out.push_str(&input[pos..]);
    out
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct LoadSkillParams {
    name: String,
}

/// Built-in read-only allowlisted tool loading a skill's `SKILL.md` by exact name.
pub(crate) struct LoadSkillTool {
    registry: Arc<RwLock<SkillRegistry>>,
    loaded: Arc<LoadedSkillsState>,
    reporter: ToolReporter,
    timeout_secs: u64,
}

impl LoadSkillTool {
    pub(crate) fn new(
        registry: Arc<RwLock<SkillRegistry>>,
        loaded: Arc<LoadedSkillsState>,
        reporter: ToolReporter,
        timeout_secs: u64,
    ) -> Self {
        Self {
            registry,
            loaded,
            reporter,
            timeout_secs,
        }
    }

    fn finish(&self, call: &ToolCallContext, output: ToolResultPayload) -> ToolResultPayload {
        self.reporter.returned(call, output.status.clone());
        output
    }
}

fn already_loaded_payload(
    call: &ToolCallContext,
    name: &str,
    sha256: &str,
    block_id: BlockId,
) -> ToolResultPayload {
    let text = format!(
        "Skill '{name}' is already loaded in the current context (block_id: {}, sha256: {sha256}). Refer back to that block in the conversation history instead of reloading.",
        block_id.0
    );
    ToolResultPayload::new(
        call.call_block_id,
        ToolResultStatus::Succeeded,
        ToolOutput::new(Value::String(text)),
    )
}

fn failed(call: &ToolCallContext, error: impl Into<String>) -> ToolResultPayload {
    ToolResultPayload::new(
        call.call_block_id,
        ToolResultStatus::Failed,
        ToolOutput::new(serde_json::json!({ "error": error.into() })),
    )
}

fn stopped(call: &ToolCallContext, control: &CallControl) -> ToolResultPayload {
    let status = if control.is_cancelled() {
        ToolResultStatus::Cancelled
    } else {
        ToolResultStatus::TimedOut
    };
    ToolResultPayload::new(
        call.call_block_id,
        status,
        ToolOutput::new(serde_json::json!({
            "error": "operation stopped before obtaining a result"
        })),
    )
}

enum LoadOutcome {
    Loaded {
        name: String,
        sha256: String,
        text: String,
    },
    AlreadyLoaded {
        name: String,
        sha256: String,
        block_id: BlockId,
    },
    Failed(String),
    Stopped,
}

struct CancelOnDrop(CancellationToken);

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

fn read_and_verify_skill(
    entry: &SkillEntry,
    loaded: &LoadedSkillsState,
    stopped: impl Fn() -> bool,
) -> LoadOutcome {
    if stopped() {
        return LoadOutcome::Stopped;
    }

    let canon_dir = match fs::canonicalize(&entry.dir) {
        Ok(path) => path,
        Err(error) => {
            return LoadOutcome::Failed(format!(
                "skill '{}' directory is unavailable: {}",
                entry.name,
                error.kind()
            ));
        }
    };
    if canon_dir != entry.dir || !fs::metadata(&canon_dir).is_ok_and(|m| m.is_dir()) {
        return LoadOutcome::Failed(format!(
            "skill '{}' directory changed or escaped its canonical root",
            entry.name
        ));
    }

    let candidate = canon_dir.join(SKILL_FILE_NAME);
    let canon_skill_md = match fs::canonicalize(&candidate) {
        Ok(path) => path,
        Err(error) => {
            return LoadOutcome::Failed(format!(
                "skill '{}' SKILL.md is unavailable: {}",
                entry.name,
                error.kind()
            ));
        }
    };
    if !canon_skill_md.starts_with(&canon_dir) {
        return LoadOutcome::Failed(format!(
            "skill '{}' SKILL.md symlink resolves outside its skill directory",
            entry.name
        ));
    }
    if !fs::metadata(&canon_skill_md).is_ok_and(|m| m.is_file()) {
        return LoadOutcome::Failed(format!(
            "skill '{}' SKILL.md is not a regular file",
            entry.name
        ));
    }

    if stopped() {
        return LoadOutcome::Stopped;
    }

    let bytes = match read_bounded_file(&canon_skill_md, MAX_LOAD_SKILL_BYTES) {
        Ok(bytes) => bytes,
        Err(BoundedReadError::TooLarge) => {
            return LoadOutcome::Failed(format!(
                "skill '{}' SKILL.md exceeds {MAX_LOAD_SKILL_BYTES} bytes",
                entry.name
            ));
        }
        Err(BoundedReadError::Io(error)) => {
            return LoadOutcome::Failed(format!(
                "failed to read skill '{}' SKILL.md: {}",
                entry.name,
                error.kind()
            ));
        }
    };

    let sha256 = sha256_hex(&bytes);
    if let Some(existing) = loaded.get(&entry.name)
        && existing.sha256 == sha256
    {
        return LoadOutcome::AlreadyLoaded {
            name: entry.name.clone(),
            sha256,
            block_id: existing.block_id,
        };
    }

    let text = match std::str::from_utf8(&bytes) {
        Ok(text) => text,
        Err(_) => {
            return LoadOutcome::Failed(format!(
                "skill '{}' SKILL.md is not valid UTF-8",
                entry.name
            ));
        }
    };

    let parsed = match parse_skill_document(text, &entry.name) {
        Ok(parsed) => parsed,
        Err((_, msg)) => {
            return LoadOutcome::Failed(format!(
                "skill '{}' SKILL.md is invalid: {msg}",
                entry.name
            ));
        }
    };

    let mut _diagnostics = Vec::new();
    let resources = collect_resources(&canon_dir, &mut _diagnostics);
    let formatted = format_skill_content(&entry.name, parsed.body, &resources);

    LoadOutcome::Loaded {
        name: entry.name.clone(),
        sha256,
        text: formatted,
    }
}

#[async_trait]
impl Tool for LoadSkillTool {
    fn definition(&self) -> ToolDefinition {
        load_skill_tool_definition()
    }

    async fn execute(
        &self,
        call: &ToolCallContext,
        control: &CallControl,
        _store: Option<&dyn causa::kernel::ArtifactStore>,
    ) -> ToolResultPayload {
        if control.check().is_err() {
            return self.finish(call, stopped(call, control));
        }
        let params: LoadSkillParams = match serde_json::from_value(call.input.arguments.clone()) {
            Ok(params) => params,
            Err(error) => {
                return self.finish(
                    call,
                    failed(call, format!("invalid load_skill parameters: {error}")),
                );
            }
        };

        let (entry, available) = {
            let guard = self.registry.read().expect("skill registry lock");
            let entry = guard.get(&params.name).cloned();
            let available: Vec<String> = guard.names().map(str::to_owned).collect();
            (entry, available)
        };

        let Some(entry) = entry else {
            let msg = if available.is_empty() {
                format!(
                    "unknown skill '{}'; no skills are currently available",
                    params.name
                )
            } else {
                format!(
                    "unknown skill '{}'; available skills: {}",
                    params.name,
                    available.join(", ")
                )
            };
            return self.finish(call, failed(call, msg));
        };

        let control = control.with_timeout(Duration::from_secs(self.timeout_secs));
        self.reporter.waiting(call);
        let dropped = CancellationToken::new();
        let _cancel = CancelOnDrop(dropped.clone());
        let worker_control = control.clone();
        let worker_dropped = dropped.clone();
        let loaded = self.loaded.clone();
        let reporter = self.reporter.clone();
        let call_owned = call.clone();
        let task = self.reporter.spawn_blocking(move || {
            let stopped_fn = || worker_dropped.is_cancelled() || worker_control.check().is_err();
            if stopped_fn() {
                return LoadOutcome::Stopped;
            }
            reporter.running(&call_owned);
            read_and_verify_skill(&entry, &loaded, stopped_fn)
        });

        let deadline = async {
            match control.deadline() {
                Some(deadline) => tokio::time::sleep_until(deadline.into()).await,
                None => std::future::pending::<()>().await,
            }
        };
        let payload = tokio::select! {
            biased;
            _ = control.cancellation_token().cancelled() => stopped(call, &control),
            _ = deadline => stopped(call, &control),
            result = task => match result {
                Ok(LoadOutcome::Loaded { name, sha256, text }) => {
                    // Atomically claim the load under write lock so concurrent calls for
                    // the same skill and hash in one batch deduplicate without racing;
                    // `LoadedSkillTrackerProcessor` and `sync_from_context` refine the
                    // provisional `call_block_id` to the committed `ToolResult` block ID.
                    if let Some(existing) =
                        self.loaded
                            .record_if_changed(&name, &sha256, call.call_block_id)
                    {
                        already_loaded_payload(call, &name, &sha256, existing.block_id)
                    } else {
                        let mut output = ToolOutput::new(Value::String(text));
                        let mut meta = ToolOutputMeta::default();
                        meta.extra = Some(serde_json::json!({
                            "source": SKILL_SOURCE,
                            "skill": name,
                            "sha256": sha256,
                        }));
                        output.meta = Some(meta);
                        ToolResultPayload::new(
                            call.call_block_id,
                            ToolResultStatus::Succeeded,
                            output,
                        )
                    }
                }
                Ok(LoadOutcome::AlreadyLoaded {
                    name,
                    sha256,
                    block_id,
                }) => already_loaded_payload(call, &name, &sha256, block_id),
                Ok(LoadOutcome::Failed(error)) => failed(call, error),
                Ok(LoadOutcome::Stopped) => stopped(call, &control),
                Err(_) => failed(call, "load_skill worker thread panicked"),
            },
        };
        self.finish(call, payload)
    }
}
