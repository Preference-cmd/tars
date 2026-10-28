use super::command::CommandSettings;
use super::mcp::McpServerConfig;
use super::workspace::{WorkspaceTool, WorkspaceToolLimits, WriteToolLimits};
use crate::session::SessionConfig;
use causa::kernel::ModelRef;
use causa::providers::{AnthropicRenderOptions, ChatRenderOptions, ResponsesRenderOptions};
use causa::runtime::{TurnLimits, TurnRunOptions};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::time::{Duration, Instant};

/// Supported wire protocols; model identifiers are supplied explicitly.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Provider {
    /// OpenAI Chat Completions wire format.
    #[serde(rename = "openai-chat")]
    OpenAiChat,
    /// OpenAI Responses wire format.
    #[serde(rename = "openai-responses")]
    OpenAiResponses,
    /// Anthropic Messages wire format.
    Anthropic,
}

/// A resolvable credential reference, never a persisted plaintext credential.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CredentialRef {
    /// Name of the environment variable resolved once when opening a session.
    Environment(String),
}

/// Frozen execution choices saved with a persistent session's materials.
/// Workspace permissions and bounds are fixed by record format 2.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionDefinition {
    /// Selected provider wire protocol.
    pub provider: Provider,
    /// Explicit provider model identifier.
    pub model: String,
    /// Full HTTP(S) endpoint, without credentials, query or fragment.
    pub endpoint: Option<String>,
    /// Credential reference; resolving it is assembly's responsibility.
    pub credential: CredentialRef,
    /// Admission-relative deadline in seconds, or no deadline; zero is valid.
    pub work_timeout_secs: Option<u64>,
    /// Maximum retained work records; zero rejects new work.
    pub retained_work_capacity: usize,
    /// Maximum logical model invocations, including the first; zero stops before invocation.
    pub max_model_rounds: u32,
    /// Maximum cumulative model tool declarations; zero permits none.
    pub max_tool_calls: u32,
    /// Upper bound on generated tokens per model call, or none to leave it to
    /// the provider. A record saved before this field existed reads as none.
    /// The Anthropic protocol requires a value; without one the request is
    /// rejected before it is sent.
    #[serde(default)]
    pub max_tokens: Option<u32>,
    /// Requested reasoning effort, or none for the provider default. The
    /// text is sent to the provider as written; Tarz keeps no list of accepted
    /// values, since they differ by model and server. A record saved before
    /// this field existed reads as none.
    #[serde(default)]
    pub reasoning_effort: Option<String>,
    /// Further request fields, sent at the top level of every model request
    /// as written, such as an Anthropic `thinking` object. A record saved
    /// before this field existed reads as empty.
    #[serde(default, skip_serializing_if = "serde_json::Map::is_empty")]
    pub extra_body: serde_json::Map<String, serde_json::Value>,
    /// Total model calls allowed per logical invocation, the first included,
    /// when the provider fails transiently or rate-limits. None or one makes
    /// a single call. A record saved before this field existed reads as none.
    #[serde(default)]
    pub max_model_attempts: Option<u32>,
    /// Render options for the provider's protocol, as written. Keys left out
    /// take the protocol's defaults; the accepted keys and values are those
    /// of the protocol's render options type. A record saved before this
    /// field existed reads as empty.
    #[serde(default, skip_serializing_if = "serde_json::Map::is_empty")]
    pub render_options: serde_json::Map<String, serde_json::Value>,
    /// Absolute workspace root frozen at creation; never resolved against reopen cwd.
    #[serde(deserialize_with = "required_workspace_root")]
    pub workspace_root: Option<PathBuf>,
    /// Explicit read-only capabilities; an empty selection performs no workspace access.
    pub tools: Vec<WorkspaceTool>,
    /// Fixed effective bounds for selected workspace tools.
    pub tool_limits: WorkspaceToolLimits,
    /// Explicit bounds for workspace write and edit operations.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub write_limits: Option<WriteToolLimits>,
    /// Initial execution permission mode; None defaults to Ask.
    /// Explicit Ask is normalized to None when creating a new session to keep records
    /// without new features unmodified.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub permission_mode: Option<crate::permission::PermissionMode>,
    /// Frozen command-tool settings; present only when `run_command` is selected.
    /// A record saved before this field existed reads as none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<CommandSettings>,
    /// Frozen snapshots of the MCP servers selected when the session was created.
    /// Empty when no server is enabled. A record saved before this field existed
    /// reads as empty.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub mcp_servers: Vec<McpServerConfig>,
}

