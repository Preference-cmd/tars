use super::types::*;
use crate::application::{ApplicationError, ManagedSession};
use crate::definition::{RenderOptions, SessionDefinition};
use crate::session::WorkRef;
use causa::kernel::{
    BlockContent, BlockId, CacheDirective, ContentPart, Context, ModelRef,
    ToolResultStatus::{Cancelled, TimedOut, UnknownOutcome},
};
use causa::protocol::translation::{
    anthropic::render_anthropic_messages, media::MediaSet,
    openai_chat::render_openai_chat_messages, openai_responses::render_openai_responses_input,
};
use std::collections::{HashMap, HashSet};
use std::io::{self, Write};

pub(super) struct Scan {
    pub diagnosis: RepairDiagnosis,
    pub unpaired: Vec<BlockId>,
    pub fatal: bool,
}

pub(super) fn scan(context: &Context) -> Scan {
    let mut diagnosis = RepairDiagnosis {
        block_count: context.blocks().len(),
        ..Default::default()
    };
    let mut declarations = HashSet::new();
    let mut completed = HashSet::new();
    let mut wires = HashMap::new();
    for block in context.blocks() {
        let id = Some(block.id());
        match block.content() {
            BlockContent::ToolCall(call) => {
                declarations.insert(block.id());
                if call.tool_name.trim().is_empty() || !call.arguments.is_object() {
                    diagnosis.push(
                        RepairIssueKind::InvalidDeclaration,
                        id,
                        "Tool declaration requires a name and object arguments",
                    );
                }
                let wire = block
                    .meta()
                    .provider_call_id
                    .clone()
                    .unwrap_or_else(|| block.id().0.to_string());
                if wire.is_empty() || wires.insert(wire, block.id()).is_some() {
                    diagnosis.push(
                        RepairIssueKind::AmbiguousWireIdentity,
                        id,
                        "Tool wire identity is empty or reused",
                    );
                }
            }
            BlockContent::ToolResult(result) => {
                if !declarations.contains(&result.call_block_id) {
                    diagnosis.push(
                        RepairIssueKind::OrphanResult,
                        id,
                        "Tool result requires an earlier declaration",
                    );
                }
                if !completed.insert(result.call_block_id) {
                    diagnosis.push(
                        RepairIssueKind::DuplicateResult,
                        id,
                        "Multiple results answer the same declaration",
                    );
                }
                if !result.media.is_empty() || result.output.artifact.is_some() {
                    diagnosis.push(
                        RepairIssueKind::UnsupportedReference,
                        id,
                        "Repair cannot resolve tool media or artifacts",
                    );
                }
            }
            // Tool definitions carry no references; their position is checked
            // by the protocol render below.
            BlockContent::ToolChange(_) => {}
            BlockContent::Message { parts, .. } => {
                if parts
                    .iter()
                    .any(|part| matches!(part, ContentPart::Media(_)))
                {
                    diagnosis.push(
                        RepairIssueKind::UnsupportedReference,
                        id,
                        "Repair cannot resolve message media",
                    );
                }
            }
        }
    }
    let fatal = diagnosis.issue_count != 0;
    let mut unpaired = Vec::new();
    for block in context.blocks() {
        if matches!(block.content(), BlockContent::ToolCall(_)) && !completed.contains(&block.id())
        {
            unpaired.push(block.id());
            diagnosis.push(
                RepairIssueKind::UnpairedDeclaration,
                Some(block.id()),
                "Declaration has no result; execution is not inferred",
            );
        }
    }
    diagnosis.unpaired_count = unpaired.len();
    Scan {
        diagnosis,
        unpaired,
        fatal,
    }
}

