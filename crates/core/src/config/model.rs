use super::file::ConfigError;
use crate::definition::{
    CodeExecutionSettings, CommandNetwork, CommandSettings, CredentialRef, McpServerConfig,
    Provider, SessionDefinition, Temperature, WorkspaceTool, WorkspaceToolLimits, WriteToolLimits,
};
use crate::policy::PermissionMode;
use crate::prelude::{DynamicCustomSlot, PreludeDefinition, StaticCustomSlot};
use serde::Deserialize;
use std::{
    collections::BTreeMap,
    fs::{self, File},
    io::{self, Read},
    path::{Path, PathBuf},
};

const MAX_PRELUDE_FILE_BYTES: usize = 1024 * 1024;

/// Override for the static custom prompt (`--system` or `--system-file`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StaticCustomOverride {
    /// Inline system prompt text.
    Inline(String),
    /// File path resolved relative to the current working directory.
    File(PathBuf),
}

/// Override for the dynamic custom context (`--dynamic-context` or `--dynamic-context-file`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DynamicCustomOverride {
    /// Inline dynamic context text.
    Inline(String),
    /// File path resolved relative to the current working directory.
    File(PathBuf),
}

/// Entry-point overrides for prelude slots and skill discovery.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PreludeOverrides {
    /// Explicit override for `tars:prelude:static-custom`.
    pub static_custom: Option<StaticCustomOverride>,
    /// Explicit override for `tars:prelude:dynamic-custom`.
    pub dynamic_custom: Option<DynamicCustomOverride>,
    /// Optional replacement list of skill directories (resolved relative to CWD).
    pub skill_dirs: Option<Vec<PathBuf>>,
    /// Additional skill directories appended after configured dirs and before workspace skills.
    pub extra_skill_dirs: Vec<PathBuf>,
    /// Override for whether `<workspace_root>/.tars/skills` is scanned.
    pub include_workspace_skills: Option<bool>,
    /// Override for whether skills discovery and `load_skill` are enabled.
    pub skills_enabled: Option<bool>,
    /// Optional replacement list of agent directories (resolved relative to CWD).
    pub agent_dirs: Option<Vec<PathBuf>>,
    /// Additional agent directories appended after configured dirs and before workspace agents.
    pub extra_agent_dirs: Vec<PathBuf>,
    /// Override for whether `<workspace_root>/.tars/agents` is scanned.
    pub include_workspace_agents: Option<bool>,
    /// Override for whether agent discovery, `<available_agents>`, and collaboration tools are enabled.
    pub agents_enabled: Option<bool>,
}

