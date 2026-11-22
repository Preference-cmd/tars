//! Multi-directory skill discovery and `agentskills.io` frontmatter validation.
//!
//! Skills are discovered from ordered skill directories (later directories
//! override earlier ones) with an optional workspace `.tars/skills` directory
//! taking highest precedence. Only `SKILL.md` files inside first-level
//! subdirectories are read; optional `scripts/`, `references/`, and `assets/`
//! subtrees are listed by relative path without reading their file contents.

mod discovery;
mod frontmatter;
mod tool;
mod types;

#[cfg(test)]
mod tests;

pub use discovery::MAX_SKILL_FILE_BYTES;
pub use frontmatter::{ParsedSkillDocument, SkillFrontmatter, parse_skill_document};
#[cfg(test)]
pub(crate) use tool::LOAD_SKILL_TOOL_NAME;
pub(crate) use tool::{
    LoadSkillTool, LoadedSkillTrackerProcessor, LoadedSkillsState, is_skill_tool_output,
    load_skill_tool_definition, make_standalone_skill_block, parse_standalone_skill_block,
    parse_tool_result_skill,
};
pub use tool::{SKILL_SOURCE, skill_block_name};
pub use types::{
    SkillDiagnostic, SkillDiagnosticKind, SkillDiscovery, SkillEntry, SkillRegistry, SkillResources,
};
