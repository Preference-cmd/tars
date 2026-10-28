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
    #[serde(alias = "auto")]
    Approve,
}

/// Per-tool approval override within one MCP server.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct McpToolSetting {
    /// Approval used for this tool instead of the server's setting.
    pub approval: McpApproval,
}

/// Saved snapshot shape that also accepts the older `env_vars` and
/// `bearer_token_env_var` fields, which convert to `${VAR}` references.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawMcpServerConfig {
    id: String,
    #[serde(default)]
    command: Option<String>,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    url: Option<String>,
    #[serde(default)]
    env: BTreeMap<String, String>,
    #[serde(default)]
    env_vars: Vec<String>,
    #[serde(default)]
    bearer_token_env_var: Option<String>,
    #[serde(default)]
    headers: BTreeMap<String, String>,
    startup_timeout_sec: u64,
    tool_timeout_sec: u64,
    #[serde(default)]
    enabled_tools: Option<Vec<String>>,
    #[serde(default)]
    disabled_tools: Option<Vec<String>>,
    approval: McpApproval,
    #[serde(default)]
    tools: BTreeMap<String, McpToolSetting>,
    required: bool,
}

impl TryFrom<RawMcpServerConfig> for McpServerConfig {
    type Error = DefinitionError;

    fn try_from(raw: RawMcpServerConfig) -> Result<Self, Self::Error> {
        let mut env = raw.env;
        for var in raw.env_vars {
            env.insert(var.clone(), format!("${{{var}}}"));
        }
        let mut headers = raw.headers;
        if let Some(token_var) = raw.bearer_token_env_var {
            headers.insert(
                "Authorization".to_string(),
                format!("Bearer ${{{token_var}}}"),
            );
        }
        Ok(Self {
            id: raw.id,
            command: raw.command,
            args: raw.args,
            url: raw.url,
            env,
            headers,
            startup_timeout_sec: raw.startup_timeout_sec,
            tool_timeout_sec: raw.tool_timeout_sec,
            enabled_tools: raw.enabled_tools,
            disabled_tools: raw.disabled_tools,
            approval: raw.approval,
            tools: raw.tools,
            required: raw.required,
        })
    }
}

/// One MCP server's frozen connection and approval settings.
///
/// Exactly one of `command` (a local stdio server) and `url` (a remote HTTP
/// server) must be set. Secrets are never stored: `${VAR}` templates reference
/// environment variables resolved when the session opens, while literal values
/// are non-secret settings.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "RawMcpServerConfig")]
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
    /// Environment values for a stdio server.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
    /// Custom HTTP request headers sent with every request to an HTTP server.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub headers: BTreeMap<String, String>,
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
                    || command.contains("${")
                    || !self.headers.is_empty()
                {
                    return Err(DefinitionError::InvalidMcpConfig);
                }
            }
            (None, Some(url)) => {
                if !self.args.is_empty()
                    || !self.env.is_empty()
                    || url.is_empty()
                    || url.contains('\0')
                    || !valid_url_template(url)
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
        if self.env.values().any(|val| validate_template(val).is_err()) {
            return Err(DefinitionError::InvalidMcpConfig);
        }
        if self.args.iter().any(|arg| validate_template(arg).is_err()) {
            return Err(DefinitionError::InvalidMcpConfig);
        }
        let mut seen_headers = std::collections::BTreeSet::new();
        for (name, val) in &self.headers {
            if !seen_headers.insert(name.to_ascii_lowercase()) {
                return Err(DefinitionError::InvalidMcpConfig);
            }
            if !valid_header_name(name) || is_transport_header(name) {
                return Err(DefinitionError::InvalidMcpConfig);
            }
            if !valid_header_value_template(val) {
                return Err(DefinitionError::InvalidMcpConfig);
            }
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

/// Validate the `${...}` template syntax in a string.
pub(crate) fn validate_template(s: &str) -> Result<(), &'static str> {
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'$' && i + 1 < bytes.len() && bytes[i + 1] == b'{' {
            let start = i + 2;
            let Some(end) = s[start..].find('}') else {
                return Err("unclosed variable reference");
            };
            let content = &s[start..start + end];
            let (var_name, default_val) = match content.split_once(":-") {
                Some((name, default)) => (name, Some(default)),
                None => (content, None),
            };
            if !valid_environment_name(var_name) {
                return Err("invalid environment variable name");
            }
            if let Some(def) = default_val
                && def.contains("${")
            {
                return Err("nested variable references are not supported");
            }
            i = start + end + 1;
            continue;
        }
        i += 1;
    }
    Ok(())
}

