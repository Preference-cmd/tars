//! Read-only, zero-network diagnostic subcommand (`tars doctor`).

use super::command::output;
use super::configuration::{self, CliError};
use std::collections::HashMap;
use std::ffi::OsString;
use std::path::PathBuf;
use std::process::ExitCode;
use tars_core::config::{Config, default_store_dir};
use tars_core::doctor::{DoctorInput, DoctorStatus, run_diagnostics};
use tars_core::permission::escape_untrusted_text;

pub(super) const HELP: &str = "Run read-only environment and configuration diagnostics

Usage: tars doctor [--config <file> | --no-config] [--profile <name>] [--store <dir>] [--recovery-dir <dir>] [--workspace <dir>] [--json]

Options:
  --config FILE       Shared TOML configuration file
  --no-config         Ignore global configuration; conflicts with --config
  --profile NAME      Inspect named model preset (otherwise default_profile)
  --store DIR         Session store directory to inspect
  --recovery-dir DIR  Recovery export directory to inspect
  --workspace DIR     Workspace root directory to inspect
  --json              Output a single structured JSON report
  -h, --help          Show this help
";

pub(super) struct DoctorOptions {
    config: Config,
    config_path: Option<PathBuf>,
    no_config: bool,
    profile: Option<String>,
    store_dir: PathBuf,
    recovery_dir: PathBuf,
    workspace: Option<PathBuf>,
    user_rules_path: Option<PathBuf>,
    credentials_path: Option<PathBuf>,
    json: bool,
}

impl DoctorOptions {
    pub(super) fn parse(args: &[OsString]) -> Result<Self, CliError> {
        let mut values: HashMap<&'static str, &OsString> = HashMap::new();
        let mut no_config = false;
        let mut json = false;
        let mut iter = args.iter();

        while let Some(arg) = iter.next() {
            let flag = arg.to_str().ok_or("option must be UTF-8")?;
            match flag {
                "--json" => {
                    if json {
                        return Err(CliError::argument("duplicate option: --json"));
                    }
                    json = true;
                }
                "--no-config" => {
                    if no_config {
                        return Err(CliError::argument("duplicate option: --no-config"));
                    }
                    no_config = true;
                }
                "--config" | "--profile" | "--store" | "--recovery-dir" | "--workspace" => {
                    let static_flag = static_doctor_flag(flag);
                    let value = iter.next().ok_or_else(|| {
                        CliError::argument(format!("option {static_flag} requires a value"))
                    })?;
                    if values.contains_key(static_flag) {
                        return Err(CliError::argument(format!(
                            "duplicate option: {static_flag}"
                        )));
                    }
                    if value.is_empty() {
                        return Err(CliError::argument(format!(
                            "option {static_flag} must not be empty"
                        )));
                    }
                    values.insert(static_flag, value);
                }
                other if !other.starts_with('-') || other == "-" => {
                    return Err(CliError::argument(format!(
                        "unexpected positional argument: {}",
                        escape_untrusted_text(other)
                    )));
                }
                other => {
                    return Err(CliError::argument(format!(
                        "unknown option: {}",
                        escape_untrusted_text(other)
                    )));
                }
            }
        }

        let profile = configuration::string(&values, "--profile")?;
        let config_path = values.get("--config").map(PathBuf::from);
        let config = configuration::load(&values, no_config)?;
        let store_dir = values
            .get("--store")
            .map(PathBuf::from)
            .or_else(|| config.store_dir().map(PathBuf::from))
            .or_else(|| (!no_config).then(default_store_dir).flatten())
            .unwrap_or_else(|| PathBuf::from(".tars/data"));
        let recovery_dir = values
            .get("--recovery-dir")
            .map(PathBuf::from)
            .or_else(|| config.recovery_dir().map(PathBuf::from))
            .unwrap_or_else(|| PathBuf::from("."));
        let workspace = values.get("--workspace").map(PathBuf::from);
        let user_rules_path = config.config_dir().map(|dir| dir.join("permissions.toml"));
        let credentials_path = configuration::credentials_path_for(&config, no_config);

        Ok(Self {
            config,
            config_path,
            no_config,
            profile,
            store_dir,
            recovery_dir,
            workspace,
            user_rules_path,
            credentials_path,
            json,
        })
    }
}

fn static_doctor_flag(flag: &str) -> &'static str {
    match flag {
        "--config" => "--config",
        "--profile" => "--profile",
        "--store" => "--store",
        "--recovery-dir" => "--recovery-dir",
        "--workspace" => "--workspace",
        _ => unreachable!("unexpected doctor flag {flag}"),
    }
}

pub(super) fn run(options: DoctorOptions) -> ExitCode {
    let input = DoctorInput {
        version: env!("CARGO_PKG_VERSION"),
        config: &options.config,
        config_path: options.config_path.as_deref(),
        config_disabled: options.no_config,
        profile: options.profile.as_deref(),
        store_dir: &options.store_dir,
        recovery_dir: &options.recovery_dir,
        workspace_root: options.workspace.as_deref(),
        active_tools: None,
        sandbox: None,
        external_tools: None,
        user_rules_path: options.user_rules_path.as_deref(),
        credentials_path: options.credentials_path.as_deref(),
    };
    let report = run_diagnostics(&input);
    let has_fail = report.fail_count() > 0;

    let rendered = if options.json {
        let items: Vec<_> = report
            .items
            .iter()
            .map(|item| {
                serde_json::json!({
                    "category": item.category,
                    "label": item.label,
                    "status": match item.status {
                        DoctorStatus::Ok => "ok",
                        DoctorStatus::Warn => "warn",
                        DoctorStatus::Fail => "fail",
                    },
                    "detail": item.detail,
                    "hint": item.hint,
                })
            })
            .collect();
        let payload = serde_json::json!({
            "ok": report.ok_count(),
            "warn": report.warn_count(),
            "fail": report.fail_count(),
            "items": items,
        });
        format!("{payload}\n")
    } else {
        let mut lines = report.format_lines().join("\n");
        lines.push('\n');
        lines
    };

    if output(&rendered) != ExitCode::SUCCESS || has_fail {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}

#[cfg(test)]
mod tests {
    use super::DoctorOptions;
    use std::ffi::OsString;

    fn os(args: &[&str]) -> Vec<OsString> {
        args.iter().map(OsString::from).collect()
    }

    #[test]
    fn doctor_parser_rejects_invalid_and_conflicting_flags() {
        for (args, expected) in [
            (vec!["--bogus"], "unknown option: --bogus"),
            (vec!["extra"], "unexpected positional argument: extra"),
            (vec!["--json", "--json"], "duplicate option: --json"),
            (
                vec!["--no-config", "--no-config"],
                "duplicate option: --no-config",
            ),
            (vec!["--profile"], "option --profile requires a value"),
            (vec!["--store", ""], "option --store must not be empty"),
            (
                vec!["--profile", "a", "--profile", "b"],
                "duplicate option: --profile",
            ),
            (
                vec!["--config", "a.toml", "--no-config"],
                "--config and --no-config are mutually exclusive",
            ),
        ] {
            let err = DoctorOptions::parse(&os(&args))
                .err()
                .expect("doctor parse must fail");
            assert!(err.is_argument(), "args={args:?}");
            assert!(
                err.message().contains(expected),
                "args={args:?}: expected '{expected}' in '{}'",
                err.message()
            );
        }
    }
}
