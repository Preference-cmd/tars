//! Read-only, zero-network environment and configuration diagnostics (`/doctor`).
//!
//! All checks inspect only local state (process environment, configuration,
//! SQLite metadata in read-only mode, filesystem permissions, and binary
//! availability) without making network requests, writing files, spawning
//! untrusted MCP servers, or echoing credential values.

use crate::agents::{AgentDiscovery, scan_workspace_agents};
use crate::config::{Config, default_config_path};
use crate::definition::{
    CredentialRef, McpServerConfig, Provider, SessionDefinition, WorkspaceTool,
    valid_environment_name,
};
use crate::external_tools::{ExternalTools, ToolSource};
use crate::permission::rules::load_rules_file;
use crate::sandbox::{BackendId, Sandbox, SandboxStatus};
use crate::skills::SkillDiscovery;
use crate::store::schema;
use std::collections::BTreeSet;
use std::ffi::OsStr;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

#[cfg(test)]
#[path = "doctor_tests.rs"]
mod tests;

/// Severity outcome of an individual diagnostic check.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DoctorStatus {
    /// Check passed with no issues.
    Ok,
    /// Non-fatal condition or optional capability that may degrade behavior.
    Warn,
    /// Misconfiguration or missing requirement that blocks normal operation.
    Fail,
}

impl DoctorStatus {
    /// Stable short status tag rendered in diagnostic reports.
    pub fn tag(&self) -> &'static str {
        match self {
            Self::Ok => "[ok]",
            Self::Warn => "[warn]",
            Self::Fail => "[fail]",
        }
    }
}

/// A single diagnostic check result.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DoctorItem {
    /// Coarse diagnostic section (e.g. `"runtime"`, `"config"`, `"credentials"`).
    pub category: &'static str,
    /// Human-readable check label.
    pub label: String,
    /// Outcome status.
    pub status: DoctorStatus,
    /// Concise factual description (never contains secret values).
    pub detail: String,
    /// Optional actionable remediation hint.
    pub hint: Option<String>,
}

/// Complete read-only diagnostic report returned by [`run_diagnostics`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DoctorReport {
    /// Ordered list of diagnostic check items.
    pub items: Vec<DoctorItem>,
}

impl DoctorReport {
    /// Count items with [`DoctorStatus::Ok`].
    pub fn ok_count(&self) -> usize {
        self.items
            .iter()
            .filter(|item| item.status == DoctorStatus::Ok)
            .count()
    }

    /// Count items with [`DoctorStatus::Warn`].
    pub fn warn_count(&self) -> usize {
        self.items
            .iter()
            .filter(|item| item.status == DoctorStatus::Warn)
            .count()
    }

    /// Count items with [`DoctorStatus::Fail`].
    pub fn fail_count(&self) -> usize {
        self.items
            .iter()
            .filter(|item| item.status == DoctorStatus::Fail)
            .count()
    }

    /// Render the report into human-readable `[ok]` / `[warn]` / `[fail]` lines
    /// followed by a summary line.
    pub fn format_lines(&self) -> Vec<String> {
        let mut lines = Vec::with_capacity(self.items.len() * 2 + 1);
        for item in &self.items {
            lines.push(format!(
                "{} [{}] {}: {}",
                item.status.tag(),
                item.category,
                item.label,
                item.detail
            ));
            if let Some(hint) = &item.hint {
                lines.push(format!("       hint: {hint}"));
            }
        }
        lines.push(format!(
            "Summary: {} ok, {} warn, {} fail",
            self.ok_count(),
            self.warn_count(),
            self.fail_count()
        ));
        lines
    }
}

/// Explicit inputs passed to [`run_diagnostics`].
pub struct DoctorInput<'a> {
    /// Running TARS application version string.
    pub version: &'a str,
    /// Loaded startup configuration.
    pub config: &'a Config,
    /// Explicit configuration file path if known, otherwise `config.source()` is inspected.
    pub config_path: Option<&'a Path>,
    /// Whether configuration loading was disabled via `--no-config`.
    pub config_disabled: bool,
    /// Selected profile name, or `None` to inspect the configured default profile.
    pub profile: Option<&'a str>,
    /// Target store root directory.
    pub store_dir: &'a Path,
    /// Target recovery output directory.
    pub recovery_dir: &'a Path,
    /// Active or explicitly selected workspace root, if any.
    pub workspace_root: Option<&'a Path>,
    /// Active workspace tool selection, if overriding the profile's list.
    pub active_tools: Option<&'a [WorkspaceTool]>,
    /// Pre-probed sandbox status, or `None` to probe `Sandbox::open()` directly.
    pub sandbox: Option<&'a SandboxStatus>,
    /// Pre-resolved external tools status, or `None` to resolve from `config`.
    pub external_tools: Option<&'a ExternalTools>,
    /// Optional path to user-level `permissions.toml`.
    pub user_rules_path: Option<&'a Path>,
}

