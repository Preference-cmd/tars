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

mod graphemes;
pub(crate) use graphemes::edit;

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
}
pub(crate) const FIELD_NAMES: [&str; 10] = [
    "Provider",
    "Model",
    "Endpoint (optional, full URL)",
    "Credential environment variable",
    "Timeout seconds (1..86400)",
    "Workspace root (optional; does not enable tools)",
    "Tools (comma-separated list_directory,read_file,search_text,write_file,edit_file,run_command)",
    "Permission mode (read-only | ask | accept-edits | full-access)",
    "MCP servers (comma-separated ids from mcp.json)",
    "Prompt cache (on | off)",
];

impl Default for Form {
    fn default() -> Self {
        Self {
            fields: [
                "openai-chat",
                "",
                "",
                "OPENAI_API_KEY",
                "120",
                "",
                "",
                "ask",
                "",
                "on",
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
        let CredentialRef::Environment(name) = &preset.credential;
        let mode_str = preset.permission_mode.map_or("ask", |m| m.as_str());
        let values = [
            provider.to_owned(),
            preset.model.clone(),
            preset.endpoint.clone().unwrap_or_default(),
            name.clone(),
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
                    api_key_env: Some(values[3].clone()),
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
        if key.kind == KeyEventKind::Release {
            return;
        }
        match key.code {
            KeyCode::Tab => self.selected = (self.selected + 1) % self.fields.len(),
            KeyCode::BackTab => {
                self.selected = (self.selected + self.fields.len() - 1) % self.fields.len()
            }
            KeyCode::Enter => {}
            _ if !key.modifiers.contains(KeyModifiers::CONTROL)
                || !matches!(key.code, KeyCode::Char('m' | 'j')) =>
            {
                edit(&mut self.fields[self.selected], key);
            }
            _ => {}
        }
    }
}
