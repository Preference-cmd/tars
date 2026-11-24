//! Unified session prelude pipeline (static system contract, dynamic sections,
//! unified budget tightening, and incremental dynamic diff updates).

mod budget;
mod contract;
mod engine;
mod types;

#[cfg(test)]
mod tests;

pub use budget::{
    AGENTS_SOFT_CAP_BYTES, DEFAULT_TOTAL_BUDGET_BYTES, DYNAMIC_CUSTOM_SOFT_CAP_BYTES,
    ENV_SOFT_CAP_BYTES, INSTRUCTIONS_SOFT_CAP_BYTES, SKILLS_SOFT_CAP_BYTES, effective_budget_bytes,
};
pub use contract::{
    BASE_HOST_CONTRACT, DynamicSection, PRELUDE_AGENTS_SOURCE, PRELUDE_BASE_SOURCE,
    PRELUDE_DYNAMIC_CUSTOM_SOURCE, PRELUDE_ENV_SOURCE, PRELUDE_SKILLS_SOURCE,
    PRELUDE_SOURCE_PREFIX, PRELUDE_STATIC_CUSTOM_SOURCE, PRELUDE_UPDATE_PREFIX,
    PRELUDE_USER_INSTRUCTIONS_SOURCE, PRELUDE_WORKSPACE_INSTRUCTIONS_SOURCE,
    dynamic_section_from_prefix_source, dynamic_section_from_update_source, has_prelude,
    is_any_prelude_source, is_prelude_prefix_source, is_prelude_update_source,
};
pub use engine::{build_initial_prelude, effective_section_contents, evaluate_dynamic_updates};
pub(crate) use engine::{
    build_initial_prelude_with_registries, discover_skills_for_prelude,
    evaluate_dynamic_updates_with_registries, fold_dynamic_prefix,
};
pub use types::{
    CapabilityCategory, DynamicCustomSlot, DynamicUpdate, InitialPrelude, PreludeBudgetConfig,
    PreludeDefinition, PreludeDiagnostic, PreludeDiagnosticKind, PreludeError, PreludeRuntimeState,
    StaticCustomSlot,
};