/// Run read-only, zero-network diagnostics across runtime, configuration,
/// credentials, storage, workspace tools, sandbox, external tools, permissions,
/// MCP, and prelude assets.
pub fn run_diagnostics(input: &DoctorInput<'_>) -> DoctorReport {
    let mut items = Vec::new();

    check_runtime(input, &mut items);
    let resolved_profile = check_config(input, &mut items);
    check_credentials(input, resolved_profile.as_ref(), &mut items);
    check_storage(input, &mut items);

    let probed_sandbox = match input.sandbox {
        Some(status) => status.clone(),
        None => match Sandbox::open() {
            Ok(sb) => SandboxStatus::Available(sb.report().clone()),
            Err(err) => SandboxStatus::Unavailable(err),
        },
    };

    check_workspace_and_tools(
        input,
        resolved_profile.as_ref(),
        &probed_sandbox,
        &mut items,
    );
    check_sandbox(&probed_sandbox, &mut items);
    check_external_tools(input, &mut items);
    check_permissions_mcp_prelude(input, resolved_profile.as_ref(), &mut items);

    DoctorReport { items }
}

fn push_item(
    items: &mut Vec<DoctorItem>,
    category: &'static str,
    label: impl Into<String>,
    status: DoctorStatus,
    detail: impl Into<String>,
    hint: Option<String>,
) {
    items.push(DoctorItem {
        category,
        label: label.into(),
        status,
        detail: detail.into(),
        hint,
    });
}

fn check_runtime(input: &DoctorInput<'_>, items: &mut Vec<DoctorItem>) {
    push_item(
        items,
        "runtime",
        "TARS",
        DoctorStatus::Ok,
        format!(
            "version {} ({}/{})",
            input.version,
            std::env::consts::OS,
            std::env::consts::ARCH
        ),
        None,
    );

    if let Some(val) = std::env::var_os("TARS_HOME") {
        let path = PathBuf::from(&val);
        if val.is_empty() {
            push_item(
                items,
                "runtime",
                "Env TARS_HOME",
                DoctorStatus::Warn,
                "set to empty value (ignored; falling back to default)",
                Some("unset TARS_HOME or set it to an absolute directory path".into()),
            );
        } else if !path.is_absolute() {
            push_item(
                items,
                "runtime",
                "Env TARS_HOME",
                DoctorStatus::Warn,
                "set to a relative path (ignored; falling back to default)",
                Some("set TARS_HOME to an absolute directory path".into()),
            );
        } else {
            push_item(
                items,
                "runtime",
                "Env TARS_HOME",
                DoctorStatus::Ok,
                format!("absolute path ({})", path.display()),
                None,
            );
        }
    }

    if let Some(val) = std::env::var_os("TARS_CONFIG") {
        let path = PathBuf::from(&val);
        if val.is_empty() {
            push_item(
                items,
                "runtime",
                "Env TARS_CONFIG",
                DoctorStatus::Warn,
                "set to empty value",
                Some("unset TARS_CONFIG or point it to a valid config.toml".into()),
            );
        } else if input.config_disabled {
            push_item(
                items,
                "runtime",
                "Env TARS_CONFIG",
                DoctorStatus::Warn,
                "set, but ignored because --no-config is active",
                None,
            );
        } else if !path.is_absolute() {
            push_item(
                items,
                "runtime",
                "Env TARS_CONFIG",
                DoctorStatus::Warn,
                "set to a relative path (resolved against current working directory)",
                Some("prefer an absolute path for TARS_CONFIG".into()),
            );
        } else {
            push_item(
                items,
                "runtime",
                "Env TARS_CONFIG",
                DoctorStatus::Ok,
                format!("absolute path ({})", path.display()),
                None,
            );
        }
    }
}

