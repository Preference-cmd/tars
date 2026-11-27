use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui_textarea::{TextArea, WrapMode};
use std::sync::Arc;
use tars_core::{
    config::{Config, DefinitionOverrides, parse_mcp, parse_tools},
    definition::{
        CommandSettings, CredentialRef, Provider, SessionDefinition, WorkspaceTool,
        WorkspaceToolLimits, WriteToolLimits,
    },
    permission::PermissionMode,
};

mod complete;
pub(crate) mod editor;
mod graphemes;
pub(crate) mod image;
mod onboarding;
pub(crate) use complete::{
    CompleteResult, ModelArgCompletion, complete_copy_arg, complete_editor, complete_model_arg,
    set_editor_text, slash_command_takes_args, strip_copy_command_prefix,
    strip_model_command_prefix,
};
pub(crate) use graphemes::edit;
pub(crate) use onboarding::FolderTrustWizard;

pub(crate) fn editor() -> TextArea<'static> {
    let mut editor = TextArea::default();
    editor.set_wrap_mode(WrapMode::Word);
    editor.set_cursor_line_style(ratatui::style::Style::default());
    editor
}

pub(crate) fn control(key: KeyEvent, letter: char) -> bool {
    key.kind == KeyEventKind::Press
        && key.modifiers.contains(KeyModifiers::CONTROL)
        && key.code == KeyCode::Char(letter)
}

// Strip control characters, including escape, from pasted content. Newlines and
// tabs are data; the paste event never passes through command dispatch.
pub(crate) fn paste(editor: &mut TextArea<'static>, text: &str, multiline: bool) {
    let normalized = text.replace("\r\n", "\n").replace('\r', "\n");
    let text: String = normalized
        .chars()
        .filter(|c| !c.is_control() || (multiline && matches!(c, '\n' | '\t')))
        .collect();
    editor.insert_str(text);
}

pub(crate) struct Form {
    pub fields: Vec<TextArea<'static>>,
    pub selected: usize,
    pub error: String,
    pub limits: (usize, u32, u32),
    /// Output token limit, reasoning effort and extra request fields carried
    /// over from the preset.
    pub generation: (
        Option<u32>,
        Option<String>,
        serde_json::Map<String, serde_json::Value>,
    ),
    /// Model attempt limit carried over from the preset.
    pub max_model_attempts: Option<u32>,
    /// Protocol render options carried over from the preset. A provider
    /// changed in the form must accept them, or creation is refused.
    pub render_options: serde_json::Map<String, serde_json::Value>,
    pub tool_limits: WorkspaceToolLimits,
    pub write_limits: Option<WriteToolLimits>,
    /// Startup configuration used to resolve the selected MCP server ids.
    pub config: Arc<Config>,
    /// Named preset the form was opened from, so resolution keeps using it.
    pub profile: Option<String>,
    /// Frozen command settings read from the preset. Display-only: the form
    /// never edits them, and creation resolves them through the preset.
    pub command: Option<CommandSettings>,
    /// Whether `generation.0` (`max_tokens`) was auto-populated when cycling
    /// Field 0 (`Provider`) to `anthropic` so cycling away can restore `None`.
    pub auto_max_tokens: bool,
}
pub(crate) const FIELD_NAMES: [&str; 11] = [
    "Provider",
    "Model",
    "Endpoint (optional, full URL)",
    "Credential (store name, or env:VAR)",
    "Timeout seconds (1..86400)",
    "Workspace root (optional; does not enable tools)",
    "Tools (comma-separated list_directory,read_file,search_text,find_files,write_file,edit_file,run_command)",
    "Permission mode (read-only | ask | accept-edits | full-access)",
    "MCP servers (comma-separated ids from mcp.json)",
    "Prompt cache (on | off)",
    "Code execution (on | off)",
];

impl Default for Form {
    fn default() -> Self {
        Self {
            fields: [
                "openai-chat",
                "",
                "",
                "openai-chat",
                "120",
                "",
                "",
                "ask",
                "",
                "on",
                "off",
            ]
            .into_iter()
            .map(|s| TextArea::from([s]))
            .collect(),
            selected: 1,
            error: String::new(),
            limits: (256, 10, 64),
            generation: (None, None, Default::default()),
            max_model_attempts: None,
            render_options: Default::default(),
            tool_limits: WorkspaceToolLimits::default(),
            write_limits: None,
            config: Arc::new(Config::default()),
            profile: None,
            command: None,
            auto_max_tokens: false,
        }
    }
}
impl Form {
    /// Read-only line describing the command settings that will apply, when
    /// the current Tools field selects the command tool. The settings come
    /// from the preset (or their defaults); the line is not a field and cannot
    /// be edited or focused.
    pub(crate) fn command_summary(&self) -> Option<String> {
        let tools = parse_tools(&self.fields[6].lines().join("")).ok()?;
        if !tools.contains(&WorkspaceTool::RunCommand) {
            return None;
        }
        let settings = self.command.unwrap_or_default();
        let source = if self.command.is_some() {
            "preset"
        } else {
            "defaults"
        };
        Some(format!(
            "Command: network {} · timeout {}s (max {}s) · {source}",
            settings.network, settings.default_timeout_secs, settings.max_timeout_secs
        ))
    }