// Unlike sparse startup configuration, a record must explicitly retain no-root
// as null rather than gaining that choice when a field is missing.
fn required_workspace_root<'de, D>(deserializer: D) -> Result<Option<PathBuf>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Option::<PathBuf>::deserialize(deserializer)
}

impl SessionDefinition {
    /// Validate without resolving credentials, contacting providers or editing data.
    /// Error messages omit endpoint and model contents to avoid accidental disclosure.
    pub fn validate(&self) -> Result<(), DefinitionError> {
        if self.model.trim().is_empty() {
            return Err(DefinitionError::BlankModel);
        }
        let CredentialRef::Environment(name) = &self.credential;
        if !valid_environment_name(name) {
            return Err(DefinitionError::InvalidCredentialReference);
        }
        if self
            .endpoint
            .as_deref()
            .is_some_and(|endpoint| !valid_http_endpoint(endpoint))
        {
            return Err(DefinitionError::InvalidEndpoint);
        }
        if self.work_timeout_secs.is_some_and(|seconds| {
            Instant::now()
                .checked_add(Duration::from_secs(seconds))
                .is_none()
        }) {
            return Err(DefinitionError::UnrepresentableTimeout);
        }
        if self
            .workspace_root
            .as_ref()
            .is_some_and(|root| !root.is_absolute())
        {
            return Err(DefinitionError::InvalidWorkspaceRoot);
        }
        if self.max_tokens == Some(0) {
            return Err(DefinitionError::InvalidMaxTokens);
        }
        if self.max_model_attempts == Some(0) {
            return Err(DefinitionError::InvalidModelAttempts);
        }
        self.protocol_render_options()?;
        if !self.tools.is_empty() && self.workspace_root.is_none() {
            return Err(DefinitionError::WorkspaceRequired);
        }
        for (index, tool) in self.tools.iter().enumerate() {
            if self.tools[..index].contains(tool) {
                return Err(DefinitionError::DuplicateTool);
            }
        }
        self.tool_limits.validate()?;
        if let Some(write_limits) = &self.write_limits {
            write_limits.validate()?;
        }
        if self.command.is_some() && !self.tools.contains(&WorkspaceTool::RunCommand) {
            return Err(DefinitionError::CommandSettingsWithoutTool);
        }
        if let Some(command) = &self.command {
            command.validate()?;
        }
        if self.mcp_servers.len() > 8 {
            return Err(DefinitionError::InvalidMcpConfig);
        }
        for (index, server) in self.mcp_servers.iter().enumerate() {
            server.validate()?;
            if self.mcp_servers[..index]
                .iter()
                .any(|other| other.id == server.id)
            {
                return Err(DefinitionError::InvalidMcpConfig);
            }
        }
        Ok(())
    }

    /// Effective command settings when `run_command` is selected; defaults fill
    /// a missing optional value.
    pub fn effective_command_settings(&self) -> Option<CommandSettings> {
        self.tools
            .contains(&WorkspaceTool::RunCommand)
            .then(|| self.command.unwrap_or_default())
    }

    /// Returns the effective initial permission mode, treating None as Ask.
    pub fn initial_permission_mode(&self) -> crate::permission::PermissionMode {
        self.permission_mode
            .unwrap_or(crate::permission::PermissionMode::Ask)
    }

    /// Construct the low-level session's explicit record and lifetime bounds.
    pub fn session_config(&self) -> SessionConfig {
        SessionConfig {
            retained_work_capacity: self.retained_work_capacity,
            work_timeout: self.work_timeout_secs.map(Duration::from_secs),
        }
    }