/// Explicit entry-point choices layered over a selected model preset.
#[derive(Default)]
pub struct DefinitionOverrides {
    /// Explicit wire protocol.
    pub provider: Option<Provider>,
    /// Explicit model identifier.
    pub model: Option<String>,
    /// Explicit full endpoint URL.
    pub endpoint: Option<String>,
    /// Explicit credential environment variable name, never its value.
    pub api_key_env: Option<String>,
    /// Explicit work deadline in seconds, restricted to 1..=86400.
    pub timeout_secs: Option<u64>,
    /// Explicit retained work count.
    pub capacity: Option<usize>,
    /// Explicit model invocation limit.
    pub max_model_rounds: Option<u32>,
    /// Explicit tool declaration limit.
    pub max_tool_calls: Option<u32>,
    /// Explicit output token limit per model call.
    pub max_tokens: Option<u32>,
    /// Explicit model context window in tokens; None leaves it unknown.
    pub context_window: Option<u64>,
    /// Explicit initial automatic-compaction switch.
    pub auto_compact: Option<bool>,
    /// Explicit initial automatic-compaction trigger percentage.
    pub compact_threshold: Option<u8>,
    /// Explicit initial prompt-caching switch.
    pub prompt_cache: Option<bool>,
    /// Explicit reasoning effort, sent to the provider as written.
    pub reasoning_effort: Option<String>,
    /// Explicit sampling temperature in `0.0..=2.0` (`0.0..=1.0` for Anthropic).
    pub temperature: Option<f32>,
    /// Explicit extra request fields; replaces the preset's as a whole.
    pub extra_body: Option<serde_json::Map<String, serde_json::Value>>,
    /// Explicit total model calls per invocation on transient failure.
    pub max_model_attempts: Option<u32>,
    /// Explicit protocol render options; replaces the preset's as a whole.
    pub render_options: Option<serde_json::Map<String, serde_json::Value>>,
    /// Explicit root resolved relative to the current directory at resolution time.
    pub workspace_root: Option<PathBuf>,
    /// Explicit selected tools; Some(empty) disables preset tools.
    pub tools: Option<Vec<WorkspaceTool>>,
    /// Explicit effective tool limits, which may only lower supported bounds.
    pub tool_limits: Option<WorkspaceToolLimits>,
    /// Explicit effective write tool limits, which may only lower supported bounds.
    pub write_limits: Option<WriteToolLimits>,
    /// Explicit command-tool network; other command settings come from the preset.
    pub command_network: Option<CommandNetwork>,
    /// Explicit initial permission mode.
    pub permission_mode: Option<PermissionMode>,
    /// Explicit MCP server ids; Some(empty) disables preset servers.
    pub mcp: Option<Vec<String>>,
    /// Explicit prelude and skills overrides.
    pub prelude: PreludeOverrides,
    /// Explicit switch enabling or disabling read-only `run_code` (`--code-execution` / `--no-code-execution`).
    pub code_execution_enabled: Option<bool>,
    /// Explicit code-execution limits override.
    pub code_execution: Option<CodeExecutionSettings>,
}