fn check_config(input: &DoctorInput<'_>, items: &mut Vec<DoctorItem>) -> Option<SessionDefinition> {
    if input.config_disabled {
        push_item(
            items,
            "config",
            "Config file",
            DoctorStatus::Ok,
            "disabled (--no-config)",
            None,
        );
    } else if let Some(path) = input.config_path.or_else(|| input.config.source()) {
        push_item(
            items,
            "config",
            "Config file",
            DoctorStatus::Ok,
            format!("loaded {}", path.display()),
            None,
        );
    } else {
        let detail = match default_config_path() {
            Some(path) => format!("not found at {}", path.display()),
            None => "no user config file found".to_owned(),
        };
        push_item(
            items,
            "config",
            "Config file",
            DoctorStatus::Warn,
            detail,
            Some("run `tars` in a terminal for guided setup or create config.toml".into()),
        );
    }

    for warning in input.config.warnings() {
        push_item(
            items,
            "config",
            "Config warning",
            DoctorStatus::Warn,
            warning.clone(),
            None,
        );
    }

    match input.config.preset(input.profile) {
        Ok(Some(def)) => {
            let name = input
                .profile
                .or_else(|| input.config.default_profile())
                .unwrap_or("default");
            push_item(
                items,
                "config",
                "Profile",
                DoctorStatus::Ok,
                format!(
                    "{name}: provider={}, model={}",
                    def.provider.as_str(),
                    def.model
                ),
                None,
            );
            if def.provider == Provider::Anthropic && def.max_tokens.is_none() {
                push_item(
                    items,
                    "config",
                    "Anthropic max_tokens",
                    DoctorStatus::Fail,
                    "Anthropic requests require a positive max_tokens setting",
                    Some(format!(
                        "set `max_tokens = 4096` under [profiles.{name}] in config.toml"
                    )),
                );
            }
            Some(def)
        }
        Ok(None) => {
            let (detail, hint) = if input.config_disabled || input.config.source().is_none() {
                (
                    "no default profile configured",
                    "pass --provider/--model or configure [profiles.default] in config.toml",
                )
            } else {
                (
                    "config loaded without a default profile",
                    "set `default_profile` in config.toml or pass --profile",
                )
            };
            push_item(
                items,
                "config",
                "Profile",
                DoctorStatus::Warn,
                detail,
                Some(hint.to_owned()),
            );
            None
        }
        Err(err) => {
            push_item(
                items,
                "config",
                "Profile",
                DoctorStatus::Fail,
                err.to_string(),
                Some("check [profiles] and default_profile in config.toml".into()),
            );
            None
        }
    }
}

fn check_credentials(
    input: &DoctorInput<'_>,
    resolved_profile: Option<&SessionDefinition>,
    items: &mut Vec<DoctorItem>,
) {
    if let Some(def) = resolved_profile {
        match &def.credential {
            CredentialRef::Environment(env_name) => {
                if env_var_is_set(env_name) {
                    push_item(
                        items,
                        "credentials",
                        format!("Env {env_name}"),
                        DoctorStatus::Ok,
                        "set",
                        None,
                    );
                } else {
                    push_item(
                        items,
                        "credentials",
                        format!("Env {env_name}"),
                        DoctorStatus::Fail,
                        "not set",
                        Some(format!("export {env_name}=...")),
                    );
                }
            }
            CredentialRef::Store(name) => {
                push_item(
                    items,
                    "credentials",
                    format!("Store {name}"),
                    DoctorStatus::Warn,
                    "store check pending",
                    Some(format!("tars auth set {name}")),
                );
            }
        }
    }

    let servers: Vec<&McpServerConfig> = match resolved_profile {
        Some(def) => def.mcp_servers.iter().collect(),
        None => input.config.mcp_servers().values().collect(),
    };

    for server in servers {
        for var in collect_required_mcp_env_vars(server) {
            if env_var_is_set(&var) {
                push_item(
                    items,
                    "credentials",
                    format!("MCP env {var} ({})", server.id),
                    DoctorStatus::Ok,
                    "set",
                    None,
                );
            } else {
                let status = if server.required {
                    DoctorStatus::Fail
                } else {
                    DoctorStatus::Warn
                };
                push_item(
                    items,
                    "credentials",
                    format!("MCP env {var} ({})", server.id),
                    status,
                    "not set",
                    Some(format!("export {var}=...")),
                );
            }
        }
    }
}

fn env_var_is_set(name: &str) -> bool {
    std::env::var_os(name).is_some_and(|val| !val.is_empty())
}

fn collect_required_mcp_env_vars(server: &McpServerConfig) -> BTreeSet<String> {
    let mut vars = BTreeSet::new();
    if let Some(url) = &server.url {
        extract_required_template_vars(url, &mut vars);
    }
    for arg in &server.args {
        extract_required_template_vars(arg, &mut vars);
    }
    for val in server.env.values() {
        extract_required_template_vars(val, &mut vars);
    }
    for val in server.headers.values() {
        extract_required_template_vars(val, &mut vars);
    }
    vars
}