pub(super) fn render(
    context: &Context,
    definition: &SessionDefinition,
) -> Result<serde_json::Value, String> {
    let frame = context.frame();
    let media = MediaSet::new();
    // Only the materials are checked and no request is sent, so a session
    // without an output limit gets a stand-in: the Anthropic renderer rejects
    // a missing limit, which says nothing about the materials.
    let mut generation = definition.run_options().generation;
    generation.max_tokens = generation.max_tokens.or(Some(1));
    let model = ModelRef::new(&definition.model);
    let options = definition
        .protocol_render_options()
        .map_err(|error| bounded(&error.to_string(), 512))?;
    let res = match options {
        RenderOptions::Chat(options) => render_openai_chat_messages(
            &frame,
            &media,
            &generation,
            &model,
            CacheDirective::None,
            &options,
        ),
        RenderOptions::Responses(options) => render_openai_responses_input(
            &frame,
            &media,
            &generation,
            &model,
            CacheDirective::None,
            &options,
        ),
        RenderOptions::Anthropic(options) => render_anthropic_messages(
            &frame,
            &media,
            &generation,
            &model,
            CacheDirective::None,
            &options,
        ),
    };
    res.map_err(|error| bounded(&error.to_string(), 512))
}

pub(super) fn diagnose(context: &Context, definition: &SessionDefinition) -> RepairDiagnosis {
    diagnose_frame(context, definition, false)
}

fn diagnose_frame(
    context: &Context,
    definition: &SessionDefinition,
    require_frame: bool,
) -> RepairDiagnosis {
    let mut scan = scan(context);
    // Admission examines the old materials before the new input is appended.
    // Empty/text-only histories need no tool-exchange preflight and may be a
    // valid prefix of the next request even when not sendable on their own.
    let has_exchange = context.blocks().iter().any(|block| {
        matches!(
            block.content(),
            BlockContent::ToolCall(_) | BlockContent::ToolResult(_)
        )
    });
    if scan.diagnosis.is_ready()
        && (require_frame || has_exchange)
        && let Err(error) = render(context, definition)
    {
        scan.diagnosis
            .push(RepairIssueKind::ProtocolStructure, None, &error);
    }
    scan.diagnosis
}

pub(super) fn candidate_check(
    context: &Context,
    definition: &SessionDefinition,
) -> Result<(), RepairError> {
    let diagnosis = diagnose_frame(context, definition, true);
    if diagnosis.is_ready() {
        Ok(())
    } else {
        Err(RepairError::Materials(diagnosis))
    }
}

pub(in crate::application) fn admission_check(context: &Context) -> Result<(), RepairError> {
    // New-key admission only needs the linear material pairing check. Full
    // rendering belongs to inspection/repair, not the synchronous input path.
    let diagnosis = scan(context).diagnosis;
    if diagnosis.is_ready() {
        Ok(())
    } else {
        Err(RepairError::Materials(diagnosis))
    }
}

