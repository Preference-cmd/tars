use super::inspect::{
    candidate_check, diagnose, extract_call_path, is_builtin_tool, scan, warning_kind,
    warning_status_applies,
};
use super::types::*;
use crate::application::{ApplicationError, ManagedSession, MaterialVersion};
use crate::definition::SessionDefinition;
use crate::session::{SessionError, WorkResult};
use causa::kernel::{
    BlockContent, BlockId, BlockMeta, Context, ContextBlock, Role, ToolBatch, ToolCallPayload,
};
use causa::runtime::{TurnInterruption, TurnResult, new_block_id};
use std::collections::{HashMap, HashSet};

pub(super) fn source_check(
    standard: bool,
    result: &WorkResult,
    batch: Option<&ToolBatch>,
) -> Result<(), RepairError> {
    if !standard
        || !matches!(
            result,
            WorkResult::Run(TurnResult::Interrupted {
                cause: TurnInterruption::Cancelled { .. }
                    | TurnInterruption::DeadlineExceeded { .. }
            })
        )
    {
        return Err(RepairError::UnsupportedSource);
    }
    let batch = batch.ok_or(RepairError::OriginalUnavailable)?;
    // MCP and other external calls resolve on their own server, whose side of
    // a cancellation is not controlled here, so their results are never adopted.
    if batch
        .results()
        .iter()
        .chain(batch.calls())
        .any(|entry| !is_builtin_tool(&entry.call().input.tool_name))
    {
        return Err(RepairError::NonBuiltinCalls);
    }
    batch.validate().map_err(|_| RepairError::InvalidOriginal)
}

// This strict subset covers the standard runner's atomic result append. It
// never rearranges a middle exchange or borrows results from another batch.
fn validate_original(
    context: &Context,
    definition: &SessionDefinition,
    batch: &ToolBatch,
    unpaired: &[BlockId],
) -> Result<(), RepairError> {
    let declarations: HashMap<_, _> = context
        .blocks()
        .iter()
        .filter_map(|block| {
            if let BlockContent::ToolCall(call) = block.content() {
                Some((block.id(), call))
            } else {
                None
            }
        })
        .collect();
    let ids: HashSet<_> = batch.declaration_ids().into_iter().collect();
    if ids.is_empty() || ids.len() != unpaired.len() || unpaired.iter().any(|id| !ids.contains(id))
    {
        return Err(RepairError::InvalidOriginal);
    }
    let first = context
        .blocks()
        .iter()
        .position(|block| ids.contains(&block.id()))
        .ok_or(RepairError::InvalidOriginal)?;
    if context.blocks()[first..].iter().any(|block| {
        !matches!(block.content(), BlockContent::ToolCall(_)) || !ids.contains(&block.id())
    }) {
        return Err(RepairError::InvalidOriginal);
    }
    for entry in batch.results().iter().chain(batch.calls()) {
        let call = entry.call();
        if declarations.get(&call.call_block_id).copied() != Some(&call.input)
            || !call.result_notes.is_empty()
            // The allow-list in `is_builtin_tool` is the reviewed adoption
            // scope; extending WorkspaceTool alone cannot grant result adoption.
            || !is_builtin_tool(&call.input.tool_name)
            || !definition.tools.iter().any(|tool| tool.name() == call.input.tool_name)
        {
            return Err(RepairError::InvalidOriginal);
        }
    }
    Ok(())
}