fn extract_required_template_vars(template: &str, out: &mut BTreeSet<String>) {
    let bytes = template.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'$' && i + 1 < bytes.len() && bytes[i + 1] == b'{' {
            let start = i + 2;
            let Some(end) = template[start..].find('}') else {
                break;
            };
            let content = &template[start..start + end];
            if !content.contains(":-") && valid_environment_name(content) {
                out.insert(content.to_owned());
            }
            i = start + end + 1;
        } else {
            i += 1;
        }
    }
}

fn check_storage(input: &DoctorInput<'_>, items: &mut Vec<DoctorItem>) {
    let store_exists_dir = match fs::metadata(input.store_dir) {
        Ok(meta) if meta.is_dir() => {
            push_item(
                items,
                "storage",
                "Store directory",
                DoctorStatus::Ok,
                format!("exists ({})", input.store_dir.display()),
                None,
            );
            true
        }
        Ok(_) => {
            push_item(
                items,
                "storage",
                "Store directory",
                DoctorStatus::Fail,
                format!("path is not a directory ({})", input.store_dir.display()),
                Some("set `store` to a writable directory path".into()),
            );
            false
        }
        Err(err) if err.kind() == io::ErrorKind::NotFound => {
            push_item(
                items,
                "storage",
                "Store directory",
                DoctorStatus::Ok,
                format!(
                    "will be created on first session ({})",
                    input.store_dir.display()
                ),
                None,
            );
            false
        }
        Err(err) => {
            push_item(
                items,
                "storage",
                "Store directory",
                DoctorStatus::Fail,
                format!(
                    "cannot inspect {}: {}",
                    input.store_dir.display(),
                    err.kind()
                ),
                None,
            );
            false
        }
    };

    if store_exists_dir {
        let db_path = input.store_dir.join("tars.db");
        match fs::metadata(&db_path) {
            Ok(_) => match open_sqlite_for_doctor(&db_path) {
                Ok(Some((conn, ver))) => {
                    let sessions = conn.query_row("SELECT COUNT(*) FROM sessions", [], |row| {
                        row.get::<_, i64>(0)
                    });
                    match sessions {
                        Ok(count) => {
                            push_item(
                                items,
                                "storage",
                                "SQLite store",
                                DoctorStatus::Ok,
                                format!("readable (schema v{ver}, {count} session(s))"),
                                None,
                            );
                        }
                        Err(err) => {
                            push_item(
                                items,
                                "storage",
                                "SQLite store",
                                DoctorStatus::Fail,
                                format!("sessions table check failed: {err}"),
                                Some("verify tars.db integrity and schema".into()),
                            );
                        }
                    }
                }
                Ok(None) => {}
                Err(err) => {
                    push_item(
                        items,
                        "storage",
                        "SQLite store",
                        DoctorStatus::Fail,
                        format!("unreadable or invalid database: {err}"),
                        Some("verify tars.db file permissions and integrity".into()),
                    );
                }
            },
            Err(err) if err.kind() == io::ErrorKind::NotFound => {
                push_item(
                    items,
                    "storage",
                    "SQLite store",
                    DoctorStatus::Ok,
                    "not initialized yet (tars.db absent)",
                    None,
                );
            }
            Err(err) => {
                push_item(
                    items,
                    "storage",
                    "SQLite store",
                    DoctorStatus::Fail,
                    format!("cannot inspect {}: {}", db_path.display(), err.kind()),
                    Some("verify store directory and tars.db permissions".into()),
                );
            }
        }

        let legacy_dir = input.store_dir.join("sessions");
        if let Ok(entries) = fs::read_dir(&legacy_dir) {
            let legacy_count = entries
                .filter_map(Result::ok)
                .filter(|entry| {
                    let path = entry.path();
                    path.extension().and_then(OsStr::to_str) == Some("json")
                        && entry.file_type().is_ok_and(|ft| ft.is_file())
                })
                .count();
            if legacy_count > 0 {
                push_item(
                    items,
                    "storage",
                    "Legacy sessions",
                    DoctorStatus::Warn,
                    format!(
                        "detected {legacy_count} legacy JSON file(s) in {}",
                        legacy_dir.display()
                    ),
                    Some("run `tars session import-legacy` to migrate v2.4 sessions".into()),
                );
            }
        }
    }

    match fs::metadata(input.recovery_dir) {
        Ok(meta) if meta.is_dir() => {
            push_item(
                items,
                "storage",
                "Recovery directory",
                DoctorStatus::Ok,
                format!("exists ({})", input.recovery_dir.display()),
                None,
            );
        }
        Ok(_) => {
            push_item(
                items,
                "storage",
                "Recovery directory",
                DoctorStatus::Warn,
                format!("path is not a directory ({})", input.recovery_dir.display()),
                Some("set `recovery_dir` to a valid directory".into()),
            );
        }
        Err(err) if err.kind() == io::ErrorKind::NotFound => {
            push_item(
                items,
                "storage",
                "Recovery directory",
                DoctorStatus::Warn,
                format!("does not exist ({})", input.recovery_dir.display()),
                Some("create the directory or update `recovery_dir` in config.toml".into()),
            );
        }
        Err(err) => {
            push_item(
                items,
                "storage",
                "Recovery directory",
                DoctorStatus::Warn,
                format!(
                    "cannot inspect {}: {}",
                    input.recovery_dir.display(),
                    err.kind()
                ),
                None,
            );
        }
    }
}