impl ManagedSession {
    /// Inspect idle materials and optionally a retained original batch without claiming it.
    /// Performs local scans/rendering only; save-blocked materials remain inspectable.
    pub fn inspect_repair(
        &self,
        work: Option<&WorkRef>,
    ) -> Result<RepairInspection, ApplicationError> {
        let mut state = self.entry.lock();
        if state.lifecycle == crate::application::Lifecycle::Closed {
            return Err(ApplicationError::Closed);
        }
        let version = self.entry.version(&state);
        let definition = state
            .definition
            .as_ref()
            .expect("published definition")
            .clone();
        let owner = state.owner.as_mut().ok_or(ApplicationError::InstanceGone)?;
        let context = owner.with_context(|context| {
            self.entry.cloned_snapshot();
            context.clone()
        })?;
        let original = work
            .map(|work| {
                owner.inspect_uncommitted(work, |standard, result, batch| {
                    let adoption_error = super::plan::source_check(standard, result, batch).err();
                    let warned = |entry: &causa::kernel::ToolCallEntry| {
                        let (_, result) = entry.result()?;
                        warning_kind(&entry.call().input.tool_name)
                            .filter(|kind| match kind {
                                RepairWarningKind::Command => {
                                    matches!(result.status, UnknownOutcome | TimedOut | Cancelled)
                                }
                                _ => result.status == UnknownOutcome,
                            })
                            .map(|kind| RepairWriteWarning {
                                call_block_id: result.call_block_id,
                                tool_name: entry.call().input.tool_name.clone(),
                                path: extract_call_path(&entry.call().input.arguments),
                                kind,
                            })
                    };
                    let warnings: Vec<_> = batch
                        .map(|b| b.results().iter().filter_map(warned).collect())
                        .unwrap_or_default();
                    let write_warnings_count = warnings.len();
                    let external_warnings_count = warnings
                        .iter()
                        .filter(|warning| warning.kind == RepairWarningKind::External)
                        .count();
                    let command_warnings_count = warnings
                        .iter()
                        .filter(|warning| warning.kind == RepairWarningKind::Command)
                        .count();
                    OriginalBatchPreview {
                        work: work.clone(),
                        available: batch.is_some(),
                        completed_count: batch.map_or(0, |batch| batch.completed_len()),
                        pending_count: batch.map_or(0, |batch| batch.calls().len()),
                        results: batch
                            .into_iter()
                            .flat_map(|batch| batch.results())
                            .take(DISPLAY_LIMIT)
                            .filter_map(|entry| {
                                entry.result().map(|(id, result)| {
                                    let write_warning = warned(entry);
                                    OriginalResultPreview {
                                        result_id: *id,
                                        call_block_id: result.call_block_id,
                                        status: result.status.clone(),
                                        excerpt: json_excerpt(result, 1024),
                                        write_warning,
                                    }
                                })
                            })
                            .collect(),
                        adoption_error,
                        write_warnings_count,
                        external_warnings_count,
                        command_warnings_count,
                    }
                })
            })
            .transpose()?;
        drop(state);
        let diagnosis = diagnose(&context, &definition);
        Ok(RepairInspection {
            version,
            diagnosis,
            original,
        })
    }
}

pub(super) fn bounded(text: &str, limit: usize) -> String {
    let mut end = text.len().min(limit);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_owned()
}

