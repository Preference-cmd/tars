//! Workspace folder trust listing and revocation (`tars trust`).

use super::command::{failure, output};
use super::configuration::{self, CliError};
use std::collections::HashMap;
use std::ffi::OsString;
use std::path::PathBuf;
use std::process::ExitCode;
use tars_core::application::SessionHost;
use tars_core::config::default_store_dir;
use tars_core::permission::escape_untrusted_text;

pub(super) const HELP: &str = "Manage trusted workspace folders

Usage: tars trust list [--store <dir>] [--config <file> | --no-config] [--json]
       tars trust revoke [DIR] [--store <dir>] [--config <file> | --no-config]

Commands:
  list           List trusted workspace folders from the local store
  revoke [DIR]   Revoke folder trust for DIR (defaults to the current directory)

Options:
  --store        Local store directory (defaults to configured store or ~/.tars/data)
  --config       Shared TOML configuration file
  --no-config    Ignore global configuration; conflicts with --config (requires --store)
  --json         Output structured JSON array for list
  -h, --help     Show this help
";

const LIST_HELP: &str = "List trusted workspace folders

Usage: tars trust list [--store <dir>] [--config <file> | --no-config] [--json]

Options:
  --store        Local store directory (defaults to configured store or ~/.tars/data)
  --config       Shared TOML configuration file
  --no-config    Ignore global configuration; conflicts with --config (requires --store)
  --json         Output a JSON array of objects with workspace_root and trusted_at_ms
  -h, --help     Show this help
";

const REVOKE_HELP: &str = "Revoke trust for a workspace folder

Usage: tars trust revoke [DIR] [--store <dir>] [--config <file> | --no-config]

Arguments:
  DIR            Target workspace folder (defaults to the current working directory)

Options:
  --store        Local store directory (defaults to configured store or ~/.tars/data)
  --config       Shared TOML configuration file
  --no-config    Ignore global configuration; conflicts with --config (requires --store)
  -h, --help     Show this help
";

pub(super) fn subcommand_help(command: &str) -> &'static str {
    match command {
        "list" => LIST_HELP,
        "revoke" => REVOKE_HELP,
        _ => HELP,
    }
}

pub(super) enum TrustCommand {
    List {
        store: PathBuf,
        json: bool,
    },
    Revoke {
        store: PathBuf,
        workspace: Option<PathBuf>,
    },
}

impl TrustCommand {
    pub(super) fn parse(args: &[OsString]) -> Result<Self, CliError> {
        let (subcommand, rest) = args.split_first().ok_or("trust command is required")?;
        let subcommand = subcommand.to_str().ok_or("trust command must be UTF-8")?;
        match subcommand {
            "list" => {
                let parsed = parse_common(rest, false, true)?;
                Ok(Self::List {
                    store: parsed.store,
                    json: parsed.json,
                })
            }
            "revoke" => {
                let parsed = parse_common(rest, true, false)?;
                Ok(Self::Revoke {
                    store: parsed.store,
                    workspace: parsed.positional,
                })
            }
            other => Err(CliError::argument(format!(
                "unknown trust command: {}; use tars trust --help",
                escape_untrusted_text(other)
            ))),
        }
    }
}

struct ParsedCommon {
    store: PathBuf,
    json: bool,
    positional: Option<PathBuf>,
}

