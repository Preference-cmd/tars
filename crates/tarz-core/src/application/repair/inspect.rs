use super::types::*;
use crate::application::{ApplicationError, ManagedSession};
use crate::definition::{Provider, SessionDefinition};
use crate::session::WorkRef;
use causa::kernel::{
    BlockContent, BlockId, CacheDirective, ContentPart, Context, ModelRef, ToolResultStatus,
};
use causa::protocol::translation::{
    anthropic::render_anthropic_messages,
    media::MediaSet,
    openai_chat::render_openai_chat_messages,
    openai_responses::render_openai_responses_input,
    render_options::{AnthropicRenderOptions, ChatRenderOptions, ResponsesRenderOptions},
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

pub(super) fn render(context: &Context, definition: &SessionDefinition) -> Result<(), String> {
    let frame = context.frame();
    let media = MediaSet::new();
    // Only the materials are checked and no request is sent, so a session
    // without an output limit gets a stand-in: the Anthropic renderer rejects
    // a missing limit, which says nothing about the materials.
    let mut generation = definition.run_options().generation;
    generation.max_tokens = generation.max_tokens.or(Some(1));
    let model = ModelRef::new(&definition.model);
    let res = match definition.provider {
        Provider::OpenAiChat => render_openai_chat_messages(
            &frame,
            &media,
            &generation,
            &model,
            CacheDirective::None,
            &ChatRenderOptions::default(),
        ),
        Provider::OpenAiResponses => render_openai_responses_input(
            &frame,
            &media,
            &generation,
            &model,
            CacheDirective::None,
            &ResponsesRenderOptions::default(),
        ),
        Provider::Anthropic => render_anthropic_messages(
            &frame,
            &media,
            &generation,
            &model,
            CacheDirective::None,
            &AnthropicRenderOptions::default(),
        ),
    };
    res.map(|_| ())
        .map_err(|error| bounded(&error.to_string(), 512))
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
                    let write_warnings_count = batch
                        .map(|b| {
                            b.results()
                                .iter()
                                .filter(|entry| {
                                    is_write_tool(&entry.call().input.tool_name)
                                        && entry.result().is_some_and(|(_, r)| {
                                            r.status == ToolResultStatus::UnknownOutcome
                                        })
                                })
                                .count()
                        })
                        .unwrap_or(0);
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
                                    let write_warning =
                                        if is_write_tool(&entry.call().input.tool_name)
                                            && result.status == ToolResultStatus::UnknownOutcome
                                        {
                                            Some(RepairWriteWarning {
                                                call_block_id: result.call_block_id,
                                                tool_name: entry.call().input.tool_name.clone(),
                                                path: extract_call_path(
                                                    &entry.call().input.arguments,
                                                ),
                                            })
                                        } else {
                                            None
                                        };
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

pub(super) fn is_write_tool(tool_name: &str) -> bool {
    matches!(tool_name, "write_file" | "edit_file")
}

pub(super) fn extract_call_path(arguments: &serde_json::Value) -> String {
    arguments
        .get("path")
        .and_then(|v| v.as_str())
        .map(|p| bounded(p, 512))
        .unwrap_or_default()
}