    pub fn new(
        config: Arc<Config>,
        profile: Option<String>,
        preset: Option<&SessionDefinition>,
    ) -> Self {
        let mut form = Self {
            config,
            profile,
            ..Self::default()
        };
        let Some(preset) = preset else {
            return form;
        };
        form.command = preset.effective_command_settings();
        let provider = match preset.provider {
            Provider::OpenAiChat => "openai-chat",
            Provider::OpenAiResponses => "openai-responses",
            Provider::Anthropic => "anthropic",
        };
        let credential_value = match &preset.credential {
            CredentialRef::Environment(name) => format!("env:{name}"),
            CredentialRef::Store(name) => name.clone(),
        };
        let mode_str = preset.permission_mode.map_or("ask", |m| m.as_str());
        let values = [
            provider.to_owned(),
            preset.model.clone(),
            preset.endpoint.clone().unwrap_or_default(),
            credential_value,
            preset.work_timeout_secs.unwrap_or(120).to_string(),
            preset
                .workspace_root
                .as_ref()
                .map(|path| path.display().to_string())
                .unwrap_or_default(),
            preset
                .tools
                .iter()
                .map(|tool| tool.name())
                .collect::<Vec<_>>()
                .join(","),
            mode_str.to_owned(),
            preset
                .mcp_servers
                .iter()
                .map(|server| server.id.as_str())
                .collect::<Vec<_>>()
                .join(","),
            if preset.prompt_cache { "on" } else { "off" }.to_owned(),
            if preset.code_execution.is_some() {
                "on"
            } else {
                "off"
            }
            .to_owned(),
        ];
        form.fields = values
            .into_iter()
            .map(|value| TextArea::from([value]))
            .collect();
        form.tool_limits = preset.tool_limits.clone();
        form.write_limits = preset.write_limits;
        form.generation = (
            preset.max_tokens,
            preset.reasoning_effort.clone(),
            preset.extra_body.clone(),
        );
        form.max_model_attempts = preset.max_model_attempts;
        form.render_options = preset.render_options.clone();
        form.limits = (
            preset.retained_work_capacity,
            preset.max_model_rounds,
            preset.max_tool_calls,
        );
        form
    }

    pub(crate) fn with_code_execution(mut self, enabled: Option<bool>) -> Self {
        if let Some(enabled) = enabled {
            self.fields[10] = TextArea::from([if enabled { "on" } else { "off" }]);
        }
        self
    }

    pub(crate) fn with_workspace_defaults(
        mut self,
        workspace_root: Option<std::path::PathBuf>,
        default_tools: &[WorkspaceTool],
    ) -> Self {
        let has_workspace = !self.fields[5].lines().join("").trim().is_empty();
        if !has_workspace && let Some(ws) = workspace_root {
            self.fields[5] = TextArea::from([ws.display().to_string()]);
        }
        let effective_workspace = !self.fields[5].lines().join("").trim().is_empty();
        let has_tools = !self.fields[6].lines().join("").trim().is_empty();
        if effective_workspace && !has_tools && !default_tools.is_empty() {
            let joined = default_tools
                .iter()
                .map(|tool| tool.name())
                .collect::<Vec<_>>()
                .join(",");
            self.fields[6] = TextArea::from([joined]);
        }
        self
    }

    pub(crate) const fn is_selector_field(index: usize) -> bool {
        matches!(index, 0 | 7 | 9 | 10)
    }

    fn cycle_selector(&mut self, index: usize, forward: bool) {
        const PROVIDERS: [&str; 3] = ["openai-chat", "openai-responses", "anthropic"];
        const MODES: [&str; 4] = ["read-only", "ask", "accept-edits", "full-access"];
        const ON_OFF: [&str; 2] = ["on", "off"];
        const OFF_ON: [&str; 2] = ["off", "on"];
        let options: &[&str] = match index {
            0 => &PROVIDERS,
            7 => &MODES,
            9 => &ON_OFF,
            10 => &OFF_ON,
            _ => return,
        };
        let current = self.fields[index].lines().join("");
        let trimmed = current.trim();
        let pos = options.iter().position(|&opt| opt == trimmed).unwrap_or(0);
        let next_pos = if forward {
            (pos + 1) % options.len()
        } else {
            (pos + options.len() - 1) % options.len()
        };
        let next = options[next_pos];
        self.fields[index] = TextArea::from([next]);
        if index == 0 {
            if next == "anthropic" {
                if self.generation.0.is_none() {
                    self.generation.0 = Some(8192);
                    self.auto_max_tokens = true;
                }
            } else if self.auto_max_tokens {
                self.generation.0 = None;
                self.auto_max_tokens = false;
            }
            if self.fields[3].lines().join("").trim() == trimmed {
                self.fields[3] = TextArea::from([next]);
            }
        }
    }