#[derive(Clone, Debug, Default)]
pub(super) struct ResolvedPreludeDefaults {
    pub(super) config_dir: Option<PathBuf>,
    pub(super) static_custom: Option<String>,
    pub(super) dynamic_custom: Option<DynamicCustomSlot>,
    pub(super) skill_dirs: Option<Vec<PathBuf>>,
    pub(super) include_workspace_skills: Option<bool>,
    pub(super) agent_dirs: Option<Vec<PathBuf>>,
    pub(super) include_workspace_agents: Option<bool>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Profile {
    provider: Provider,
    model: String,
    endpoint: Option<String>,
    api_key_env: Option<String>,
    timeout_secs: Option<u64>,
    capacity: Option<usize>,
    max_model_rounds: Option<u32>,
    max_tool_calls: Option<u32>,
    max_tokens: Option<u32>,
    context_window: Option<u64>,
    auto_compact: Option<bool>,
    compact_threshold: Option<u8>,
    prompt_cache: Option<bool>,
    reasoning_effort: Option<String>,
    temperature: Option<f32>,
    extra_body: Option<serde_json::Map<String, serde_json::Value>>,
    max_model_attempts: Option<u32>,
    render_options: Option<serde_json::Map<String, serde_json::Value>>,
    pub(super) workspace_root: Option<PathBuf>,
    tools: Option<Vec<WorkspaceTool>>,
    tool_limits: Option<ProfileToolLimits>,
    write_limits: Option<ProfileWriteLimits>,
    command: Option<ProfileCommand>,
    permission_mode: Option<PermissionMode>,
    mcp: Option<Vec<String>>,
    code_execution: Option<ProfileCodeExecution>,
    pub(super) system_prompt: Option<String>,
    pub(super) system_prompt_file: Option<PathBuf>,
    pub(super) dynamic_context: Option<String>,
    pub(super) dynamic_context_file: Option<PathBuf>,
    pub(super) skill_dirs: Option<Vec<PathBuf>>,
    pub(super) include_workspace_skills: Option<bool>,
    pub(super) agent_dirs: Option<Vec<PathBuf>>,
    pub(super) include_workspace_agents: Option<bool>,
    #[serde(skip)]
    pub(super) resolved_system_prompt: Option<String>,
    #[serde(skip)]
    pub(super) resolved_dynamic_custom: Option<DynamicCustomSlot>,
    #[serde(skip)]
    pub(super) resolved_skill_dirs: Option<Vec<PathBuf>>,
    #[serde(skip)]
    pub(super) resolved_agent_dirs: Option<Vec<PathBuf>>,
}

impl Profile {
    pub(super) fn to_model_profile(&self, name: &str) -> crate::agents::ModelProfile {
        crate::agents::ModelProfile {
            name: name.to_owned(),
            provider: self.provider,
            model: self.model.clone(),
            endpoint: self.endpoint.clone(),
            credential: CredentialRef::Environment(self.api_key_env.clone().unwrap_or_else(|| {
                match self.provider {
                    Provider::Anthropic => "ANTHROPIC_API_KEY".into(),
                    _ => "OPENAI_API_KEY".into(),
                }
            })),
            max_tokens: self.max_tokens,
            reasoning_effort: self.reasoning_effort.clone(),
            temperature: self.temperature.and_then(|t| Temperature::new(t).ok()),
            extra_body: self.extra_body.clone().unwrap_or_default(),
            context_window: self.context_window,
            max_model_attempts: self.max_model_attempts,
            render_options: self.render_options.clone().unwrap_or_default(),
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ProfileCommand {
    network: Option<CommandNetwork>,
    default_timeout_secs: Option<u64>,
    max_timeout_secs: Option<u64>,
}
impl ProfileCommand {
    fn effective(&self) -> CommandSettings {
        let defaults = CommandSettings::default();
        CommandSettings {
            network: self.network.unwrap_or(defaults.network),
            default_timeout_secs: self
                .default_timeout_secs
                .unwrap_or(defaults.default_timeout_secs),
            max_timeout_secs: self.max_timeout_secs.unwrap_or(defaults.max_timeout_secs),
        }
    }
}

// Sparse startup defaults are deliberately separate from the complete persisted limits.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ProfileToolLimits {
    max_concurrent_operations: Option<usize>,
    call_timeout_secs: Option<u64>,
    max_output_bytes: Option<usize>,
    max_directory_entries: Option<usize>,
    max_file_scan_bytes: Option<usize>,
    max_search_files: Option<usize>,
    max_search_depth: Option<usize>,
    max_search_scan_bytes: Option<usize>,
    max_search_matches: Option<usize>,
}
impl ProfileToolLimits {
    fn effective(&self) -> WorkspaceToolLimits {
        let defaults = WorkspaceToolLimits::default();
        WorkspaceToolLimits {
            max_concurrent_operations: self
                .max_concurrent_operations
                .unwrap_or(defaults.max_concurrent_operations),
            call_timeout_secs: self.call_timeout_secs.unwrap_or(defaults.call_timeout_secs),
            max_output_bytes: self.max_output_bytes.unwrap_or(defaults.max_output_bytes),
            max_directory_entries: self
                .max_directory_entries
                .unwrap_or(defaults.max_directory_entries),
            max_file_scan_bytes: self
                .max_file_scan_bytes
                .unwrap_or(defaults.max_file_scan_bytes),
            max_search_files: self.max_search_files.unwrap_or(defaults.max_search_files),
            max_search_depth: self.max_search_depth.unwrap_or(defaults.max_search_depth),
            max_search_scan_bytes: self
                .max_search_scan_bytes
                .unwrap_or(defaults.max_search_scan_bytes),
            max_search_matches: self
                .max_search_matches
                .unwrap_or(defaults.max_search_matches),
        }
    }
}
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ProfileWriteLimits {
    max_edit_file_bytes: Option<usize>,
    max_write_bytes: Option<usize>,
    max_edit_segments: Option<usize>,
}
impl ProfileWriteLimits {
    fn effective(&self) -> WriteToolLimits {
        let defaults = WriteToolLimits::default();
        WriteToolLimits {
            max_edit_file_bytes: self
                .max_edit_file_bytes
                .unwrap_or(defaults.max_edit_file_bytes),
            max_write_bytes: self.max_write_bytes.unwrap_or(defaults.max_write_bytes),
            max_edit_segments: self.max_edit_segments.unwrap_or(defaults.max_edit_segments),
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ProfileCodeExecution {
    pub(super) enabled: Option<bool>,
    pub(super) timeout_secs: Option<u64>,
    pub(super) memory_limit_bytes: Option<usize>,
    pub(super) stack_limit_bytes: Option<usize>,
    pub(super) max_subcalls: Option<usize>,
    pub(super) max_concurrency: Option<usize>,
    pub(super) max_output_bytes: Option<usize>,
}

impl ProfileCodeExecution {
    pub(super) fn effective_over(&self, base: CodeExecutionSettings) -> CodeExecutionSettings {
        CodeExecutionSettings {
            timeout_secs: self.timeout_secs.unwrap_or(base.timeout_secs),
            memory_limit_bytes: self.memory_limit_bytes.unwrap_or(base.memory_limit_bytes),
            stack_limit_bytes: self.stack_limit_bytes.unwrap_or(base.stack_limit_bytes),
            max_subcalls: self.max_subcalls.unwrap_or(base.max_subcalls),
            max_concurrency: self.max_concurrency.unwrap_or(base.max_concurrency),
            max_output_bytes: self.max_output_bytes.unwrap_or(base.max_output_bytes),
        }
    }
}

pub(super) fn resolve(
    profile: Option<&Profile>,
    options: DefinitionOverrides,
    servers: &BTreeMap<String, McpServerConfig>,
    prelude_defaults: &ResolvedPreludeDefaults,
    code_execution_defaults: Option<&ProfileCodeExecution>,
) -> Result<SessionDefinition, ConfigError> {
    let provider = options
        .provider
        .or(profile.map(|p| p.provider))
        .ok_or(ConfigError::ProviderRequired)?;
    let model = options
        .model
        .or_else(|| profile.map(|p| p.model.clone()))
        .ok_or(ConfigError::ModelRequired)?;
    let timeout = options
        .timeout_secs
        .or(profile.and_then(|p| p.timeout_secs))
        .unwrap_or(120);
    if !(1..=86400).contains(&timeout) {
        return Err(ConfigError::InvalidTimeout);
    }
    let workspace_root = match options.workspace_root {
        Some(path) => Some(absolute_root(path, None)?),
        None => profile.and_then(|p| p.workspace_root.clone()),
    };
    let selected = options
        .mcp
        .or_else(|| profile.and_then(|p| p.mcp.clone()))
        .unwrap_or_default();
    let mut mcp_servers = Vec::with_capacity(selected.len());
    for id in selected {
        if id.is_empty()
            || mcp_servers
                .iter()
                .any(|server: &McpServerConfig| server.id == id)
        {
            return Err(ConfigError::InvalidMcpServer(id));
        }
        let server = servers
            .get(&id)
            .ok_or_else(|| ConfigError::UnknownMcpServer(id.clone()))?;
        mcp_servers.push(server.clone());
    }
    let tools = options
        .tools
        .or_else(|| profile.and_then(|p| p.tools.clone()))
        .unwrap_or_default();
    let uses_command = tools.contains(&WorkspaceTool::RunCommand);
    if profile.and_then(|p| p.command.as_ref()).is_some() && !uses_command {
        return Err(ConfigError::Definition(
            crate::definition::DefinitionError::CommandSettingsWithoutTool,
        ));
    }
    if options.command_network.is_some() && !uses_command {
        return Err(ConfigError::Definition(
            crate::definition::DefinitionError::CommandSettingsWithoutTool,
        ));
    }
    let command = if uses_command {
        let mut settings = profile
            .and_then(|p| p.command.as_ref())
            .map(ProfileCommand::effective)
            .unwrap_or_default();
        if let Some(network) = options.command_network {
            settings.network = network;
        }
        settings.validate().map_err(ConfigError::Definition)?;
        Some(settings)
    } else {
        None
    };
    let code_execution_settings = match options.code_execution {
        Some(settings) => settings,
        None => {
            let mut settings = CodeExecutionSettings::default();
            if let Some(top) = code_execution_defaults {
                settings = top.effective_over(settings);
            }
            if let Some(profile_ce) = profile.and_then(|p| p.code_execution.as_ref()) {
                settings = profile_ce.effective_over(settings);
            }
            settings
        }
    };
    code_execution_settings
        .validate()
        .map_err(ConfigError::Definition)?;
    let code_execution_enabled = options
        .code_execution_enabled
        .or_else(|| profile.and_then(|p| p.code_execution.as_ref()?.enabled))
        .or_else(|| code_execution_defaults.and_then(|c| c.enabled))
        .unwrap_or(false);
    let code_execution = code_execution_enabled.then_some(code_execution_settings);
    let temperature = options
        .temperature
        .or(profile.and_then(|p| p.temperature))
        .map(Temperature::new)
        .transpose()
        .map_err(ConfigError::Definition)?;
    let definition = SessionDefinition {
        provider,
        model,
        endpoint: options
            .endpoint
            .or_else(|| profile.and_then(|p| p.endpoint.clone())),
        credential: CredentialRef::Environment(
            options
                .api_key_env
                .or_else(|| profile.and_then(|p| p.api_key_env.clone()))
                .unwrap_or_else(|| match provider {
                    Provider::Anthropic => "ANTHROPIC_API_KEY".into(),
                    _ => "OPENAI_API_KEY".into(),
                }),
        ),
        work_timeout_secs: Some(timeout),
        retained_work_capacity: options
            .capacity
            .or(profile.and_then(|p| p.capacity))
            .unwrap_or(256),
        max_model_rounds: options
            .max_model_rounds
            .or(profile.and_then(|p| p.max_model_rounds))
            .unwrap_or(10),
        max_tool_calls: options
            .max_tool_calls
            .or(profile.and_then(|p| p.max_tool_calls))
            .unwrap_or(64),
        // No default: an unset limit stays unset, and the Anthropic protocol
        // then rejects the request before it is sent.
        max_tokens: options.max_tokens.or(profile.and_then(|p| p.max_tokens)),
        // An unset window stays unknown, which disables automatic compaction.
        context_window: options
            .context_window
            .or(profile.and_then(|p| p.context_window)),
        auto_compact: options
            .auto_compact
            .or(profile.and_then(|p| p.auto_compact))
            .unwrap_or(true),
        compact_threshold: options
            .compact_threshold
            .or(profile.and_then(|p| p.compact_threshold))
            .unwrap_or(70),
        prompt_cache: options
            .prompt_cache
            .or(profile.and_then(|p| p.prompt_cache))
            .unwrap_or(true),
        reasoning_effort: options
            .reasoning_effort
            .or_else(|| profile.and_then(|p| p.reasoning_effort.clone())),
        temperature,
        extra_body: options
            .extra_body
            .or_else(|| profile.and_then(|p| p.extra_body.clone()))
            .unwrap_or_default(),
        // No default: retry cost is the user's choice, so unset means one call.
        max_model_attempts: options
            .max_model_attempts
            .or(profile.and_then(|p| p.max_model_attempts)),
        render_options: options
            .render_options
            .or_else(|| profile.and_then(|p| p.render_options.clone()))
            .unwrap_or_default(),
        workspace_root,
        tools,
        tool_limits: options
            .tool_limits
            .or_else(|| {
                profile.and_then(|p| p.tool_limits.as_ref().map(ProfileToolLimits::effective))
            })
            .unwrap_or_default(),
        write_limits: options.write_limits.or_else(|| {
            profile.and_then(|p| p.write_limits.as_ref().map(ProfileWriteLimits::effective))
        }),
        command,
        permission_mode: match options
            .permission_mode
            .or(profile.and_then(|p| p.permission_mode))
        {
            Some(PermissionMode::Ask) | None => None,
            Some(mode) => Some(mode),
        },
        mcp_servers,
        prelude: resolve_prelude(profile, options.prelude, prelude_defaults)?,
        code_execution,
    };
    definition.validate().map_err(ConfigError::Definition)?;
    Ok(definition)
}

fn resolve_prelude(
    profile: Option<&Profile>,
    overrides: PreludeOverrides,
    defaults: &ResolvedPreludeDefaults,
) -> Result<PreludeDefinition, ConfigError> {
    let static_text = match overrides.static_custom {
        Some(StaticCustomOverride::Inline(text)) => Some(text),
        Some(StaticCustomOverride::File(path)) => Some(read_explicit_prelude_file(path, None)?),
        None => profile
            .and_then(|p| p.resolved_system_prompt.clone())
            .or_else(|| defaults.static_custom.clone()),
    };
    let static_custom = static_text
        .filter(|t| !t.trim().is_empty())
        .map(StaticCustomSlot::new);

    let dynamic_custom = match overrides.dynamic_custom {
        Some(DynamicCustomOverride::Inline(text)) => Some(DynamicCustomSlot::Inline(text)),
        Some(DynamicCustomOverride::File(path)) => {
            let canonical = resolve_and_validate_dynamic_file(path, None)?;
            Some(DynamicCustomSlot::File(canonical))
        }
        None => profile
            .and_then(|p| p.resolved_dynamic_custom.clone())
            .or_else(|| defaults.dynamic_custom.clone()),
    }
    .filter(|slot| !matches!(slot, DynamicCustomSlot::Inline(text) if text.trim().is_empty()));

    let mut skill_dirs = match overrides.skill_dirs {
        Some(dirs) => {
            let mut resolved = Vec::with_capacity(dirs.len());
            for dir in dirs {
                resolved.push(resolve_explicit_skill_dir(dir, None)?);
            }
            resolved
        }
        None => {
            if let Some(dirs) = profile.and_then(|p| p.resolved_skill_dirs.as_ref()) {
                dirs.clone()
            } else if let Some(dirs) = defaults.skill_dirs.as_ref() {
                dirs.clone()
            } else if let Some(config_dir) = defaults.config_dir.as_deref() {
                default_config_subdir(config_dir, "skills")
            } else {
                Vec::new()
            }
        }
    };

    for extra in overrides.extra_skill_dirs {
        skill_dirs.push(resolve_explicit_skill_dir(extra, None)?);
    }

    let include_workspace_skills = overrides
        .include_workspace_skills
        .or_else(|| profile.and_then(|p| p.include_workspace_skills))
        .or(defaults.include_workspace_skills)
        .unwrap_or(true);
    let skills_enabled = overrides.skills_enabled.unwrap_or(true);

    let mut agent_dirs = match overrides.agent_dirs {
        Some(dirs) => {
            let mut resolved = Vec::with_capacity(dirs.len());
            for dir in dirs {
                resolved.push(resolve_explicit_agent_dir(dir, None)?);
            }
            resolved
        }
        None => {
            if let Some(dirs) = profile.and_then(|p| p.resolved_agent_dirs.as_ref()) {
                dirs.clone()
            } else if let Some(dirs) = defaults.agent_dirs.as_ref() {
                dirs.clone()
            } else if let Some(config_dir) = defaults.config_dir.as_deref() {
                default_config_subdir(config_dir, "agents")
            } else {
                Vec::new()
            }
        }
    };

    for extra in overrides.extra_agent_dirs {
        agent_dirs.push(resolve_explicit_agent_dir(extra, None)?);
    }

    let include_workspace_agents = overrides
        .include_workspace_agents
        .or_else(|| profile.and_then(|p| p.include_workspace_agents))
        .or(defaults.include_workspace_agents)
        .unwrap_or(true);
    let agents_enabled = overrides.agents_enabled.unwrap_or(true);

    Ok(PreludeDefinition {
        static_custom,
        dynamic_custom,
        config_dir: defaults.config_dir.clone(),
        skill_dirs,
        include_workspace_skills,
        skills_enabled,
        agent_dirs,
        include_workspace_agents,
        agents_enabled,
    })
}

fn default_config_subdir(config_dir: &Path, subdir: &str) -> Vec<PathBuf> {
    let candidate = config_dir.join(subdir);
    match fs::canonicalize(&candidate) {
        Ok(canon) if fs::metadata(&canon).is_ok_and(|m| m.is_dir()) => vec![canon],
        _ => Vec::new(),
    }
}

pub(super) fn resolve_relative_to_base(
    path: PathBuf,
    base: Option<&Path>,
) -> Result<PathBuf, ConfigError> {
    if path.as_os_str().is_empty() {
        return Err(ConfigError::EmptyPath);
    }
    if path.is_absolute() {
        return Ok(path);
    }
    let base = match base {
        Some(base) => base.to_owned(),
        None => std::env::current_dir().map_err(|error| ConfigError::Io {
            path: path.clone(),
            kind: error.kind(),
        })?,
    };
    Ok(base.join(path))
}

pub(super) fn read_explicit_prelude_file(
    path: PathBuf,
    base: Option<&Path>,
) -> Result<String, ConfigError> {
    let resolved = resolve_relative_to_base(path, base)?;
    let canon = fs::canonicalize(&resolved).map_err(|error| ConfigError::PreludeFile {
        path: resolved.clone(),
        reason: error.to_string(),
    })?;
    let meta = fs::metadata(&canon).map_err(|error| ConfigError::PreludeFile {
        path: canon.clone(),
        reason: error.to_string(),
    })?;
    if !meta.is_file() {
        return Err(ConfigError::PreludeFile {
            path: canon,
            reason: "path is not a regular file".into(),
        });
    }
    let file = File::open(&canon).map_err(|error| ConfigError::PreludeFile {
        path: canon.clone(),
        reason: error.to_string(),
    })?;
    let mut bytes = Vec::new();
    file.take(MAX_PRELUDE_FILE_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| ConfigError::PreludeFile {
            path: canon.clone(),
            reason: error.to_string(),
        })?;
    if bytes.len() > MAX_PRELUDE_FILE_BYTES {
        return Err(ConfigError::PreludeFile {
            path: canon,
            reason: format!("file exceeds {MAX_PRELUDE_FILE_BYTES} bytes"),
        });
    }
    String::from_utf8(bytes).map_err(|_| ConfigError::PreludeFile {
        path: canon,
        reason: "file is not valid UTF-8".into(),
    })
}

pub(super) fn resolve_and_validate_dynamic_file(
    path: PathBuf,
    base: Option<&Path>,
) -> Result<PathBuf, ConfigError> {
    let resolved = resolve_relative_to_base(path, base)?;
    let canon = fs::canonicalize(&resolved).map_err(|error| ConfigError::PreludeFile {
        path: resolved.clone(),
        reason: error.to_string(),
    })?;
    let meta = fs::metadata(&canon).map_err(|error| ConfigError::PreludeFile {
        path: canon.clone(),
        reason: error.to_string(),
    })?;
    if !meta.is_file() {
        return Err(ConfigError::PreludeFile {
            path: canon,
            reason: "path is not a regular file".into(),
        });
    }
    let file = File::open(&canon).map_err(|error| ConfigError::PreludeFile {
        path: canon.clone(),
        reason: error.to_string(),
    })?;
    let mut bytes = Vec::new();
    file.take(MAX_PRELUDE_FILE_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| ConfigError::PreludeFile {
            path: canon.clone(),
            reason: error.to_string(),
        })?;
    if bytes.len() > MAX_PRELUDE_FILE_BYTES {
        return Err(ConfigError::PreludeFile {
            path: canon,
            reason: format!("file exceeds {MAX_PRELUDE_FILE_BYTES} bytes"),
        });
    }
    if std::str::from_utf8(&bytes).is_err() {
        return Err(ConfigError::PreludeFile {
            path: canon,
            reason: "file is not valid UTF-8".into(),
        });
    }
    Ok(canon)
}

pub(super) fn resolve_explicit_skill_dir(
    path: PathBuf,
    base: Option<&Path>,
) -> Result<PathBuf, ConfigError> {
    resolve_explicit_dir(path, base, |path, kind| ConfigError::SkillDirectory {
        path,
        kind,
    })
}

pub(super) fn resolve_explicit_agent_dir(
    path: PathBuf,
    base: Option<&Path>,
) -> Result<PathBuf, ConfigError> {
    resolve_explicit_dir(path, base, |path, kind| ConfigError::AgentDirectory {
        path,
        kind,
    })
}

fn resolve_explicit_dir(
    path: PathBuf,
    base: Option<&Path>,
    make_err: impl Fn(PathBuf, io::ErrorKind) -> ConfigError,
) -> Result<PathBuf, ConfigError> {
    let resolved = resolve_relative_to_base(path, base)?;
    let canon =
        fs::canonicalize(&resolved).map_err(|error| make_err(resolved.clone(), error.kind()))?;
    match fs::metadata(&canon) {
        Ok(meta) if meta.is_dir() => Ok(canon),
        Ok(_) => Err(make_err(canon, io::ErrorKind::NotADirectory)),
        Err(error) => Err(make_err(canon, error.kind())),
    }
}

pub(super) fn absolute_root(path: PathBuf, base: Option<&Path>) -> Result<PathBuf, ConfigError> {
    if path.as_os_str().is_empty() {
        return Err(ConfigError::EmptyPath);
    }
    let absolute = if path.is_absolute() {
        path
    } else {
        let base = match base {
            Some(base) => base.to_owned(),
            None => std::env::current_dir().map_err(|error| ConfigError::Io {
                path: path.clone(),
                kind: error.kind(),
            })?,
        };
        base.join(path)
    };
    let resolved = std::fs::canonicalize(&absolute)
        .map_err(|error| ConfigError::Workspace { kind: error.kind() })?;
    if !resolved.is_dir() {
        return Err(ConfigError::Workspace {
            kind: std::io::ErrorKind::NotADirectory,
        });
    }
    Ok(resolved)
}

/// Parse the shared comma-separated MCP server selection; an empty string
/// disables all servers. Malformed and repeated ids are rejected here;
/// resolving the selection also rejects ids without a configured server.
pub fn parse_mcp(value: &str) -> Result<Vec<String>, ConfigError> {
    if value.trim().is_empty() {
        return Ok(Vec::new());
    }
    let mut ids: Vec<String> = Vec::new();
    for id in value.split(',').map(str::trim) {
        if !crate::definition::valid_server_id(id) || ids.iter().any(|selected| selected == id) {
            return Err(ConfigError::InvalidMcpServer(id.to_owned()));
        }
        ids.push(id.to_owned());
    }
    Ok(ids)
}

/// Parse the shared comma-separated tool selection; an empty string disables all tools.
/// Unknown and duplicate names are rejected rather than granting extra capabilities.
pub fn parse_tools(value: &str) -> Result<Vec<WorkspaceTool>, ConfigError> {
    if value.trim().is_empty() {
        return Ok(Vec::new());
    }
    let mut tools = Vec::new();
    for name in value.split(',').map(str::trim) {
        let Some(tool) = WorkspaceTool::from_name(name) else {
            return Err(invalid_tools());
        };
        if tools.contains(&tool) {
            return Err(invalid_tools());
        }
        tools.push(tool);
    }
    Ok(tools)
}

// The message follows the single declaration list so a new built-in tool is
// named without editing this error.
fn invalid_tools() -> ConfigError {
    ConfigError::InvalidTools(
        WorkspaceTool::ALL
            .iter()
            .map(|tool| tool.name())
            .collect::<Vec<_>>()
            .join(", "),
    )
}
