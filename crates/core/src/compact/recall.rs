//! Built-in `recall` tool reading original material removed by compaction.
//!
//! Provides three call forms:
//! 1. List original blocks of a rewrite record by record ID (`id = "<record id>"`).
//! 2. Read full block text by block reference (`id = "<record id>/<block id>"`).
//! 3. Search original blocks across all records (`query = "..."`).

use super::CompactionRuntime;
use super::block_text::block_to_text;
use super::history::{HistoryRecordEntry, HistoryStore, is_canonical_uuid, parse_artifact_id};
use crate::session::ToolReporter;
use async_trait::async_trait;
use causa::kernel::{
    BlockContent, BlockId, CallControl, ContextBlock, Tool, ToolCallContext, ToolDefinition,
    ToolHints, ToolOutput, ToolResultPayload, ToolResultStatus, Truncation,
};
use serde::Deserialize;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

/// Name of the built-in recall tool advertised to models.
pub(crate) const TOOL_NAME: &str = "recall";

/// Model-facing description explaining what recall does and its three forms.
pub(crate) const TOOL_DESCRIPTION: &str = "\
Read back conversation material that compaction removed. \
id=<record id> lists its original blocks; id=<record id>/<block id> reads one block. \
query searches all records, newest first (literal text, or a regex with regex=true). \
Ids come from brief pointers, listings and search hits.";

/// Maximum output size in bytes (64 KiB).
pub(crate) const MAX_OUTPUT_BYTES: usize = 64 * 1024;

/// Longest accepted regular expression, in characters.
const MAX_REGEX_CHARS: usize = 1_000;
/// Bound on the compiled program and lazy DFA of one regular expression.
const REGEX_SIZE_LIMIT: usize = 1 << 20;

/// Small trait abstraction for string matching in recall search.
pub(crate) trait Matcher: Send + Sync {
    /// Returns 1-based line numbers and matched line slices for each matching line.
    fn find_matches<'a>(&self, text: &'a str) -> Vec<(usize, &'a str)>;
}

/// Literal substring matcher, supporting optional case-sensitivity.
pub(crate) struct LiteralMatcher {
    pattern: String,
    case_sensitive: bool,
}

impl LiteralMatcher {
    pub(crate) fn new(query: &str, case_sensitive: bool) -> Self {
        Self {
            pattern: if case_sensitive {
                query.to_string()
            } else {
                query.to_lowercase()
            },
            case_sensitive,
        }
    }
}

impl Matcher for LiteralMatcher {
    fn find_matches<'a>(&self, text: &'a str) -> Vec<(usize, &'a str)> {
        let mut matches = Vec::new();
        for (idx, line) in text.lines().enumerate() {
            let matched = if self.case_sensitive {
                line.contains(&self.pattern)
            } else {
                line.to_lowercase().contains(&self.pattern)
            };
            if matched {
                matches.push((idx + 1, line));
            }
        }
        matches
    }
}

/// Regular-expression matcher. The `regex` crate matches in linear time, and
/// the size limits bound what a model-supplied pattern can compile to.
pub(crate) struct RegexMatcher {
    regex: regex::Regex,
}

impl RegexMatcher {
    /// Compile `query`, or explain why it is refused.
    pub(crate) fn new(query: &str, case_sensitive: bool) -> Result<Self, String> {
        if query.chars().count() > MAX_REGEX_CHARS {
            return Err(format!(
                "regular expression is longer than {MAX_REGEX_CHARS} characters"
            ));
        }
        regex::RegexBuilder::new(query)
            .case_insensitive(!case_sensitive)
            .size_limit(REGEX_SIZE_LIMIT)
            .dfa_size_limit(REGEX_SIZE_LIMIT)
            .build()
            .map(|regex| Self { regex })
            .map_err(|error| format!("invalid regular expression: {error}"))
    }
}

impl Matcher for RegexMatcher {
    fn find_matches<'a>(&self, text: &'a str) -> Vec<(usize, &'a str)> {
        text.lines()
            .enumerate()
            .filter(|(_, line)| self.regex.is_match(line))
            .map(|(index, line)| (index + 1, line))
            .collect()
    }
}

