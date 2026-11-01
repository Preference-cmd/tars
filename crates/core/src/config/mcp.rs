use super::file::ConfigError;
use crate::definition::{
    McpApproval, McpServerConfig, McpToolSetting, is_transport_header, valid_environment_name,
    valid_header_name, valid_header_value_template, valid_server_id, valid_url_template,
    validate_template,
};
use std::collections::{BTreeMap, BTreeSet};

const SECRET_WORDS: &[&str] = &[
    "token",
    "key",
    "apikey",
    "secret",
    "password",
    "passwd",
    "credential",
    "credentials",
];

fn has_secret_segment(name: &str, delimiter: char) -> bool {
    name.split(delimiter).any(|seg| {
        SECRET_WORDS
            .iter()
            .any(|word| seg.eq_ignore_ascii_case(word))
    })
}

fn has_authorization_segment(name: &str) -> bool {
    name.split('-')
        .any(|seg| seg.eq_ignore_ascii_case("authorization"))
}

fn is_valid_authorization_value(val: &str) -> bool {
    let parts: Vec<&str> = val.split_whitespace().collect();
    if parts.len() != 2 {
        return false;
    }
    let scheme = parts[0];
    let var_part = parts[1];
    if scheme.is_empty() || scheme.contains("${") {
        return false;
    }
    is_exact_var_without_default(var_part)
}

fn is_exact_var_without_default(val: &str) -> bool {
    let val = val.trim();
    if let Some(content) = val.strip_prefix("${").and_then(|s| s.strip_suffix('}')) {
        !content.contains(":-") && valid_environment_name(content)
    } else {
        false
    }
}