impl ManagedSession {
    /// Prepare an immutable, locally validated candidate without consuming a batch.
    /// This copies materials and may render them; call off the interactive draw path.
    pub fn plan_repair(&self, mode: RepairMode) -> Result<RepairPlan, ApplicationError> {
        let mut state = self.entry.lock();
        self.entry.new_mutation(&state)?;
        let base = self.entry.version(&state);
        let definition = state
            .definition
            .as_ref()
            .expect("published definition")
            .clone();
        let owner = state.owner.as_mut().expect("published owner");
        let claims_revision = owner.claims_revision();
        let context = owner.with_context(|context| {
            self.entry.cloned_snapshot();
            context.clone()
        })?;
        let scan = scan(&context);
        if scan.fatal {
            return Err(RepairError::Materials(scan.diagnosis).into());
        }
        if scan.unpaired.is_empty() {
            drop(state);
            let diagnosis = diagnose(&context, &definition);
            if !diagnosis.is_ready() {
                return Err(RepairError::Materials(diagnosis).into());
            }
            return Err(RepairError::NotNeeded.into());
        }
        let results = match &mode {
            RepairMode::RemoveUnpaired => HashMap::new(),
            RepairMode::AdoptOriginal(work) => {
                owner.inspect_uncommitted(work, |standard, result, batch| {
                    source_check(standard, result, batch)?;
                    let batch = batch.expect("checked original");
                    validate_original(&context, &definition, batch, &scan.unpaired)?;
                    Ok::<_, RepairError>(
                        batch
                            .results()
                            .iter()
                            .map(|entry| {
                                let (id, payload) = entry.result().expect("validated result");
                                (
                                    payload.call_block_id,
                                    ContextBlock::new(
                                        *id,
                                        BlockContent::ToolResult(payload.clone()),
                                        BlockMeta::default(),
                                    ),
                                )
                            })
                            .collect::<HashMap<_, _>>(),
                    )
                })??
            }
        };
        let declarations: HashMap<BlockId, ToolCallPayload> = context
            .blocks()
            .iter()
            .filter_map(|block| {
                if let BlockContent::ToolCall(call) = block.content() {
                    Some((block.id(), call.clone()))
                } else {
                    None
                }
            })
            .collect();
        // A removed or adopted-unknown declaration is flagged when its tool can
        // have had a side effect before the interruption: built-in writes for
        // the workspace file, any non-builtin name for its own server. Removal
        // flags every removed declaration; adoption only unknown outcomes.
        let require_unknown = matches!(&mode, RepairMode::AdoptOriginal(_));
        let warnings: Vec<RepairWriteWarning> = scan
            .unpaired
            .iter()
            .filter_map(|id| {
                let call = declarations.get(id)?;
                let kind = warning_kind(&call.tool_name)?;
                if require_unknown {
                    let Some(BlockContent::ToolResult(payload)) =
                        results.get(id).map(|block| block.content())
                    else {
                        return None;
                    };
                    if !warning_status_applies(&kind, &payload.status) {
                        return None;
                    }
                }
                Some(RepairWriteWarning {
                    call_block_id: *id,
                    tool_name: call.tool_name.clone(),
                    path: extract_call_path(&call.arguments),
                    kind,
                })
            })
            .collect();
        let write_warnings_count = warnings.len();
        let external_warnings_count = warnings
            .iter()
            .filter(|warning| warning.kind == RepairWarningKind::External)
            .count();
        let command_warnings_count = warnings
            .iter()
            .filter(|warning| warning.kind == RepairWarningKind::Command)
            .count();
        let write_warnings: Vec<RepairWriteWarning> =
            warnings.into_iter().take(DISPLAY_LIMIT).collect();
        // Sorting, explanation construction and protocol rendering need no
        // coordination lock. The eventual apply rechecks this exact baseline.
        drop(state);
        let removed: Vec<_> = scan
            .unpaired
            .iter()
            .filter(|id| !results.contains_key(id))
            .copied()
            .collect();
        let removed_set: HashSet<_> = removed.iter().copied().collect();
        let original_blocks = context.blocks().len();
        let mut blocks: Vec<_> = context
            .into_blocks()
            .into_iter()
            .filter(|block| !removed_set.contains(&block.id()))
            .collect();
        // Pending-suffix partition swaps completion order. Restore declaration
        // order when appending original results, without changing any payload.
        let mut results = results;
        let mut adopted = Vec::new();
        for id in &scan.unpaired {
            if let Some(result) = results.remove(id) {
                adopted.push(result.id());
                blocks.push(result);
            }
        }
        let explanation_id = new_block_id();
        let explanation = serde_json::json!({
            "application": "tars", "action": "material_repair",
            "removed_declaration_ids": removed, "adopted_original_result_ids": adopted,
            "meaning": "Declarations were removed only from future request materials. This does not establish whether operations occurred. Original work states and result statuses remain unchanged. No old work was resumed or executed.",
        });
        blocks.push(ContextBlock::new(
            explanation_id,
            BlockContent::text(Role::System, explanation.to_string()),
            BlockMeta::from_source("application:tars:material-repair"),
        ));
        let candidate = Context::from_blocks(blocks)
            .map_err(|failure| SessionError::ContextEdit(failure.reason))?;
        candidate_check(&candidate, &definition)?;
        let summary = RepairSummary {
            original_blocks,
            candidate_blocks: candidate.blocks().len(),
            removed_count: removed.len(),
            removed: removed.into_iter().take(DISPLAY_LIMIT).collect(),
            adopted_count: adopted.len(),
            adopted: adopted.into_iter().take(DISPLAY_LIMIT).collect(),
            explanation_id,
            write_warnings_count,
            external_warnings_count,
            command_warnings_count,
            write_warnings,
        };
        Ok(RepairPlan {
            base,
            claims_revision,
            mode,
            candidate,
            summary,
        })
    }

    /// Apply exactly the previewed candidate under the shared mutation/save gate.
    /// Does not start work, claim/forget original results, or wait for persistence.
    /// Call `wait_saved` before sending a new work to a persistent instance.
    pub fn apply_repair(&self, plan: &RepairPlan) -> Result<MaterialVersion, ApplicationError> {
        let candidate = plan.candidate.clone();
        let mut state = self.entry.lock();
        if plan.base.target != self.entry.target {
            return Err(ApplicationError::StaleInstance);
        }
        if plan.base != self.entry.version(&state) {
            return Err(ApplicationError::StaleMaterial);
        }
        self.entry.new_mutation(&state)?;
        let owner = state.owner.as_mut().expect("published owner");
        if plan.claims_revision != owner.claims_revision() {
            return Err(RepairError::StaleEvidence.into());
        }
        if let RepairMode::AdoptOriginal(work) = &plan.mode {
            // The original batch is immutable until claimed; the revision above
            // also makes any successful claim/forget invalidate every old plan.
            owner.inspect_uncommitted(work, source_check)??;
        }
        // Both the validated candidate and this instance's protocol definition
        // are immutable. No repeat rendering is needed while holding the gate.
        self.entry.cloned_snapshot();
        self.commit_materials(&mut state, candidate)
    }
}