fn effective_workspace_root<'a>(
    input: &'a DoctorInput<'a>,
    resolved_profile: Option<&'a SessionDefinition>,
) -> Option<&'a Path> {
    input
        .workspace_root
        .or_else(|| resolved_profile.and_then(|def| def.workspace_root.as_deref()))
}

fn check_workspace_and_tools(
    input: &DoctorInput<'_>,
    resolved_profile: Option<&SessionDefinition>,
    sandbox: &SandboxStatus,
    items: &mut Vec<DoctorItem>,
) {
    if let Some(root) = effective_workspace_root(input, resolved_profile) {
        match fs::metadata(root) {
            Ok(meta) if meta.is_dir() => match fs::read_dir(root) {
                Ok(_) => push_item(
                    items,
                    "workspace",
                    "Workspace root",
                    DoctorStatus::Ok,
                    format!("readable ({})", root.display()),
                    None,
                ),
                Err(err) => push_item(
                    items,
                    "workspace",
                    "Workspace root",
                    DoctorStatus::Fail,
                    format!("unreadable ({}: {})", root.display(), err.kind()),
                    Some("check directory permissions".into()),
                ),
            },
            Ok(_) => push_item(
                items,
                "workspace",
                "Workspace root",
                DoctorStatus::Fail,
                format!("not a directory ({})", root.display()),
                None,
            ),
            Err(err) => push_item(
                items,
                "workspace",
                "Workspace root",
                DoctorStatus::Fail,
                format!("unavailable ({}: {})", root.display(), err.kind()),
                None,
            ),
        }
    } else {
        push_item(
            items,
            "workspace",
            "Workspace root",
            DoctorStatus::Ok,
            "none selected",
            None,
        );
    }

    let tools = input
        .active_tools
        .or_else(|| resolved_profile.map(|def| def.tools.as_slice()));
    if let Some(tools) = tools
        && !tools.is_empty()
    {
        let names = tools
            .iter()
            .map(|tool| tool.name())
            .collect::<Vec<_>>()
            .join(", ");
        push_item(
            items,
            "workspace",
            "Workspace tools",
            DoctorStatus::Ok,
            names,
            None,
        );

        let needs_sandbox = tools
            .iter()
            .any(|tool| matches!(tool, WorkspaceTool::RunCommand | WorkspaceTool::SearchText));
        if needs_sandbox && !sandbox.is_available() {
            push_item(
                items,
                "workspace",
                "Sandboxed tools",
                DoctorStatus::Warn,
                "run_command and search_text fail closed without a sandbox; find_files works without sandbox",
                Some("remove run_command/search_text from tools or run on a platform with sandbox support".into()),
            );
        }
    }
}