    /// Read the render options as the provider's protocol type. The protocol
    /// types reject unknown keys and values, so a key meant for another
    /// protocol fails here instead of being ignored.
    pub(crate) fn protocol_render_options(&self) -> Result<RenderOptions, DefinitionError> {
        fn read<T: serde::de::DeserializeOwned>(
            table: &serde_json::Map<String, serde_json::Value>,
        ) -> Result<T, DefinitionError> {
            serde_json::from_value(serde_json::Value::Object(table.clone()))
                .map_err(|_| DefinitionError::InvalidRenderOptions)
        }
        Ok(match self.provider {
            Provider::OpenAiChat => RenderOptions::Chat(read(&self.render_options)?),
            Provider::OpenAiResponses => RenderOptions::Responses(read(&self.render_options)?),
            Provider::Anthropic => RenderOptions::Anthropic(read(&self.render_options)?),
        })
    }

    /// Construct per-work options with the saved model and explicit execution limits.
    pub fn run_options(&self) -> TurnRunOptions {
        let mut options = TurnRunOptions::new(ModelRef::new(self.model.clone()));
        options.limits = TurnLimits::new(self.max_model_rounds, self.max_tool_calls);
        options.generation.max_tokens = self.max_tokens;
        options.generation.reasoning_effort = self.reasoning_effort.clone();
        options.generation.extra_body = self.extra_body.clone();
        options
    }
}

/// Render options read for one protocol.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RenderOptions {
    Chat(ChatRenderOptions),
    Responses(ResponsesRenderOptions),
    Anthropic(AnthropicRenderOptions),
}

// Shared syntax for a stored credential reference and an MCP environment name.
pub(crate) fn valid_environment_name(name: &str) -> bool {
    let mut bytes = name.bytes();
    bytes
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == b'_')
        && bytes.all(|c| c.is_ascii_alphanumeric() || c == b'_')
}

// Endpoint syntax shared by the model endpoint and remote MCP servers.
pub(crate) fn valid_http_endpoint(endpoint: &str) -> bool {
    url::Url::parse(endpoint).is_ok_and(|url| {
        matches!(url.scheme(), "http" | "https")
            && url.host_str().is_some()
            && url.username().is_empty()
            && url.password().is_none()
            && url.query().is_none()
            && url.fragment().is_none()
            && !endpoint.chars().any(char::is_whitespace)
    })
}

/// A definition rejected before credential resolution or provider execution.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum DefinitionError {
    /// A saved workspace path cannot depend on the process working directory.
    #[error("workspace root must be an absolute nonempty path")]
    InvalidWorkspaceRoot,
    /// Enabled tools require one explicit workspace resource.
    #[error("workspace root is required when tools are enabled")]
    WorkspaceRequired,
    /// A capability may appear only once in the fixed catalog.
    #[error("workspace tool selection contains a duplicate")]
    DuplicateTool,
    /// Effective bounds cannot exceed the supported hard limits or be zero.
    #[error("workspace tool limits must be positive and no greater than the supported bounds")]
    InvalidToolLimits,
    /// No explicit nonblank model identifier was provided.
    #[error("model identifier must not be blank")]
    BlankModel,
    /// The reference is not a portable environment variable name.
    #[error("credential reference must name a valid environment variable")]
    InvalidCredentialReference,
    /// Endpoint syntax or embedded credential-bearing components are forbidden.
    #[error("endpoint must be an absolute HTTP(S) URL without credentials, query or fragment")]
    InvalidEndpoint,
    /// A zero output limit can never produce a response.
    #[error("max_tokens must be positive when set")]
    InvalidMaxTokens,
    /// At least the first model call must be allowed.
    #[error("max_model_attempts must be positive when set")]
    InvalidModelAttempts,
    /// A render option is unknown to the provider's protocol or has an
    /// invalid value. The message omits the table's contents.
    #[error("render_options has a key or value the provider's protocol does not accept")]
    InvalidRenderOptions,
    /// The deadline cannot be represented on this platform.
    #[error("work timeout cannot be represented")]
    UnrepresentableTimeout,
    /// An MCP server snapshot is malformed, duplicated or exceeds the supported
    /// count. The message is static and omits environment values.
    #[error("MCP server configuration is invalid")]
    InvalidMcpConfig,
    /// Command settings are zero, inverted or above the hard timeout ceiling.
    #[error("command settings require 1 <= default timeout <= maximum timeout <= 600 seconds")]
    InvalidCommandSettings,
    /// Command settings may not be present without selecting `run_command`.
    #[error("command settings require the run_command tool")]
    CommandSettingsWithoutTool,
}