/// The matcher for one search call.
fn matcher(query: &str, regex: bool, case_sensitive: bool) -> Result<Box<dyn Matcher>, String> {
    if regex {
        Ok(Box::new(RegexMatcher::new(query, case_sensitive)?))
    } else {
        Ok(Box::new(LiteralMatcher::new(query, case_sensitive)))
    }
}

/// Map a Context block to its role and kind labels for recall filtering and display.
pub(crate) fn block_role_and_kind(block: &ContextBlock) -> (&'static str, &'static str) {
    match block.content() {
        BlockContent::Message { role, .. } => {
            let role_str = match role {
                causa::kernel::Role::User => "user",
                causa::kernel::Role::Assistant => "assistant",
                causa::kernel::Role::System => "system",
            };
            (role_str, "message")
        }
        BlockContent::ToolCall(_) => ("assistant", "tool_call"),
        BlockContent::ToolResult(_) => ("tool", "tool_result"),
        BlockContent::ToolChange(_) => ("system", "tool_change"),
    }
}

/// Characters shown in a block preview.
const PREVIEW_CHARS: usize = 200;

/// The first [`PREVIEW_CHARS`] characters of `text`, trimmed, with each run of
/// whitespace collapsed to one space. Stops early instead of copying `text`.
pub(crate) fn preview(text: &str) -> String {
    let mut out = String::new();
    let mut count = 0;
    let mut pending_space = false;
    for c in text.chars() {
        if c.is_whitespace() {
            pending_space = count > 0;
            continue;
        }
        if pending_space {
            if count == PREVIEW_CHARS {
                break;
            }
            out.push(' ');
            count += 1;
            pending_space = false;
        }
        if count == PREVIEW_CHARS {
            break;
        }
        out.push(c);
        count += 1;
    }
    out
}

/// A block's `role/kind` label, e.g. `assistant/tool_call`.
fn label(block: &ContextBlock) -> String {
    let (role, kind) = block_role_and_kind(block);
    format!("{role}/{kind}")
}