fn sandbox_unavailable_hint(sandbox: &SandboxStatus) -> String {
    let SandboxStatus::Unavailable(err) = sandbox else {
        return String::new();
    };
    let reason = err.reason.as_str();
    let is_bwrap = err.backend == BackendId::BWRAP;
    let missing_bwrap = reason.contains("bwrap executable not found")
        || (reason.contains("bubblewrap") && reason.contains("not found"));
    if missing_bwrap {
        return "install bubblewrap (e.g. `sudo apt install bubblewrap`); command execution and ripgrep search fail closed until bwrap is available".to_owned();
    }
    let mentions_userns_or_apparmor = [
        "AppArmor",
        "userns",
        "uid map",
        "RTM_NEWADDR",
        "Operation not permitted",
        "Permission denied",
    ]
    .iter()
    .any(|needle| reason.contains(needle));
    if (is_bwrap || cfg!(target_os = "linux")) && mentions_userns_or_apparmor {
        return "unprivileged user namespaces appear restricted for bwrap; on Ubuntu 24.04+ load an AppArmor userns profile for /usr/bin/bwrap or verify kernel.apparmor_restrict_unprivileged_userns".to_owned();
    }
    if is_bwrap || cfg!(target_os = "linux") {
        return "ensure `bubblewrap` (`bwrap`) and unprivileged user namespaces are available; command execution and ripgrep search fail closed when sandbox is unavailable".to_owned();
    }
    "command execution and ripgrep search fail closed when sandbox is unavailable".to_owned()
}

fn check_sandbox(sandbox: &SandboxStatus, items: &mut Vec<DoctorItem>) {
    if sandbox.is_available() {
        push_item(
            items,
            "sandbox",
            "Sandbox",
            DoctorStatus::Ok,
            sandbox.to_string(),
            None,
        );
    } else {
        push_item(
            items,
            "sandbox",
            "Sandbox",
            DoctorStatus::Warn,
            sandbox.to_string(),
            Some(sandbox_unavailable_hint(sandbox)),
        );
    }
}

fn check_external_tools(input: &DoctorInput<'_>, items: &mut Vec<DoctorItem>) {
    let owned;
    let tools = match input.external_tools {
        Some(existing) => existing,
        None => {
            owned = ExternalTools::resolve(input.config);
            &owned
        }
    };
    let rg = tools.rg();
    if rg.is_ready() {
        let mut detail = format!(
            "ready (version {}, source={})",
            rg.version.as_deref().unwrap_or("unknown"),
            rg.source
        );
        if let Some(path) = &rg.path {
            detail.push_str(&format!(", {}", path.display()));
        }
        push_item(
            items,
            "external_tools",
            "ripgrep (rg)",
            DoctorStatus::Ok,
            detail,
            None,
        );
    } else {
        let hint = match rg.source {
            ToolSource::Auto => {
                Some("run `tars tools install rg` or install ripgrep >= 14 on PATH".to_owned())
            }
            ToolSource::Managed => Some("run `tars tools install rg`".to_owned()),
            ToolSource::System => Some(
                "install ripgrep >= 14 on PATH or switch [external_tools] rg = \"managed\""
                    .to_owned(),
            ),
            ToolSource::Path(_) => {
                Some("verify [external_tools] rg path in config.toml".to_owned())
            }
        };
        push_item(
            items,
            "external_tools",
            "ripgrep (rg)",
            DoctorStatus::Warn,
            rg.unavailable_reason().unwrap_or("unavailable"),
            hint,
        );
    }
}

