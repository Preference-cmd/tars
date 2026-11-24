use super::types::{AgentModelConfig, AgentPreset, AgentPresetSource};
use crate::definition::{BuiltinTool, WorkspaceTool};
use crate::events::AgentInteraction;

const EXPLORE_ROLE_PROMPT: &str = "\
You are a read-only exploration subagent inside TARS. \
Use your read-only inspection and search tools to gather precise, evidence-backed findings \
within the workspace and report them concisely without modifying any files.";

/// Returns the compiled-in default agent presets (`explore` and `general`).
pub(super) fn builtin_presets() -> Vec<AgentPreset> {
    vec![
        AgentPreset {
            name: "explore".to_owned(),
            description: "Read-only workspace exploration and code search agent.".to_owned(),
            source: AgentPresetSource::Builtin,
            tools: Some(vec![
                BuiltinTool::Workspace(WorkspaceTool::ReadFile),
                BuiltinTool::Workspace(WorkspaceTool::ListDirectory),
                BuiltinTool::Workspace(WorkspaceTool::SearchText),
                BuiltinTool::Recall,
                BuiltinTool::RunCode,
            ]),
            model: None,
            max_model_rounds: None,
            timeout_secs: None,
            default_interaction: AgentInteraction::SingleWork,
            role_prompt: Some(EXPLORE_ROLE_PROMPT.to_owned()),
            source_file: None,
            sha256: None,
        },
        AgentPreset {
            name: "general".to_owned(),
            description: "General-purpose delegated agent inheriting caller capabilities."
                .to_owned(),
            source: AgentPresetSource::Builtin,
            tools: None,
            model: None,
            max_model_rounds: None,
            timeout_secs: None,
            default_interaction: AgentInteraction::SingleWork,
            role_prompt: None,
            source_file: None,
            sha256: None,
        },
    ]
}

// Keep unused import warning away if AgentModelConfig is referenced by sibling modules.
const _: Option<AgentModelConfig> = None;