fn json_excerpt(value: &impl serde::Serialize, limit: usize) -> String {
    struct Prefix {
        bytes: Vec<u8>,
        limit: usize,
    }
    impl Write for Prefix {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            let count = bytes.len().min(self.limit.saturating_sub(self.bytes.len()));
            self.bytes.extend_from_slice(&bytes[..count]);
            if count != bytes.len() {
                return Err(io::Error::other("preview bound"));
            }
            Ok(count)
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    let mut output = Prefix {
        bytes: Vec::with_capacity(limit),
        limit,
    };
    let _ = serde_json::to_writer(&mut output, value);
    bounded(&String::from_utf8_lossy(&output.bytes), limit)
}

// The explicit adoption allow-list for original results. New capabilities
// (including MCP) need a separately reviewed policy; adding a workspace tool
// variant must not widen it implicitly.
pub(super) const BUILTIN_TOOL_NAMES: [&str; 6] = [
    "list_directory",
    "read_file",
    "search_text",
    "write_file",
    "edit_file",
    "run_command",
];

pub(super) fn is_builtin_tool(tool_name: &str) -> bool {
    BUILTIN_TOOL_NAMES.contains(&tool_name)
}

/// Which result status makes a warning relevant for its side-effect kind.
pub(super) fn warning_status_applies(
    kind: &RepairWarningKind,
    status: &causa::kernel::ToolResultStatus,
) -> bool {
    match kind {
        RepairWarningKind::FileWrite => *status == UnknownOutcome,
        RepairWarningKind::Command => {
            matches!(*status, UnknownOutcome | TimedOut | Cancelled)
        }
        RepairWarningKind::External => *status == UnknownOutcome,
    }
}

/// Which warning a tool call can carry: built-in writes may have changed a
/// file; every other non-builtin name may have had effects on its own server.
/// Built-in read-only tools carry no warning.
pub(super) fn warning_kind(tool_name: &str) -> Option<RepairWarningKind> {
    match tool_name {
        // The write names must stay a subset of `BUILTIN_TOOL_NAMES`; a
        // built-in write missing from that list would surface as external.
        "write_file" | "edit_file" => Some(RepairWarningKind::FileWrite),
        "run_command" => Some(RepairWarningKind::Command),
        name if BUILTIN_TOOL_NAMES.contains(&name) => None,
        _ => Some(RepairWarningKind::External),
    }
}

pub(super) fn extract_call_path(arguments: &serde_json::Value) -> String {
    arguments
        .get("path")
        .and_then(|v| v.as_str())
        .map(|p| bounded(p, 512))
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::render;
    use crate::definition::{CredentialRef, Provider, SessionDefinition};
    use causa::kernel::{BlockContent, Context, ContextBlock, Role};

    fn definition(render_options: serde_json::Value) -> SessionDefinition {
        SessionDefinition {
            provider: Provider::OpenAiChat,
            model: "fixture-model".into(),
            endpoint: None,
            credential: CredentialRef::Environment("PATH".into()),
            work_timeout_secs: Some(10),
            retained_work_capacity: 1,
            max_model_rounds: 1,
            max_tool_calls: 0,
            max_tokens: Some(7),
            reasoning_effort: None,
            extra_body: Default::default(),
            max_model_attempts: None,
            render_options: render_options.as_object().cloned().unwrap_or_default(),
            workspace_root: None,
            tools: Vec::new(),
            tool_limits: Default::default(),
            write_limits: None,
            permission_mode: None,
            command: None,
            mcp_servers: Vec::new(),
            context_window: None,
            auto_compact: true,
            compact_threshold: 70,
            prompt_cache: true,
        }
    }

    fn context() -> Context {
        Context::from_blocks(vec![ContextBlock::new(
            causa::runtime::new_block_id(),
            BlockContent::text(Role::User, "hello"),
            Default::default(),
        )])
        .unwrap()
    }

    #[test]
    fn preflight_render_uses_the_saved_chat_render_options() {
        let default = render(&context(), &definition(serde_json::json!({}))).unwrap();
        assert_eq!(default["max_completion_tokens"], 7);
        assert!(default.get("max_tokens").is_none());

        let saved = render(
            &context(),
            &definition(serde_json::json!({"max_tokens_field": "max_tokens"})),
        )
        .unwrap();
        assert_eq!(saved["max_tokens"], 7);
        assert!(saved.get("max_completion_tokens").is_none());
    }

    #[test]
    fn preflight_render_uses_the_saved_responses_render_options() {
        let mut default_definition = definition(serde_json::json!({}));
        default_definition.provider = Provider::OpenAiResponses;
        let default = render(&context(), &default_definition).unwrap();
        assert_eq!(default["store"], false);

        let mut saved_definition = default_definition;
        saved_definition.render_options = serde_json::json!({"store": true})
            .as_object()
            .cloned()
            .unwrap();
        let saved = render(&context(), &saved_definition).unwrap();
        assert_eq!(saved["store"], true);
    }
}

#[cfg(test)]
mod command_tests {
    use super::*;

    #[test]
    fn run_command_is_builtin_and_carries_a_command_warning() {
        assert!(is_builtin_tool("run_command"));
        assert_eq!(
            warning_kind("run_command"),
            Some(RepairWarningKind::Command)
        );
        let warning = RepairWriteWarning {
            call_block_id: causa::kernel::BlockId::new(uuid::Uuid::now_v7()),
            tool_name: "run_command".into(),
            path: String::new(),
            kind: RepairWarningKind::Command,
        };
        assert!(warning.advice().contains("may have executed"));
        assert!(warning_status_applies(
            &RepairWarningKind::Command,
            &causa::kernel::ToolResultStatus::TimedOut
        ));
        assert!(warning_status_applies(
            &RepairWarningKind::Command,
            &causa::kernel::ToolResultStatus::Cancelled
        ));
        assert!(warning_status_applies(
            &RepairWarningKind::Command,
            &causa::kernel::ToolResultStatus::UnknownOutcome
        ));
        assert!(!warning_status_applies(
            &RepairWarningKind::Command,
            &causa::kernel::ToolResultStatus::Succeeded
        ));
    }
}