    pub fn definition(&self) -> Result<SessionDefinition, String> {
        let values: Vec<_> = self.fields.iter().map(|f| f.lines().join("")).collect();
        let provider = match values[0].as_str() {
            "openai-chat" => Provider::OpenAiChat,
            "openai-responses" => Provider::OpenAiResponses,
            "anthropic" => Provider::Anthropic,
            _ => return Err("Provider: openai-chat | openai-responses | anthropic".into()),
        };
        let timeout = values[4]
            .parse::<u64>()
            .ok()
            .filter(|n| (1..=86400).contains(n))
            .ok_or("Timeout must be 1..86400 seconds")?;
        let permission_mode = values[7]
            .trim()
            .parse::<PermissionMode>()
            .map_err(|_| "Permission mode: read-only | ask | accept-edits | full-access")?;
        let prompt_cache = match values[9].trim() {
            "on" => true,
            "off" => false,
            _ => return Err("Prompt cache: on | off".into()),
        };
        let code_execution_enabled = match values[10].trim() {
            "on" => true,
            "off" => false,
            _ => return Err("Code execution: on | off".into()),
        };
        let raw_credential = values[3].trim();
        let (credential, api_key_env) = if let Some(env_var) = raw_credential.strip_prefix("env:") {
            (None, Some(env_var.trim().to_owned()))
        } else {
            (Some(raw_credential.to_owned()), None)
        };
        // Resolve explicit form values through the same core path as CLI input.
        // Profile limits were frozen when the selected startup preset was loaded.
        let mut definition = self
            .config
            .definition(
                self.profile.as_deref(),
                DefinitionOverrides {
                    provider: Some(provider),
                    model: Some(values[1].clone()),
                    endpoint: (!values[2].is_empty()).then(|| values[2].clone()),
                    credential,
                    api_key_env,
                    timeout_secs: Some(timeout),
                    capacity: Some(self.limits.0),
                    max_model_rounds: Some(self.limits.1),
                    max_tool_calls: Some(self.limits.2),
                    context_window: None,
                    auto_compact: None,
                    compact_threshold: None,
                    prompt_cache: Some(prompt_cache),
                    max_tokens: self.generation.0,
                    reasoning_effort: self.generation.1.clone(),
                    temperature: None,
                    extra_body: Some(self.generation.2.clone()),
                    max_model_attempts: self.max_model_attempts,
                    render_options: Some(self.render_options.clone()),
                    workspace_root: (!values[5].is_empty()).then(|| values[5].clone().into()),
                    tools: Some(parse_tools(&values[6]).map_err(|e| e.to_string())?),
                    tool_limits: Some(self.tool_limits.clone()),
                    write_limits: self.write_limits,
                    command_network: None,
                    permission_mode: Some(permission_mode),
                    mcp: Some(parse_mcp(&values[8]).map_err(|e| e.to_string())?),
                    prelude: Default::default(),
                    code_execution_enabled: Some(code_execution_enabled),
                    code_execution: None,
                },
            )
            .map_err(|e| e.to_string())?;
        // Endpoint and workspace root are visible fields. An empty value is an
        // explicit clear, not an absent override that falls back to the profile.
        // A nonempty workspace root was already made absolute and validated by
        // the resolver, so only the empty case is applied here.
        definition.endpoint = (!values[2].is_empty()).then(|| values[2].clone());
        if values[5].is_empty() {
            definition.workspace_root = None;
        }
        definition.validate().map_err(|e| e.to_string())?;
        Ok(definition)
    }
    pub fn key(&mut self, key: KeyEvent) {
        if key.kind == KeyEventKind::Release
            || (key.kind == KeyEventKind::Repeat && key.modifiers.contains(KeyModifiers::CONTROL))
        {
            return;
        }
        match key.code {
            KeyCode::Tab => self.selected = (self.selected + 1) % self.fields.len(),
            KeyCode::BackTab => {
                self.selected = (self.selected + self.fields.len() - 1) % self.fields.len()
            }
            KeyCode::Enter => {}
            _ if Self::is_selector_field(self.selected) => {
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
                {
                    match key.code {
                        KeyCode::Left => self.cycle_selector(self.selected, false),
                        KeyCode::Right | KeyCode::Char(' ') => {
                            self.cycle_selector(self.selected, true)
                        }
                        _ => {}
                    }
                }
            }
            _ if !key.modifiers.contains(KeyModifiers::CONTROL)
                || !matches!(key.code, KeyCode::Char('m' | 'j')) =>
            {
                edit(&mut self.fields[self.selected], key);
            }
            _ => {}
        }
    }
}