/// Expand `${VAR}` and `${VAR:-default}` references in `s` using the process environment,
/// recording each substituted `(pattern, expanded_value)` into `substitutions`.
pub(crate) fn expand_template_recording(
    s: &str,
    substitutions: &mut Vec<(String, String)>,
) -> Result<String, String> {
    let mut result = String::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'$' && i + 1 < bytes.len() && bytes[i + 1] == b'{' {
            let start = i + 2;
            let end = s[start..]
                .find('}')
                .ok_or_else(|| "unclosed variable reference".to_string())?;
            let content = &s[start..start + end];
            let (var_name, default_val) = match content.split_once(":-") {
                Some((name, default)) => (name, Some(default)),
                None => (content, None),
            };
            if !valid_environment_name(var_name) {
                return Err(format!("invalid environment variable name '{var_name}'"));
            }
            let val = if let Some(val) = std::env::var(var_name).ok().filter(|v| !v.is_empty()) {
                val
            } else if let Some(def) = default_val {
                def.to_string()
            } else {
                return Err(format!("environment variable {var_name} is not set"));
            };
            if !val.is_empty() {
                substitutions.push((format!("${{{var_name}}}"), val.clone()));
            }
            result.push_str(&val);
            i = start + end + 1;
        } else {
            let ch = s[i..].chars().next().unwrap();
            result.push(ch);
            i += ch.len_utf8();
        }
    }
    Ok(result)
}

#[cfg(test)]
pub(crate) fn expand_template(s: &str) -> Result<String, String> {
    expand_template_recording(s, &mut Vec::new())
}

/// Redact substituted variable values from text displayed externally.
///
/// Any substituted variable value (from `url`, `headers`, `args`, or `env`)
/// is replaced back with its `${VAR}` template representation.
///
/// Longer values are replaced first so that shorter values that happen to be
/// substrings of longer values do not prevent full replacement.
/// Values shorter than 4 bytes are not replaced to avoid mangling ordinary text.
pub(crate) fn redact_substitutions(text: &str, substitutions: &[(String, String)]) -> String {
    if substitutions.is_empty() {
        return text.to_owned();
    }
    let mut eligible: Vec<(&str, &str)> = substitutions
        .iter()
        .filter_map(|(pattern, value)| {
            if value.len() >= 4 {
                Some((pattern.as_str(), value.as_str()))
            } else {
                None
            }
        })
        .collect();

    eligible.sort_by_key(|b| std::cmp::Reverse(b.1.len()));

    let mut result = text.to_owned();
    for (pattern, value) in eligible {
        result = result.replace(value, pattern);
    }
    result
}

/// Check RFC 9110 tchar byte.
pub(crate) fn is_tchar(b: u8) -> bool {
    b.is_ascii_alphanumeric()
        || matches!(
            b,
            b'!' | b'#'
                | b'$'
                | b'%'
                | b'&'
                | b'\''
                | b'*'
                | b'+'
                | b'-'
                | b'.'
                | b'^'
                | b'_'
                | b'`'
                | b'|'
                | b'~'
        )
}

pub(crate) fn valid_header_name(name: &str) -> bool {
    !name.is_empty() && name.bytes().all(is_tchar)
}

pub(crate) fn is_transport_header(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    lower == "accept"
        || lower == "content-type"
        || lower == "last-event-id"
        || lower.starts_with("mcp-")
}

pub(crate) fn valid_header_value_template(value: &str) -> bool {
    if validate_template(value).is_err() {
        return false;
    }
    value
        .bytes()
        .all(|b| b == b'\t' || (0x20..=0x7E).contains(&b) || b >= 0x80)
}

/// Validate URL template by ensuring variable syntax is valid and substituting dummy
/// values to test endpoint rules.
pub(crate) fn valid_url_template(url: &str) -> bool {
    if validate_template(url).is_err() {
        return false;
    }
    let mut substituted = String::with_capacity(url.len());
    let bytes = url.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'$' && i + 1 < bytes.len() && bytes[i + 1] == b'{' {
            let start = i + 2;
            let Some(end) = url[start..].find('}') else {
                return false;
            };
            if i == 0 {
                if start + end + 1 == url.len() {
                    substituted.push_str("https://example.invalid/mcp");
                } else if url[start + end + 1..].starts_with("://") {
                    substituted.push_str("https");
                } else {
                    substituted.push_str("placeholder.invalid");
                }
            } else {
                substituted.push_str("placeholder.invalid");
            }
            i = start + end + 1;
        } else {
            let ch = url[i..].chars().next().unwrap();
            substituted.push(ch);
            i += ch.len_utf8();
        }
    }
    valid_http_endpoint(&substituted)
}