fn check_permissions_mcp_prelude(
    input: &DoctorInput<'_>,
    resolved_profile: Option<&SessionDefinition>,
    items: &mut Vec<DoctorItem>,
) {
    if let Some(rules_path) = input.user_rules_path {
        match load_rules_file(rules_path) {
            Ok(Some(loaded)) => {
                push_item(
                    items,
                    "permissions",
                    "User permissions",
                    DoctorStatus::Ok,
                    format!("{} ({} rule(s))", rules_path.display(), loaded.rules.len()),
                    None,
                );
            }
            Ok(None) => {}
            Err(err) => {
                push_item(
                    items,
                    "permissions",
                    "User permissions",
                    DoctorStatus::Fail,
                    err.to_string(),
                    Some("fix invalid syntax or forbidden rules in permissions.toml".into()),
                );
            }
        }
    }

    if let Some(mcp_path) = input.config.mcp_source() {
        push_item(
            items,
            "mcp",
            "MCP config",
            DoctorStatus::Ok,
            format!(
                "loaded {} ({} server(s))",
                mcp_path.display(),
                input.config.mcp_servers().len()
            ),
            None,
        );
    }
    let path_env = std::env::var_os("PATH");
    for (id, server) in input.config.mcp_servers() {
        if let Some(cmd) = &server.command {
            if command_exists_without_spawning(cmd, path_env.as_deref()) {
                push_item(
                    items,
                    "mcp",
                    format!("MCP stdio '{id}'"),
                    DoctorStatus::Ok,
                    format!("command available ({cmd})"),
                    None,
                );
            } else {
                push_item(
                    items,
                    "mcp",
                    format!("MCP stdio '{id}'"),
                    DoctorStatus::Warn,
                    format!("command not found: {cmd}"),
                    Some(format!("install `{cmd}` or update `command` in mcp.json")),
                );
            }
        }
    }

    let ws_root = effective_workspace_root(input, resolved_profile);

    if let Some(cfg_dir) = input.config.config_dir() {
        check_agents_md_file("User AGENTS.md", &cfg_dir.join("AGENTS.md"), false, items);
    }
    if let Some(root) = ws_root {
        check_agents_md_file("Workspace AGENTS.md", &root.join("AGENTS.md"), true, items);
    }

    let (skill_dirs, include_ws_skills) = if let Some(def) = resolved_profile {
        (
            def.prelude.skill_dirs.clone(),
            def.prelude.include_workspace_skills,
        )
    } else if let Some(dirs) = input.config.prelude_skill_dirs() {
        (
            dirs.to_vec(),
            input
                .config
                .prelude_include_workspace_skills()
                .unwrap_or(true),
        )
    } else {
        (
            default_existing_subdir(input.config.config_dir(), "skills"),
            input
                .config
                .prelude_include_workspace_skills()
                .unwrap_or(true),
        )
    };

    let skill_discovery = SkillDiscovery::discover(&skill_dirs, ws_root, include_ws_skills);
    push_item(
        items,
        "prelude",
        "Skills",
        DoctorStatus::Ok,
        format!("{} skill(s) discovered", skill_discovery.registry.len()),
        None,
    );
    if !skill_discovery.diagnostics.is_empty() {
        push_item(
            items,
            "prelude",
            "Skill diagnostics",
            DoctorStatus::Warn,
            format!("{} diagnostic(s)", skill_discovery.diagnostics.len()),
            skill_discovery
                .diagnostics
                .first()
                .map(|diag| diag.message.clone()),
        );
    }

    let (agent_dirs, include_ws_agents) = if let Some(def) = resolved_profile {
        (
            def.prelude.agent_dirs.clone(),
            def.prelude.include_workspace_agents,
        )
    } else if let Some(dirs) = input.config.prelude_agent_dirs() {
        (
            dirs.to_vec(),
            input
                .config
                .prelude_include_workspace_agents()
                .unwrap_or(true),
        )
    } else {
        (
            Vec::new(),
            input
                .config
                .prelude_include_workspace_agents()
                .unwrap_or(true),
        )
    };

    let non_ws_agents =
        AgentDiscovery::discover_non_workspace(input.config.config_dir(), &agent_dirs);
    let ws_snap = if include_ws_agents {
        ws_root.map(scan_workspace_agents)
    } else {
        None
    };
    let ws_count = ws_snap.as_ref().map_or(0, |snap| snap.presets.len());
    push_item(
        items,
        "prelude",
        "Agents",
        DoctorStatus::Ok,
        format!(
            "{} preset(s) (non-workspace), {ws_count} workspace preset(s)",
            non_ws_agents.registry.len()
        ),
        None,
    );
    let total_agent_diags =
        non_ws_agents.diagnostics.len() + ws_snap.as_ref().map_or(0, |snap| snap.diagnostics.len());
    if total_agent_diags > 0 {
        let hint = non_ws_agents
            .diagnostics
            .first()
            .or_else(|| ws_snap.as_ref().and_then(|snap| snap.diagnostics.first()))
            .map(|diag| diag.message.clone());
        push_item(
            items,
            "prelude",
            "Agent diagnostics",
            DoctorStatus::Warn,
            format!("{total_agent_diags} diagnostic(s)"),
            hint,
        );
    }
}

fn check_agents_md_file(label: &str, path: &Path, suggest_init: bool, items: &mut Vec<DoctorItem>) {
    match fs::metadata(path) {
        Ok(meta) if meta.is_file() => push_item(
            items,
            "prelude",
            label,
            DoctorStatus::Ok,
            format!("present ({})", path.display()),
            None,
        ),
        Ok(_) => push_item(
            items,
            "prelude",
            label,
            DoctorStatus::Warn,
            format!("not a regular file ({})", path.display()),
            None,
        ),
        Err(err) if err.kind() == io::ErrorKind::NotFound => {
            let detail = if suggest_init {
                format!("not present ({}; run /init to create)", path.display())
            } else {
                format!("not present ({})", path.display())
            };
            push_item(items, "prelude", label, DoctorStatus::Ok, detail, None);
        }
        Err(err) => push_item(
            items,
            "prelude",
            label,
            DoctorStatus::Warn,
            format!("unreadable ({}: {})", path.display(), err.kind()),
            None,
        ),
    }
}