fn parse_common(
    args: &[OsString],
    allow_positional: bool,
    allow_json: bool,
) -> Result<ParsedCommon, CliError> {
    let mut values: HashMap<&'static str, &OsString> = HashMap::new();
    let mut no_config = false;
    let mut json = false;
    let mut positional: Option<PathBuf> = None;
    let mut iter = args.iter();

    while let Some(arg) = iter.next() {
        let token = arg.to_str().ok_or("argument must be UTF-8")?;
        match token {
            "--json" => {
                if !allow_json {
                    return Err(CliError::argument(
                        "option --json is not applicable to trust revoke",
                    ));
                }
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
            "--store" | "--config" => {
                let static_flag = if token == "--store" {
                    "--store"
                } else {
                    "--config"
                };
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
            other if !other.starts_with('-') => {
                if !allow_positional {
                    return Err(CliError::argument(format!(
                        "unexpected positional argument: {}",
                        escape_untrusted_text(other)
                    )));
                }
                if other.is_empty() {
                    return Err(CliError::argument("workspace directory must not be empty"));
                }
                if positional.is_some() {
                    return Err(CliError::argument(format!(
                        "unexpected positional argument: {}",
                        escape_untrusted_text(other)
                    )));
                }
                positional = Some(PathBuf::from(other));
            }
            other => {
                return Err(CliError::argument(format!(
                    "unknown option: {}",
                    escape_untrusted_text(other)
                )));
            }
        }
    }

    let store = resolve_store(&values, no_config)?;
    Ok(ParsedCommon {
        store,
        json,
        positional,
    })
}

fn resolve_store(
    values: &HashMap<&'static str, &OsString>,
    no_config: bool,
) -> Result<PathBuf, CliError> {
    let config = configuration::load(values, no_config)?;
    if let Some(store) = values.get("--store") {
        return Ok(PathBuf::from(store));
    }
    if no_config {
        return Err(CliError::argument(
            "--store is required when --no-config is set",
        ));
    }
    config
        .store_dir()
        .map(PathBuf::from)
        .or_else(default_store_dir)
        .ok_or_else(|| CliError::argument("--store or a configured store directory is required"))
}

pub(super) fn run(command: TrustCommand) -> ExitCode {
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => return failure(error),
    };
    runtime.block_on(async move {
        match command {
            TrustCommand::List { store, json } => list(store, json).await,
            TrustCommand::Revoke { store, workspace } => revoke(store, workspace).await,
        }
    })
}

async fn list(store: PathBuf, json: bool) -> ExitCode {
    let host = SessionHost::new(Some(store));
    let folders = match host.list_trusted_workspace_folders().await {
        Ok(folders) => folders,
        Err(error) => return failure(error),
    };
    if json {
        let items: Vec<serde_json::Value> = folders
            .iter()
            .map(|folder| {
                serde_json::json!({
                    "workspace_root": folder.workspace_root,
                    "trusted_at_ms": folder.trusted_at_ms,
                })
            })
            .collect();
        return output(&format!("{}\n", serde_json::Value::Array(items)));
    }
    if folders.is_empty() {
        return output("No trusted workspace folders.\n");
    }
    let mut text = String::new();
    for folder in &folders {
        let escaped = escape_untrusted_text(&folder.workspace_root.display().to_string());
        let timestamp = if folder.trusted_at_ms >= 0 {
            tars_core::format_timestamp_utc(folder.trusted_at_ms as u64)
        } else {
            folder.trusted_at_ms.to_string()
        };
        text.push_str(&format!("{escaped}  (trusted {timestamp})\n"));
    }
    output(&text)
}

async fn revoke(store: PathBuf, workspace: Option<PathBuf>) -> ExitCode {
    let target = match workspace {
        Some(dir) => dir,
        None => match std::env::current_dir() {
            Ok(cwd) => cwd,
            Err(error) => return failure(error),
        },
    };
    let host = SessionHost::new(Some(store));
    match host.revoke_workspace_folder_trust(&target).await {
        Ok(true) => output(&format!(
            "Revoked folder trust for {}\n",
            escape_untrusted_text(&target.display().to_string())
        )),
        Ok(false) => output(&format!(
            "Folder is not trusted: {}\n",
            escape_untrusted_text(&target.display().to_string())
        )),
        Err(error) => failure(error),
    }
}

#[cfg(test)]
mod tests {
    use super::TrustCommand;
    use std::ffi::OsString;
    use std::path::PathBuf;

    fn os(args: &[&str]) -> Vec<OsString> {
        args.iter().map(OsString::from).collect()
    }

    #[test]
    fn trust_parser_enforces_shared_store_resolution_and_argument_errors() {
        // Both `list` and `revoke` must reject `--no-config` without `--store` as exit code 2.
        for args in [
            vec!["list", "--no-config"],
            vec!["revoke", "--no-config"],
            vec!["revoke", "/tmp/some-ws", "--no-config"],
        ] {
            let err = TrustCommand::parse(&os(&args))
                .err()
                .expect("expected --no-config without --store to fail");
            assert!(err.is_argument(), "args={args:?}: {err:?}");
            assert!(
                err.message().contains("--store"),
                "args={args:?}: {}",
                err.message()
            );
        }

        // `list --no-config --store DIR` and `revoke [DIR] --no-config --store DIR` succeed.
        match TrustCommand::parse(&os(&[
            "list",
            "--no-config",
            "--store",
            "/tmp/st",
            "--json",
        ])) {
            Ok(TrustCommand::List { store, json }) => {
                assert_eq!(store, PathBuf::from("/tmp/st"));
                assert!(json);
            }
            _ => panic!("expected List"),
        }
        match TrustCommand::parse(&os(&["revoke", "--no-config", "--store", "/tmp/st"])) {
            Ok(TrustCommand::Revoke { store, workspace }) => {
                assert_eq!(store, PathBuf::from("/tmp/st"));
                assert!(workspace.is_none());
            }
            _ => panic!("expected Revoke"),
        }
        match TrustCommand::parse(&os(&[
            "revoke",
            "/tmp/ws",
            "--no-config",
            "--store",
            "/tmp/st",
        ])) {
            Ok(TrustCommand::Revoke { store, workspace }) => {
                assert_eq!(store, PathBuf::from("/tmp/st"));
                assert_eq!(workspace, Some(PathBuf::from("/tmp/ws")));
            }
            _ => panic!("expected Revoke with workspace"),
        }

        for args in [
            vec![],
            vec!["bogus"],
            vec!["list", "extra"],
            vec!["list", "--bogus"],
            vec!["list", "--store"],
            vec!["list", "--store", ""],
            vec!["list", "--config", "a.toml", "--no-config"],
            vec!["revoke", "one", "two"],
            vec!["revoke", "--json"],
            vec!["revoke", ""],
        ] {
            let err = TrustCommand::parse(&os(&args))
                .err()
                .expect("expected parse failure");
            assert!(err.is_argument(), "args={args:?}: {err:?}");
        }
    }
}