/// Bound content bytes to 64 KiB using middle truncation on char boundaries.
pub(crate) fn cap_output(content: String) -> (String, Truncation) {
    if content.len() <= MAX_OUTPUT_BYTES {
        return (content, Truncation::None);
    }
    let base_notice = format!(
        "\n[... truncated: {} bytes omitted; page with offset/limit ...]\n",
        content.len()
    );
    let available = MAX_OUTPUT_BYTES.saturating_sub(base_notice.len());
    let head_target = available / 2;
    let tail_target = available - head_target;

    let mut head_end = 0;
    for (idx, _) in content.char_indices() {
        if idx <= head_target {
            head_end = idx;
        } else {
            break;
        }
    }

    let tail_boundary = content.len().saturating_sub(tail_target);
    let mut tail_start = content.len();
    for (idx, _) in content.char_indices() {
        if idx >= tail_boundary {
            tail_start = idx;
            break;
        }
    }
    if tail_start < head_end {
        tail_start = head_end;
    }

    let omitted = tail_start - head_end;
    let notice =
        format!("\n[... truncated: {omitted} bytes omitted; page with offset/limit ...]\n");
    let truncated = format!(
        "{}{}{}",
        &content[..head_end],
        notice,
        &content[tail_start..]
    );
    (truncated, Truncation::Middle)
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

fn success(call: &ToolCallContext, text: String) -> ToolResultPayload {
    let (capped_text, truncation) = cap_output(text);
    let mut output = ToolOutput::new(serde_json::Value::String(capped_text));
    output.truncation = truncation;
    ToolResultPayload::new(call.call_block_id, ToolResultStatus::Succeeded, output)
}

fn sanitize_store_error(err: &crate::store::StoreError) -> String {
    match err {
        crate::store::StoreError::Io(io) => {
            if io.kind() == std::io::ErrorKind::NotFound {
                "record file not found".to_string()
            } else {
                format!("I/O error: {}", io.kind())
            }
        }
        other => {
            let msg = other.to_string();
            // Ensure no path containing slashes is leaked
            if msg.contains('/') || msg.contains('\\') {
                "record could not be read".to_string()
            } else {
                msg
            }
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RecallParams {
    id: Option<String>,
    query: Option<String>,
    regex: Option<bool>,
    offset: Option<u64>,
    limit: Option<u64>,
    case_sensitive: Option<bool>,
    context: Option<u32>,
    role: Option<String>,
    kind: Option<String>,
}

/// Validated form of one call.
enum Request {
    List {
        record_id: String,
        offset: usize,
        limit: usize,
    },
    Read {
        record_id: String,
        block_id: BlockId,
        offset: usize,
        limit: usize,
    },
    Search {
        matcher: Box<dyn Matcher>,
        context: usize,
        limit: usize,
        role: Option<String>,
        kind: Option<String>,
    },
}

impl Request {
    /// Check the parameters against the live index, or explain the refusal.
    fn parse(params: RecallParams, store: &HistoryStore) -> Result<Self, String> {
        let offset = params.offset.unwrap_or(0).try_into().unwrap_or(usize::MAX);
        match (params.id, params.query) {
            (None, None) | (Some(_), Some(_)) => {
                Err("must provide exactly one of 'id' or 'query'".into())
            }
            (None, Some(query)) => {
                if query.is_empty() {
                    return Err("search query cannot be empty".into());
                }
                let matcher = matcher(
                    &query,
                    params.regex.unwrap_or(false),
                    params.case_sensitive.unwrap_or(false),
                )?;
                let context = params.context.unwrap_or(0);
                if context > 3 {
                    return Err("search context must be between 0 and 3".into());
                }
                let limit = params.limit.unwrap_or(20);
                if !(1..=20).contains(&limit) {
                    return Err("search limit must be between 1 and 20".into());
                }
                if let Some(role) = &params.role
                    && !matches!(role.as_str(), "user" | "assistant" | "system" | "tool")
                {
                    return Err(format!("invalid role filter: {role}"));
                }
                if let Some(kind) = &params.kind
                    && !matches!(
                        kind.as_str(),
                        "message" | "tool_call" | "tool_result" | "tool_change"
                    )
                {
                    return Err(format!("invalid kind filter: {kind}"));
                }
                Ok(Self::Search {
                    matcher,
                    context: context as usize,
                    limit: limit as usize,
                    role: params.role,
                    kind: params.kind,
                })
            }
            (Some(id), None) => {
                if params.regex.is_some() || params.case_sensitive.is_some() {
                    return Err("regex and case_sensitive apply only to a query".into());
                }
                if id.contains('/') {
                    let (record_id, block_id) = parse_artifact_id(&id)?;
                    if !store.has_record(record_id) {
                        return Err(format!(
                            "record {record_id} is not in this session's history index"
                        ));
                    }
                    let limit = params.limit.unwrap_or(12_000);
                    if !(1..=12_000).contains(&limit) {
                        return Err("read limit must be between 1 and 12000".into());
                    }
                    Ok(Self::Read {
                        record_id: record_id.to_string(),
                        block_id,
                        offset,
                        limit: limit as usize,
                    })
                } else {
                    if !is_canonical_uuid(&id) {
                        return Err(format!("record id is not a canonical UUID: {id}"));
                    }
                    if !store.has_record(&id) {
                        return Err(format!(
                            "record {id} is not in this session's history index"
                        ));
                    }
                    let limit = params.limit.unwrap_or(100);
                    if !(1..=200).contains(&limit) {
                        return Err("list limit must be between 1 and 200".into());
                    }
                    Ok(Self::List {
                        record_id: id,
                        offset,
                        limit: limit as usize,
                    })
                }
            }
        }
    }
}

/// Stop signal seen by the blocking worker: the call's own control, or the
/// tool future being dropped.
struct WorkerStop {
    call: CallControl,
    dropped: CancellationToken,
}

impl WorkerStop {
    fn stopped(&self) -> bool {
        self.dropped.is_cancelled() || self.call.check().is_err()
    }
}

/// Cancels the worker when the tool future returns or is dropped.
struct CancelOnDrop(CancellationToken);

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

/// What the worker produced: text for a successful result, a refusal, or a stop.
enum Outcome {
    Text(String),
    Failed(String),
    Stopped,
}

/// Everything one blocking worker needs.
struct Worker {
    store: Arc<HistoryStore>,
    entries: Vec<HistoryRecordEntry>,
    /// Inserted block id → id of the record that inserted it.
    inserted: HashMap<BlockId, String>,
    stop: WorkerStop,
}

impl Worker {
    fn run(&self, request: Request) -> Outcome {
        if self.stop.stopped() {
            return Outcome::Stopped;
        }
        match request {
            Request::List {
                record_id,
                offset,
                limit,
            } => self.list(&record_id, offset, limit),
            Request::Read {
                record_id,
                block_id,
                offset,
                limit,
            } => self.read(&record_id, block_id, offset, limit),
            Request::Search {
                matcher,
                context,
                limit,
                role,
                kind,
            } => self.search(
                matcher.as_ref(),
                context,
                limit,
                role.as_deref(),
                kind.as_deref(),
            ),
        }
    }

    /// The `original:` pointer line for a block an earlier rewrite inserted.
    fn pointer(&self, block_id: BlockId) -> Option<String> {
        self.inserted
            .get(&block_id)
            .map(|record_id| format!("original: recall id={record_id}"))
    }

    fn list(&self, record_id: &str, offset: usize, limit: usize) -> Outcome {
        let record = match self.store.read(record_id) {
            Ok(record) => record,
            Err(error) => {
                return Outcome::Failed(format!(
                    "history record {record_id} cannot be read: {}",
                    sanitize_store_error(&error)
                ));
            }
        };
        let originals: Vec<&ContextBlock> = record
            .replacements
            .iter()
            .flat_map(|replacement| &replacement.originals)
            .collect();
        let total = originals.len();
        let mut lines = vec![format!(
            "{} {} {} {} ({total} blocks)",
            record.record_id,
            record.kind.name(),
            record.trigger.name(),
            crate::time::format_timestamp_utc(record.created_at_ms),
        )];
        let start = offset.min(total);
        let end = start.saturating_add(limit).min(total);
        for (index, block) in originals[start..end].iter().enumerate() {
            if self.stop.stopped() {
                return Outcome::Stopped;
            }
            let text = block_to_text(block);
            lines.push(format!(
                "{} {} {}/{} ({} chars): {}",
                start + index,
                label(block),
                record.record_id,
                block.id().0,
                text.chars().count(),
                preview(&text)
            ));
            lines.extend(self.pointer(block.id()));
        }
        if end < total {
            lines.push(format!("more: recall id={} offset={end}", record.record_id));
        }
        Outcome::Text(lines.join("\n"))
    }

    fn read(&self, record_id: &str, block_id: BlockId, offset: usize, limit: usize) -> Outcome {
        let block = match self.store.read_original_block(record_id, block_id) {
            Ok(block) => block,
            // Index and membership refusals name ids only.
            Err(error @ crate::store::StoreError::InvalidHistory(_)) => {
                return Outcome::Failed(sanitize_store_error(&error));
            }
            Err(error) => {
                return Outcome::Failed(format!(
                    "history record {record_id} cannot be read: {}",
                    sanitize_store_error(&error)
                ));
            }
        };
        if self.stop.stopped() {
            return Outcome::Stopped;
        }
        let text = block_to_text(&block);
        let total = text.chars().count();
        let start = offset.min(total);
        let end = start.saturating_add(limit).min(total);
        let byte_at = |chars: usize| {
            text.char_indices()
                .nth(chars)
                .map_or(text.len(), |(byte, _)| byte)
        };
        let (start_byte, end_byte) = (byte_at(start), byte_at(end));
        let reference = format!("{record_id}/{}", block_id.0);
        let mut lines = vec![format!(
            "{reference} {} ({total} chars, range {start}..{end})",
            label(&block)
        )];
        lines.extend(self.pointer(block_id));
        lines.push(text[start_byte..end_byte].to_string());
        if end < total {
            lines.push(format!("more: recall id={reference} offset={end}"));
        }
        Outcome::Text(lines.join("\n"))
    }

    fn search(
        &self,
        matcher: &dyn Matcher,
        context: usize,
        limit: usize,
        role: Option<&str>,
        kind: Option<&str>,
    ) -> Outcome {
        let mut hits = Vec::new();
        let mut more_hits = false;
        let mut unreadable = Vec::new();
        // Newest record first; inside a record, last block first.
        'records: for entry in self.entries.iter().rev() {
            if self.stop.stopped() {
                return Outcome::Stopped;
            }
            let record = match self.store.read(&entry.record_id) {
                Ok(record) => record,
                Err(error) => {
                    unreadable.push((entry.record_id.clone(), sanitize_store_error(&error)));
                    continue;
                }
            };
            let originals: Vec<&ContextBlock> = record
                .replacements
                .iter()
                .flat_map(|replacement| &replacement.originals)
                .collect();
            for (index, block) in originals.iter().enumerate().rev() {
                if self.stop.stopped() {
                    return Outcome::Stopped;
                }
                let (block_role, block_kind) = block_role_and_kind(block);
                if role.is_some_and(|role| role != block_role)
                    || kind.is_some_and(|kind| kind != block_kind)
                {
                    continue;
                }
                let text = block_to_text(block);
                let matches = matcher.find_matches(&text);
                if matches.is_empty() {
                    continue;
                }
                if hits.len() >= limit {
                    more_hits = true;
                    break 'records;
                }
                let mut lines = vec![format!(
                    "{}/{} {}",
                    record.record_id,
                    block.id().0,
                    label(block)
                )];
                for (line_number, line) in matches.iter().take(3) {
                    let line: String = line.chars().take(PREVIEW_CHARS).collect();
                    lines.push(format!("L{line_number}: {line}"));
                }
                lines.extend(self.pointer(block.id()));
                if context > 0 {
                    let before = index.saturating_sub(context)..index;
                    let after = (index + 1)..(index + 1 + context).min(originals.len());
                    for (side, range) in [("before", before), ("after", after)] {
                        for neighbour in &originals[range] {
                            lines.push(format!(
                                "  {side}: {}/{} {}: {}",
                                record.record_id,
                                neighbour.id().0,
                                label(neighbour),
                                preview(&block_to_text(neighbour))
                            ));
                        }
                    }
                }
                hits.push(lines.join("\n"));
            }
        }
        let mut output = if hits.is_empty() {
            "no hits found for query".to_string()
        } else {
            let mut text = hits.join("\n\n");
            if more_hits {
                text.push_str("\n\nmore hits exist; suggest narrowing query");
            }
            text
        };
        for (record_id, reason) in unreadable {
            output.push_str(&format!("\nunreadable: {record_id} ({reason})"));
        }
        Outcome::Text(output)
    }
}

/// The built-in recall tool implementation.
pub(crate) struct RecallTool {
    runtime: Arc<CompactionRuntime>,
    reporter: ToolReporter,
    timeout_secs: u64,
    /// Pauses the worker after it reports Running, so tests can stop a call in flight.
    #[cfg(test)]
    pub(crate) gate: std::sync::Mutex<Option<Arc<crate::tools::TestGate>>>,
}

impl RecallTool {
    pub(crate) fn new(
        runtime: Arc<CompactionRuntime>,
        reporter: ToolReporter,
        timeout_secs: u64,
    ) -> Self {
        Self {
            runtime,
            reporter,
            timeout_secs,
            #[cfg(test)]
            gate: std::sync::Mutex::new(None),
        }
    }

    /// The reporter that tracks this tool's blocking workers.
    #[cfg(test)]
    pub(crate) fn reporter(&self) -> &ToolReporter {
        &self.reporter
    }

    fn finish(&self, call: &ToolCallContext, output: ToolResultPayload) -> ToolResultPayload {
        self.reporter.returned(call, output.status.clone());
        output
    }
}

#[async_trait]
impl Tool for RecallTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition::new(
            TOOL_NAME,
            TOOL_DESCRIPTION,
            serde_json::json!({
                "type": "object",
                "properties": {
                    "id": { "type": "string", "description": "Record id, or <record id>/<block id>." },
                    "query": { "type": "string", "description": "Text to search for, per line." },
                    "regex": { "type": "boolean", "description": "Query is a regex. Default false." },
                    "case_sensitive": { "type": "boolean", "description": "Default false." },
                    "offset": { "type": "integer", "minimum": 0, "description": "Blocks to skip in a listing, characters to skip in a read." },
                    "limit": { "type": "integer", "minimum": 1, "description": "Listing: blocks (max 200). Read: characters (max 12000). Search: hits (max 20)." },
                    "context": { "type": "integer", "minimum": 0, "maximum": 3, "description": "Neighbouring blocks to show around each search hit." },
                    "role": { "type": "string", "enum": ["user", "assistant", "system", "tool"] },
                    "kind": { "type": "string", "enum": ["message", "tool_call", "tool_result", "tool_change"] }
                },
                "additionalProperties": false
            }),
        )
    }

    fn hints(&self) -> ToolHints {
        ToolHints::default()
            .with_read_only(true)
            .with_destructive(false)
            .with_idempotent(true)
            .with_open_world(false)
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
        let params: RecallParams = match serde_json::from_value(call.input.arguments.clone()) {
            Ok(params) => params,
            Err(error) => {
                return self.finish(
                    call,
                    failed(call, format!("invalid recall parameters: {error}")),
                );
            }
        };
        let Some(store) = self.runtime.history() else {
            return self.finish(
                call,
                failed(call, "history store is not available for this session"),
            );
        };
        let request = match Request::parse(params, &store) {
            Ok(request) => request,
            Err(error) => return self.finish(call, failed(call, error)),
        };

        // Next-layer pointers come from the live index; no record file is read.
        let entries = self.runtime.entries();
        let mut inserted = HashMap::new();
        for entry in &entries {
            for &block_id in &entry.inserted_block_ids {
                inserted.insert(block_id, entry.record_id.clone());
            }
        }

        let control = control.with_timeout(Duration::from_secs(self.timeout_secs));
        self.reporter.waiting(call);
        let dropped = CancellationToken::new();
        let _cancel = CancelOnDrop(dropped.clone());
        let worker = Worker {
            store,
            entries,
            inserted,
            stop: WorkerStop {
                call: control.clone(),
                dropped,
            },
        };
        let reporter = self.reporter.clone();
        let call_owned = call.clone();
        #[cfg(test)]
        let gate = self.gate.lock().expect("recall gate lock").clone();
        let task = self.reporter.spawn_blocking(move || {
            if worker.stop.stopped() {
                return Outcome::Stopped;
            }
            reporter.running(&call_owned);
            #[cfg(test)]
            if let Some(gate) = gate {
                gate.pause(false);
            }
            worker.run(request)
        });

        let deadline = async {
            match control.deadline() {
                Some(deadline) => tokio::time::sleep_until(deadline.into()).await,
                None => std::future::pending::<()>().await,
            }
        };
        let output = tokio::select! {
            biased;
            _ = control.cancellation_token().cancelled() => stopped(call, &control),
            _ = deadline => stopped(call, &control),
            result = task => match result {
                Ok(Outcome::Text(text)) => success(call, text),
                Ok(Outcome::Failed(error)) => failed(call, error),
                Ok(Outcome::Stopped) => stopped(call, &control),
                Err(_) => failed(call, "recall worker thread panicked"),
            },
        };
        self.finish(call, output)
    }
}