fn default_existing_subdir(config_dir: Option<&Path>, subdir: &str) -> Vec<PathBuf> {
    let Some(dir) = config_dir else {
        return Vec::new();
    };
    let candidate = dir.join(subdir);
    match fs::canonicalize(&candidate) {
        Ok(canon) if fs::metadata(&canon).is_ok_and(|meta| meta.is_dir()) => vec![canon],
        _ => Vec::new(),
    }
}

pub(crate) fn command_exists_without_spawning(command: &str, path_env: Option<&OsStr>) -> bool {
    let cmd_path = Path::new(command);
    if cmd_path.is_absolute() || command.contains('/') || command.contains('\\') {
        return is_executable_file(cmd_path);
    }
    let Some(path_var) = path_env else {
        return false;
    };
    std::env::split_paths(path_var)
        .filter(|dir| !dir.as_os_str().is_empty())
        .any(|dir| {
            let candidate = dir.join(command);
            if is_executable_file(&candidate) {
                return true;
            }
            #[cfg(windows)]
            if cmd_path.extension().is_none()
                && is_executable_file(&dir.join(format!("{command}.exe")))
            {
                return true;
            }
            false
        })
}

fn is_executable_file(path: &Path) -> bool {
    let Ok(meta) = fs::metadata(path) else {
        return false;
    };
    if !meta.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        meta.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

fn db_sidecar_path(db_path: &Path, suffix: &str) -> PathBuf {
    let mut os = db_path.as_os_str().to_os_string();
    os.push(suffix);
    PathBuf::from(os)
}

fn percent_encode_sqlite_uri_path(raw: &str) -> String {
    let mut encoded = String::with_capacity(raw.len());
    for ch in raw.chars() {
        match ch {
            '%' => encoded.push_str("%25"),
            '?' => encoded.push_str("%3F"),
            '#' => encoded.push_str("%23"),
            '\\' if cfg!(windows) => encoded.push('/'),
            _ => encoded.push(ch),
        }
    }
    encoded
}

fn sqlite_immutable_uri(db_path: &Path) -> Option<String> {
    let abs = std::path::absolute(db_path).ok()?;
    let raw = abs.to_str()?;
    #[cfg(windows)]
    let raw = raw.strip_prefix(r"\\?\").unwrap_or(raw);
    let encoded = percent_encode_sqlite_uri_path(raw);
    let prefix = if encoded.starts_with('/') {
        "file://"
    } else {
        "file:///"
    };
    Some(format!("{prefix}{encoded}?immutable=1"))
}

fn open_sqlite_for_doctor(
    db_path: &Path,
) -> Result<Option<(rusqlite::Connection, u32)>, crate::store::StoreError> {
    let wal_path = db_sidecar_path(db_path, "-wal");
    let shm_path = db_sidecar_path(db_path, "-shm");
    let wal_absent_or_empty = match fs::metadata(&wal_path) {
        Ok(meta) => meta.is_file() && meta.len() == 0,
        Err(err) if err.kind() == io::ErrorKind::NotFound => true,
        Err(_) => false,
    };
    let shm_absent = matches!(
        fs::metadata(&shm_path),
        Err(err) if err.kind() == io::ErrorKind::NotFound
    );

    if wal_absent_or_empty
        && shm_absent
        && let Some(uri) = sqlite_immutable_uri(db_path)
    {
        let conn = rusqlite::Connection::open_with_flags(
            uri,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY
                | rusqlite::OpenFlags::SQLITE_OPEN_URI
                | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        conn.execute_batch("PRAGMA query_only = ON;")?;
        let version = schema::read_user_version(&conn)?;
        if version > schema::SCHEMA_VERSION {
            return Err(crate::store::StoreError::UnsupportedSchemaVersion {
                found: version,
                supported: schema::SCHEMA_VERSION,
            });
        }
        if version == 0 {
            return Err(crate::store::StoreError::InvalidDatabase(
                "SQLite store schema is uninitialized (user_version = 0)".into(),
            ));
        }
        return Ok(Some((conn, version)));
    }

    let Some(conn) = schema::open_read_only(db_path)? else {
        return Ok(None);
    };
    let version = schema::read_user_version(&conn)?;
    Ok(Some((conn, version)))
}