/// Parse and validate an `mcp.json` text.
///
/// Returns the servers and the warnings for ignored foreign fields. Errors omit
/// all argument, header, environment, and URL values.
pub(super) fn parse_mcp_json(
    text: &str,
) -> Result<(BTreeMap<String, McpServerConfig>, Vec<String>), ConfigError> {
    let root: serde_json::Value =
        serde_json::from_str(text).map_err(|error| ConfigError::McpSyntax {
            line: error.line(),
            column: error.column(),
        })?;

    let root_map = root
        .as_object()
        .ok_or_else(|| ConfigError::InvalidMcpFile("mcp.json root must be an object".into()))?;

    for key in root_map.keys() {
        if key != "mcpServers" {
            return Err(ConfigError::InvalidMcpFile(format!(
                "unknown top-level key '{key}'; only 'mcpServers' is allowed"
            )));
        }
    }

    let Some(servers_val) = root_map.get("mcpServers") else {
        return Ok((BTreeMap::new(), Vec::new()));
    };

    if servers_val.is_null() {
        return Ok((BTreeMap::new(), Vec::new()));
    }

    let servers_map = servers_val
        .as_object()
        .ok_or_else(|| ConfigError::InvalidMcpFile("'mcpServers' must be an object".into()))?;

    let mut warnings = Vec::new();
    let mut mcp_servers = BTreeMap::new();

    for (id, server_val) in servers_map {
        if !valid_server_id(id) {
            return Err(ConfigError::InvalidMcpFile(format!(
                "invalid MCP server id '{id}'"
            )));
        }

        let server_map = server_val.as_object().ok_or_else(|| {
            ConfigError::InvalidMcpFile(format!("MCP server '{id}' must be an object"))
        })?;

        // Reject unsupported keys; timeout has a dedicated hint directing to startup/tool timeouts.
        for key in server_map.keys() {
            match key.as_str() {
                "type" | "command" | "args" | "env" | "url" | "headers" | "approval" | "tools"
                | "enabledTools" | "disabledTools" | "required" | "startupTimeoutSec"
                | "toolTimeoutSec" | "disabled" | "autoApprove" | "alwaysAllow" | "trust"
                | "description" => {}
                "timeout" => {
                    return Err(ConfigError::InvalidMcpFile(format!(
                        "MCP server '{id}' has unknown key 'timeout'; use 'startupTimeoutSec' and 'toolTimeoutSec'"
                    )));
                }
                _ => {
                    return Err(ConfigError::InvalidMcpFile(format!(
                        "MCP server '{id}' has unknown key '{key}'"
                    )));
                }
            }
        }

        // Warn on known foreign configuration keys without relaxing permissions.
        if server_map.contains_key("disabled") {
            warnings.push(format!(
                "TARS: mcp.json server '{id}': ignoring 'disabled'; server selection is controlled by profile or --mcp"
            ));
        }
        if server_map.contains_key("autoApprove") {
            warnings.push(format!(
                "TARS: mcp.json server '{id}': ignoring 'autoApprove'; approval is set by TARS's 'approval'"
            ));
        }
        if server_map.contains_key("alwaysAllow") {
            warnings.push(format!(
                "TARS: mcp.json server '{id}': ignoring 'alwaysAllow'; approval is set by TARS's 'approval'"
            ));
        }
        if server_map.contains_key("trust") {
            warnings.push(format!(
                "TARS: mcp.json server '{id}': ignoring 'trust'; permissions are controlled by TARS's 'approval'"
            ));
        }
        if server_map.contains_key("description") {
            warnings.push(format!(
                "TARS: mcp.json server '{id}': ignoring 'description'; descriptions are provided by MCP tools"
            ));
        }

        let transport_type = if let Some(type_val) = server_map.get("type") {
            let type_str = type_val.as_str().ok_or_else(|| {
                ConfigError::InvalidMcpFile(format!("MCP server '{id}': 'type' must be a string"))
            })?;
            match type_str {
                "sse" => {
                    return Err(ConfigError::InvalidMcpFile(format!(
                        "MCP server '{id}': type 'sse' is not supported by Causa"
                    )));
                }
                "stdio" | "http" | "streamable-http" => Some(type_str),
                _ => {
                    return Err(ConfigError::InvalidMcpFile(format!(
                        "MCP server '{id}': unknown type; expected 'stdio', 'http', or 'streamable-http'"
                    )));
                }
            }
        } else {
            None
        };

        let command = if let Some(cmd_val) = server_map.get("command") {
            let cmd_str = cmd_val.as_str().ok_or_else(|| {
                ConfigError::InvalidMcpFile(format!(
                    "MCP server '{id}': 'command' must be a string"
                ))
            })?;
            if cmd_str.is_empty() || cmd_str.contains('\0') {
                return Err(ConfigError::InvalidMcpFile(format!(
                    "MCP server '{id}': 'command' must be a non-empty string without null bytes"
                )));
            }
            if cmd_str.contains("${") {
                return Err(ConfigError::InvalidMcpFile(format!(
                    "MCP server '{id}': command does not expand variables"
                )));
            }
            Some(cmd_str.to_owned())
        } else {
            None
        };

        let url = if let Some(url_val) = server_map.get("url") {
            let url_str = url_val.as_str().ok_or_else(|| {
                ConfigError::InvalidMcpFile(format!("MCP server '{id}': 'url' must be a string"))
            })?;
            if url_str.is_empty() || url_str.contains('\0') {
                return Err(ConfigError::InvalidMcpFile(format!(
                    "MCP server '{id}': 'url' must be a non-empty string without null bytes"
                )));
            }
            if !valid_url_template(url_str) {
                return Err(ConfigError::InvalidMcpFile(format!(
                    "MCP server '{id}': 'url' must be an HTTP(S) URL template"
                )));
            }
            Some(url_str.to_owned())
        } else {
            None
        };

        match (command.is_some(), url.is_some()) {
            (true, true) => {
                return Err(ConfigError::InvalidMcpFile(format!(
                    "MCP server '{id}': 'command' and 'url' are mutually exclusive"
                )));
            }
            (false, false) => {
                return Err(ConfigError::InvalidMcpFile(format!(
                    "MCP server '{id}': exactly one of 'command' or 'url' must be specified"
                )));
            }
            _ => {}
        }

        if let Some(transport) = transport_type {
            match transport {
                "stdio" if url.is_some() => {
                    return Err(ConfigError::InvalidMcpFile(format!(
                        "MCP server '{id}': type 'stdio' is incompatible with 'url'"
                    )));
                }
                "http" | "streamable-http" if command.is_some() => {
                    return Err(ConfigError::InvalidMcpFile(format!(
                        "MCP server '{id}': type '{transport}' is incompatible with 'command'"
                    )));
                }
                _ => {}
            }
        }

        if command.is_some() && server_map.contains_key("headers") {
            return Err(ConfigError::InvalidMcpFile(format!(
                "MCP server '{id}': 'headers' is not allowed for stdio servers"
            )));
        }

        if url.is_some() {
            if server_map.contains_key("args") {
                return Err(ConfigError::InvalidMcpFile(format!(
                    "MCP server '{id}': 'args' is not allowed for HTTP servers"
                )));
            }
            if server_map.contains_key("env") {
                return Err(ConfigError::InvalidMcpFile(format!(
                    "MCP server '{id}': 'env' is not allowed for HTTP servers"
                )));
            }
        }

        let args = if let Some(args_val) = server_map.get("args") {
            let args_arr = args_val.as_array().ok_or_else(|| {
                ConfigError::InvalidMcpFile(format!(
                    "MCP server '{id}': 'args' must be an array of strings"
                ))
            })?;
            let mut list = Vec::with_capacity(args_arr.len());
            for item in args_arr {
                let s = item.as_str().ok_or_else(|| {
                    ConfigError::InvalidMcpFile(format!(
                        "MCP server '{id}': 'args' must be an array of strings"
                    ))
                })?;
                if validate_template(s).is_err() {
                    return Err(ConfigError::InvalidMcpFile(format!(
                        "MCP server '{id}': invalid variable syntax in args"
                    )));
                }
                list.push(s.to_owned());
            }
            list
        } else {
            Vec::new()
        };

        // A secret-looking name must hold a ${VAR} reference, never a literal.
        let env = if let Some(env_val) = server_map.get("env") {
            let env_obj = env_val.as_object().ok_or_else(|| {
                ConfigError::InvalidMcpFile(format!(
                    "MCP server '{id}': 'env' must be an object with string values"
                ))
            })?;
            let mut map = BTreeMap::new();
            for (k, v) in env_obj {
                if !valid_environment_name(k) {
                    return Err(ConfigError::InvalidMcpFile(format!(
                        "MCP server '{id}': invalid environment variable name '{k}'"
                    )));
                }
                let val_str = v.as_str().ok_or_else(|| {
                    ConfigError::InvalidMcpFile(format!(
                        "MCP server '{id}': 'env' must be an object with string values"
                    ))
                })?;
                if validate_template(val_str).is_err() {
                    return Err(ConfigError::InvalidMcpFile(format!(
                        "MCP server '{id}': invalid variable syntax in env variable '{k}'"
                    )));
                }
                if has_secret_segment(k, '_') && !is_exact_var_without_default(val_str) {
                    return Err(ConfigError::InvalidMcpFile(format!(
                        "MCP server '{id}': environment variable '{k}' must be a single '${{VAR}}' referencing an environment variable without a default value"
                    )));
                }
                map.insert(k.clone(), val_str.to_owned());
            }
            map
        } else {
            BTreeMap::new()
        };

        let headers = if let Some(headers_val) = server_map.get("headers") {
            let headers_obj = headers_val.as_object().ok_or_else(|| {
                ConfigError::InvalidMcpFile(format!(
                    "MCP server '{id}': 'headers' must be an object with string values"
                ))
            })?;
            let mut map = BTreeMap::new();
            let mut seen_headers = BTreeSet::new();
            for (k, v) in headers_obj {
                if !seen_headers.insert(k.to_ascii_lowercase()) {
                    return Err(ConfigError::InvalidMcpFile(format!(
                        "MCP server '{id}': duplicate header name '{k}'"
                    )));
                }
                if !valid_header_name(k) {
                    return Err(ConfigError::InvalidMcpFile(format!(
                        "MCP server '{id}': invalid header name '{k}'"
                    )));
                }
                if is_transport_header(k) {
                    return Err(ConfigError::InvalidMcpFile(format!(
                        "MCP server '{id}': header '{k}' is set by the MCP transport"
                    )));
                }
                let val_str = v.as_str().ok_or_else(|| {
                    ConfigError::InvalidMcpFile(format!(
                        "MCP server '{id}': 'headers' must be an object with string values"
                    ))
                })?;
                if !valid_header_value_template(val_str) {
                    return Err(ConfigError::InvalidMcpFile(format!(
                        "MCP server '{id}': invalid value for header '{k}'"
                    )));
                }
                if has_authorization_segment(k) {
                    if !is_valid_authorization_value(val_str) {
                        return Err(ConfigError::InvalidMcpFile(format!(
                            "MCP server '{id}': header '{k}' must be '<scheme> ${{VAR}}' referencing an environment variable without a default value"
                        )));
                    }
                } else if has_secret_segment(k, '-') && !is_exact_var_without_default(val_str) {
                    return Err(ConfigError::InvalidMcpFile(format!(
                        "MCP server '{id}': header '{k}' must be a single '${{VAR}}' referencing an environment variable without a default value"
                    )));
                }
                map.insert(k.clone(), val_str.to_owned());
            }
            map
        } else {
            BTreeMap::new()
        };

        let startup_timeout_sec = if let Some(val) = server_map.get("startupTimeoutSec") {
            let timeout = val.as_u64().ok_or_else(|| {
                ConfigError::InvalidMcpFile(format!(
                    "MCP server '{id}': 'startupTimeoutSec' must be an integer between 1 and 60"
                ))
            })?;
            if !(1..=60).contains(&timeout) {
                return Err(ConfigError::InvalidMcpFile(format!(
                    "MCP server '{id}': 'startupTimeoutSec' must be an integer between 1 and 60"
                )));
            }
            timeout
        } else {
            10
        };

        let tool_timeout_sec = if let Some(val) = server_map.get("toolTimeoutSec") {
            let timeout = val.as_u64().ok_or_else(|| {
                ConfigError::InvalidMcpFile(format!(
                    "MCP server '{id}': 'toolTimeoutSec' must be an integer between 1 and 3600"
                ))
            })?;
            if !(1..=3600).contains(&timeout) {
                return Err(ConfigError::InvalidMcpFile(format!(
                    "MCP server '{id}': 'toolTimeoutSec' must be an integer between 1 and 3600"
                )));
            }
            timeout
        } else {
            60
        };

        let approval = if let Some(app_val) = server_map.get("approval") {
            let app_str = app_val.as_str().ok_or_else(|| {
                ConfigError::InvalidMcpFile(format!(
                    "MCP server '{id}': 'approval' must be 'prompt', 'writes', or 'approve'"
                ))
            })?;
            match app_str {
                "auto" => {
                    return Err(ConfigError::InvalidMcpFile(format!(
                        "MCP server '{id}': approval 'auto' is not supported; use 'approve'"
                    )));
                }
                "prompt" => McpApproval::Prompt,
                "writes" => McpApproval::Writes,
                "approve" => McpApproval::Approve,
                _ => {
                    return Err(ConfigError::InvalidMcpFile(format!(
                        "MCP server '{id}': invalid approval; expected 'prompt', 'writes', or 'approve'"
                    )));
                }
            }
        } else {
            McpApproval::Prompt
        };

        let tools = if let Some(tools_val) = server_map.get("tools") {
            let tools_obj = tools_val.as_object().ok_or_else(|| {
                ConfigError::InvalidMcpFile(format!("MCP server '{id}': 'tools' must be an object"))
            })?;
            let mut map = BTreeMap::new();
            for (tool_name, setting_val) in tools_obj {
                if tool_name.is_empty() {
                    return Err(ConfigError::InvalidMcpFile(format!(
                        "MCP server '{id}': tool name must not be empty"
                    )));
                }
                let setting_obj = setting_val.as_object().ok_or_else(|| {
                    ConfigError::InvalidMcpFile(format!(
                        "MCP server '{id}': setting for tool '{tool_name}' must be an object"
                    ))
                })?;
                for key in setting_obj.keys() {
                    if key != "approval" {
                        return Err(ConfigError::InvalidMcpFile(format!(
                            "MCP server '{id}': tool '{tool_name}' has unknown key '{key}'; only 'approval' is supported"
                        )));
                    }
                }
                let tool_approval = if let Some(app_val) = setting_obj.get("approval") {
                    let app_str = app_val.as_str().ok_or_else(|| {
                        ConfigError::InvalidMcpFile(format!(
                            "MCP server '{id}': tool '{tool_name}' approval must be 'prompt', 'writes', or 'approve'"
                        ))
                    })?;
                    match app_str {
                        "auto" => {
                            return Err(ConfigError::InvalidMcpFile(format!(
                                "MCP server '{id}': tool '{tool_name}' approval 'auto' is not supported; use 'approve'"
                            )));
                        }
                        "prompt" => McpApproval::Prompt,
                        "writes" => McpApproval::Writes,
                        "approve" => McpApproval::Approve,
                        _ => {
                            return Err(ConfigError::InvalidMcpFile(format!(
                                "MCP server '{id}': tool '{tool_name}' has invalid approval; expected 'prompt', 'writes', or 'approve'"
                            )));
                        }
                    }
                } else {
                    approval
                };
                map.insert(
                    tool_name.clone(),
                    McpToolSetting {
                        approval: tool_approval,
                    },
                );
            }
            map
        } else {
            BTreeMap::new()
        };

        let enabled_tools = if let Some(val) = server_map.get("enabledTools") {
            let arr = val.as_array().ok_or_else(|| {
                ConfigError::InvalidMcpFile(format!(
                    "MCP server '{id}': 'enabledTools' must be an array of strings"
                ))
            })?;
            if arr.is_empty() {
                return Err(ConfigError::InvalidMcpFile(format!(
                    "MCP server '{id}': 'enabledTools' must not be empty"
                )));
            }
            let mut list = Vec::with_capacity(arr.len());
            let mut seen = BTreeSet::new();
            for item in arr {
                let name = item.as_str().ok_or_else(|| {
                    ConfigError::InvalidMcpFile(format!(
                        "MCP server '{id}': 'enabledTools' must be an array of strings"
                    ))
                })?;
                if name.is_empty() || !seen.insert(name) {
                    return Err(ConfigError::InvalidMcpFile(format!(
                        "MCP server '{id}': 'enabledTools' items must be non-empty and unique"
                    )));
                }
                list.push(name.to_owned());
            }
            Some(list)
        } else {
            None
        };

        let disabled_tools = if let Some(val) = server_map.get("disabledTools") {
            let arr = val.as_array().ok_or_else(|| {
                ConfigError::InvalidMcpFile(format!(
                    "MCP server '{id}': 'disabledTools' must be an array of strings"
                ))
            })?;
            if arr.is_empty() {
                return Err(ConfigError::InvalidMcpFile(format!(
                    "MCP server '{id}': 'disabledTools' must not be empty"
                )));
            }
            let mut list = Vec::with_capacity(arr.len());
            let mut seen = BTreeSet::new();
            for item in arr {
                let name = item.as_str().ok_or_else(|| {
                    ConfigError::InvalidMcpFile(format!(
                        "MCP server '{id}': 'disabledTools' must be an array of strings"
                    ))
                })?;
                if name.is_empty() || !seen.insert(name) {
                    return Err(ConfigError::InvalidMcpFile(format!(
                        "MCP server '{id}': 'disabledTools' items must be non-empty and unique"
                    )));
                }
                list.push(name.to_owned());
            }
            Some(list)
        } else {
            None
        };

        match (&enabled_tools, &disabled_tools) {
            (Some(_), Some(_)) => {
                return Err(ConfigError::InvalidMcpFile(format!(
                    "MCP server '{id}': 'enabledTools' and 'disabledTools' are mutually exclusive"
                )));
            }
            (Some(enabled), None) => {
                if tools
                    .keys()
                    .any(|tool| !enabled.iter().any(|name| name == tool))
                {
                    return Err(ConfigError::InvalidMcpFile(format!(
                        "MCP server '{id}': tool overrides must be included in 'enabledTools'"
                    )));
                }
            }
            (None, Some(disabled)) => {
                if tools
                    .keys()
                    .any(|tool| disabled.iter().any(|name| name == tool))
                {
                    return Err(ConfigError::InvalidMcpFile(format!(
                        "MCP server '{id}': tool overrides must not be included in 'disabledTools'"
                    )));
                }
            }
            (None, None) => {}
        }

        let required = if let Some(req_val) = server_map.get("required") {
            req_val.as_bool().ok_or_else(|| {
                ConfigError::InvalidMcpFile(format!(
                    "MCP server '{id}': 'required' must be a boolean"
                ))
            })?
        } else {
            false
        };

        let server_config = McpServerConfig {
            id: id.clone(),
            command,
            args,
            url,
            env,
            headers,
            startup_timeout_sec,
            tool_timeout_sec,
            enabled_tools,
            disabled_tools,
            approval,
            tools,
            required,
        };

        server_config
            .validate()
            .map_err(|error| ConfigError::InvalidMcpFile(format!("MCP server '{id}': {error}")))?;

        mcp_servers.insert(id.clone(), server_config);
    }

    Ok((mcp_servers, warnings))
}
