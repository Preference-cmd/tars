use super::types::{DefinitionError, valid_environment_name, valid_http_endpoint};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// How calls to one MCP server's tools are approved.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum McpApproval {
    /// Ask before a call, subject to the current permission mode.
    #[default]
    Prompt,
    /// Treat tools the server declares read-only as read-only; ask for others.
    Writes,
    /// Execute without asking.
    Auto,
}

/// Per-tool approval override within one MCP server.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct McpToolSetting {
    /// Approval used for this tool instead of the server's setting.
    pub approval: McpApproval,
}

/// One MCP server's frozen connection and approval settings.
///
/// Exactly one of `command` (a local stdio server) and `url` (a remote HTTP
/// server) must be set. Secrets are never stored: `env_vars` and
/// `bearer_token_env_var` name environment variables resolved when the session
/// opens, while `env` holds only non-secret literal values.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct McpServerConfig {
    /// Stable server identifier used in tool names and configuration selections.
    pub id: String,
    /// Executable started for a stdio server; mutually exclusive with `url`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    /// Arguments passed to the stdio server command.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<String>,
    /// Endpoint of an HTTP server; mutually exclusive with `command`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// Non-secret environment values for a stdio server.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
    /// Environment variable names forwarded from the Tarz process to a stdio server.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub env_vars: Vec<String>,
    /// Environment variable holding an HTTP bearer token; the token is never stored.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bearer_token_env_var: Option<String>,
    /// Server startup deadline in seconds.
    pub startup_timeout_sec: u64,
    /// Per-call deadline in seconds.
    pub tool_timeout_sec: u64,
    /// Allowed tool names, or None to allow every tool the server lists.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled_tools: Option<Vec<String>>,
    /// Excluded tool names, or None to exclude none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disabled_tools: Option<Vec<String>>,
    /// Server-wide approval setting.
    pub approval: McpApproval,
    /// Per-tool approval overrides, keyed by the server's tool name.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub tools: BTreeMap<String, McpToolSetting>,
    /// Whether a startup failure must prevent the session from opening.
    pub required: bool,
}

impl McpServerConfig {
    /// Validate this snapshot without resolving variables, starting a process or
    /// connecting to the server. Error messages are static so they never echo
    /// environment values.
    pub fn validate(&self) -> Result<(), DefinitionError> {
        if !valid_server_id(&self.id) {
            return Err(DefinitionError::InvalidMcpConfig);
        }
        match (&self.command, &self.url) {
            (Some(command), None) => {
                if command.is_empty()
                    || command.contains('\0')
                    || self.bearer_token_env_var.is_some()
                {
                    return Err(DefinitionError::InvalidMcpConfig);
                }
            }
            (None, Some(url)) => {
                if !self.args.is_empty()
                    || !self.env.is_empty()
                    || !self.env_vars.is_empty()
                    || !valid_http_endpoint(url)
                {
                    return Err(DefinitionError::InvalidMcpConfig);
                }
                if self
                    .bearer_token_env_var
                    .as_deref()
                    .is_some_and(|name| !valid_environment_name(name))
                {
                    return Err(DefinitionError::InvalidMcpConfig);
                }
            }
            _ => return Err(DefinitionError::InvalidMcpConfig),
        }
        if !(1..=60).contains(&self.startup_timeout_sec)
            || !(1..=3600).contains(&self.tool_timeout_sec)
        {
            return Err(DefinitionError::InvalidMcpConfig);
        }
        if self.env.keys().any(|name| !valid_environment_name(name)) {
            return Err(DefinitionError::InvalidMcpConfig);
        }
        if (0..self.env_vars.len()).any(|index| {
            !valid_environment_name(&self.env_vars[index])
                || self.env_vars[..index].contains(&self.env_vars[index])
        }) {
            return Err(DefinitionError::InvalidMcpConfig);
        }
        if self.tools.keys().any(String::is_empty) {
            return Err(DefinitionError::InvalidMcpConfig);
        }
        match (&self.enabled_tools, &self.disabled_tools) {
            (Some(_), Some(_)) => return Err(DefinitionError::InvalidMcpConfig),
            (Some(enabled), None) => {
                if !unique_nonempty(enabled)
                    || self
                        .tools
                        .keys()
                        .any(|tool| !enabled.iter().any(|name| name == tool))
                {
                    return Err(DefinitionError::InvalidMcpConfig);
                }
            }
            (None, Some(disabled)) => {
                if !unique_nonempty(disabled)
                    || self
                        .tools
                        .keys()
                        .any(|tool| disabled.iter().any(|name| name == tool))
                {
                    return Err(DefinitionError::InvalidMcpConfig);
                }
            }
            (None, None) => {}
        }
        Ok(())
    }
}

/// A server id is a short lowercase ASCII token usable in tool names.
pub(crate) fn valid_server_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 16
        && id
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
}

fn unique_nonempty(names: &[String]) -> bool {
    let mut seen = std::collections::BTreeSet::new();
    !names.is_empty()
        && names
            .iter()
            .all(|name| !name.is_empty() && seen.insert(name.as_str()))
}
