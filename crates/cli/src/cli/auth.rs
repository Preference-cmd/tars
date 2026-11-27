//! Isolated `0600` model credential store management (`tars auth`).

use super::command::{failure, output};
use super::configuration::CliError;
use std::ffi::OsString;
use std::io::{self, IsTerminal, Read, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use tars_core::config::{CredentialsStore, credentials_path_for_config};
use tars_core::definition::valid_credential_name;
use tars_core::permission::escape_untrusted_text;

const MAX_STDIN_SECRET_BYTES: usize = 16 * 1024;

pub(super) const HELP: &str = "Manage isolated 0600 model credentials (credentials.toml)

Usage: tars auth set <name> [--stdin] [--config <file>]
       tars auth list [--config <file>] [--json]
       tars auth remove <name> [--config <file>]
       tars auth path [--config <file>]

Commands:
  set <name>     Store or update a named API key (prompts via TTY or reads --stdin)
  list           List stored credential entry names (never prints secrets)
  remove <name>  Remove a named credential entry
  path           Print the resolved credentials.toml path

Options:
  --stdin        Read secret from standard input (only for set)
  --config FILE  Shared TOML configuration file (uses sibling credentials.toml)
  --json         Output structured JSON (only for list)
  -h, --help     Show this help

Secrets are never accepted as command-line arguments. Use interactive TTY input
or pipe via --stdin. --no-config is rejected on tars auth commands.
";

const SET_HELP: &str = "Store or update a named API key in 0600 credentials.toml

Usage: tars auth set <name> [--stdin] [--config <file>]

Arguments:
  <name>         Credential name (1..=64 ASCII alphanumeric, '_' or '-', starting with alphanumeric)

Options:
  --stdin        Read secret from standard input (max 16 KiB, trailing CR/LF trimmed)
  --config FILE  Shared TOML configuration file (uses sibling credentials.toml)
  -h, --help     Show this help
";

const LIST_HELP: &str = "List stored credential entry names without echoing any secrets

Usage: tars auth list [--config <file>] [--json]

Options:
  --config FILE  Shared TOML configuration file (uses sibling credentials.toml)
  --json         Output JSON {\"path\": \"...\", \"credentials\": [...]}
  -h, --help     Show this help
";

const REMOVE_HELP: &str = "Remove a named credential entry from credentials.toml

Usage: tars auth remove <name> [--config <file>]

Arguments:
  <name>         Credential name to remove

Options:
  --config FILE  Shared TOML configuration file (uses sibling credentials.toml)
  -h, --help     Show this help
";

const PATH_HELP: &str = "Print the resolved credentials.toml path

Usage: tars auth path [--config <file>]

Options:
  --config FILE  Shared TOML configuration file (uses sibling credentials.toml)
  -h, --help     Show this help
";

pub(super) fn subcommand_help(command: &str) -> &'static str {
    match command {
        "set" => SET_HELP,
        "list" => LIST_HELP,
        "remove" => REMOVE_HELP,
        "path" => PATH_HELP,
        _ => HELP,
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum AuthCommand {
    Set {
        name: String,
        stdin: bool,
        config: Option<PathBuf>,
    },
    List {
        config: Option<PathBuf>,
        json: bool,
    },
    Remove {
        name: String,
        config: Option<PathBuf>,
    },
    Path {
        config: Option<PathBuf>,
    },
}

impl AuthCommand {
    pub(super) fn parse(args: &[OsString]) -> Result<Self, CliError> {
        let (subcommand, rest) = args.split_first().ok_or("auth command is required")?;
        let subcommand = subcommand.to_str().ok_or("auth command must be UTF-8")?;
        match subcommand {
            "set" => {
                let parsed = parse_auth_args(rest, SubcommandMode::Set)?;
                let name = parsed
                    .name
                    .ok_or_else(|| CliError::argument("credential name is required"))?;
                validate_name(&name)?;
                Ok(Self::Set {
                    name,
                    stdin: parsed.stdin,
                    config: parsed.config,
                })
            }
            "list" => {
                let parsed = parse_auth_args(rest, SubcommandMode::List)?;
                Ok(Self::List {
                    config: parsed.config,
                    json: parsed.json,
                })
            }
            "remove" => {
                let parsed = parse_auth_args(rest, SubcommandMode::Remove)?;
                let name = parsed
                    .name
                    .ok_or_else(|| CliError::argument("credential name is required"))?;
                validate_name(&name)?;
                Ok(Self::Remove {
                    name,
                    config: parsed.config,
                })
            }
            "path" => {
                let parsed = parse_auth_args(rest, SubcommandMode::Path)?;
                Ok(Self::Path {
                    config: parsed.config,
                })
            }
            other => Err(CliError::argument(format!(
                "unknown auth command: {}; use tars auth --help",
                escape_untrusted_text(other)
            ))),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SubcommandMode {
    Set,
    List,
    Remove,
    Path,
}

impl SubcommandMode {
    fn label(self) -> &'static str {
        match self {
            Self::Set => "set",
            Self::List => "list",
            Self::Remove => "remove",
            Self::Path => "path",
        }
    }
}

#[derive(Default)]
struct ParsedAuthArgs {
    name: Option<String>,
    stdin: bool,
    json: bool,
    config: Option<PathBuf>,
}

fn parse_auth_args(args: &[OsString], mode: SubcommandMode) -> Result<ParsedAuthArgs, CliError> {
    let mut parsed = ParsedAuthArgs::default();
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        let token = arg.to_str().ok_or("argument must be UTF-8")?;
        match token {
            "--no-config" => {
                return Err(CliError::argument(
                    "--no-config cannot be used with tars auth",
                ));
            }
            "--stdin" => {
                if mode != SubcommandMode::Set {
                    return Err(CliError::argument(format!(
                        "option --stdin is not applicable to auth {}",
                        mode.label()
                    )));
                }
                if parsed.stdin {
                    return Err(CliError::argument("duplicate option: --stdin"));
                }
                parsed.stdin = true;
            }
            "--json" => {
                if mode != SubcommandMode::List {
                    return Err(CliError::argument(format!(
                        "option --json is not applicable to auth {}",
                        mode.label()
                    )));
                }
                if parsed.json {
                    return Err(CliError::argument("duplicate option: --json"));
                }
                parsed.json = true;
            }
            "--config" => {
                let value = iter
                    .next()
                    .ok_or_else(|| CliError::argument("option --config requires a value"))?;
                if parsed.config.is_some() {
                    return Err(CliError::argument("duplicate option: --config"));
                }
                if value.is_empty() {
                    return Err(CliError::argument("option --config must not be empty"));
                }
                parsed.config = Some(PathBuf::from(value));
            }
            other if !other.starts_with('-') => match mode {
                SubcommandMode::Set | SubcommandMode::Remove => {
                    if parsed.name.is_none() {
                        parsed.name = Some(other.to_owned());
                    } else if mode == SubcommandMode::Set {
                        return Err(CliError::argument(
                            "secrets must not be passed as CLI arguments; omit the secret for an interactive prompt or use --stdin",
                        ));
                    } else {
                        return Err(CliError::argument(format!(
                            "unexpected positional argument: {}",
                            escape_untrusted_text(other)
                        )));
                    }
                }
                SubcommandMode::List | SubcommandMode::Path => {
                    return Err(CliError::argument(format!(
                        "unexpected positional argument: {}",
                        escape_untrusted_text(other)
                    )));
                }
            },
            other => {
                return Err(CliError::argument(format!(
                    "unknown option: {}",
                    escape_untrusted_text(other)
                )));
            }
        }
    }
    Ok(parsed)
}

fn validate_name(name: &str) -> Result<(), CliError> {
    if valid_credential_name(name) {
        Ok(())
    } else {
        Err(CliError::argument(
            "credential name must be 1..=64 ASCII alphanumeric, '_' or '-' characters starting with alphanumeric",
        ))
    }
}

fn resolve_credentials_path(config_path: Option<&Path>) -> Option<PathBuf> {
    let env_config = std::env::var_os("TARS_CONFIG").map(PathBuf::from);
    let effective = config_path.or(env_config.as_deref());
    credentials_path_for_config(effective)
}

pub(super) fn run(command: AuthCommand) -> ExitCode {
    match command {
        AuthCommand::Set {
            name,
            stdin,
            config,
        } => run_set(name, stdin, config.as_deref()),
        AuthCommand::List { config, json } => run_list(config.as_deref(), json),
        AuthCommand::Remove { name, config } => run_remove(name, config.as_deref()),
        AuthCommand::Path { config } => run_path(config.as_deref()),
    }
}

fn run_set(name: String, stdin: bool, config: Option<&Path>) -> ExitCode {
    if !stdin && !io::stdin().is_terminal() {
        let _ = writeln!(
            io::stderr().lock(),
            "TARS: interactive terminal required; pass --stdin to read the secret from standard input"
        );
        return ExitCode::from(2);
    }

    let Some(path) = resolve_credentials_path(config) else {
        return failure("cannot resolve credentials.toml path");
    };

    let mut secret = if stdin {
        let stdin_handle = io::stdin();
        let mut lock = stdin_handle.lock();
        match read_secret_from_stdin(&mut lock) {
            Ok(secret) => secret,
            Err(error) => return failure(error),
        }
    } else {
        if let Err(error) = CredentialsStore::check_permissions(&path) {
            return failure(error);
        }
        let prompt = format!("Enter API key for {name}: ");
        match tars_tui::read_secret_from_tty(&prompt) {
            Ok(secret) => secret,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {
                let _ = writeln!(io::stderr().lock(), "TARS: cancelled");
                return ExitCode::from(130);
            }
            Err(error) => return failure(error),
        }
    };

    let res = CredentialsStore::set(&path, &name, &secret);
    secret.clear();
    match res {
        Ok(()) => output(&format!("Saved credential {name} to {}\n", path.display())),
        Err(error) => failure(error),
    }
}

fn read_secret_from_stdin(reader: &mut dyn Read) -> Result<String, String> {
    let mut bytes = Vec::new();
    reader
        .take(MAX_STDIN_SECRET_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|err| format!("failed to read secret from stdin: {}", err.kind()))?;
    if bytes.len() > MAX_STDIN_SECRET_BYTES {
        bytes.fill(0);
        return Err("credential secret from stdin exceeds 16 KiB".to_owned());
    }
    let mut text = match String::from_utf8(bytes) {
        Ok(text) => text,
        Err(err) => {
            err.into_bytes().fill(0);
            return Err("credential secret from stdin must be valid UTF-8".to_owned());
        }
    };
    let trimmed = text.trim_end_matches(['\r', '\n']);
    if trimmed.trim().is_empty() || trimmed.chars().any(char::is_control) {
        text.clear();
        return Err(
            "credential secret must be non-empty and must not contain control characters"
                .to_owned(),
        );
    }
    let out = trimmed.to_owned();
    text.clear();
    Ok(out)
}

fn run_list(config: Option<&Path>, json: bool) -> ExitCode {
    let Some(path) = resolve_credentials_path(config) else {
        return failure("cannot resolve credentials.toml path");
    };
    let names = match CredentialsStore::load_optional(&path) {
        Ok(Some(store)) => store.list_names(),
        Ok(None) => Vec::new(),
        Err(error) => return failure(error),
    };
    if json {
        let payload = serde_json::json!({
            "path": path,
            "credentials": names,
        });
        return output(&format!("{payload}\n"));
    }
    if names.is_empty() {
        return ExitCode::SUCCESS;
    }
    let mut text = String::new();
    for name in &names {
        text.push_str(name);
        text.push('\n');
    }
    output(&text)
}

fn run_remove(name: String, config: Option<&Path>) -> ExitCode {
    let Some(path) = resolve_credentials_path(config) else {
        return failure("cannot resolve credentials.toml path");
    };
    match CredentialsStore::remove(&path, &name) {
        Ok(()) => output(&format!(
            "Removed credential {name} from {}\n",
            path.display()
        )),
        Err(error) => failure(error),
    }
}

fn run_path(config: Option<&Path>) -> ExitCode {
    let Some(path) = resolve_credentials_path(config) else {
        return failure("cannot resolve credentials.toml path");
    };
    output(&format!("{}\n", path.display()))
}

#[cfg(test)]
mod tests {
    use super::{AuthCommand, MAX_STDIN_SECRET_BYTES, read_secret_from_stdin};
    use std::ffi::OsString;
    use std::io::Cursor;
    use std::path::PathBuf;

    fn os(args: &[&str]) -> Vec<OsString> {
        args.iter().map(OsString::from).collect()
    }

    #[test]
    fn auth_parser_handles_valid_and_rejected_invocations() {
        assert_eq!(
            AuthCommand::parse(&os(&["set", "work", "--stdin", "--config", "cfg.toml"])).unwrap(),
            AuthCommand::Set {
                name: "work".to_owned(),
                stdin: true,
                config: Some(PathBuf::from("cfg.toml")),
            }
        );
        assert_eq!(
            AuthCommand::parse(&os(&["set", "--stdin", "work"])).unwrap(),
            AuthCommand::Set {
                name: "work".to_owned(),
                stdin: true,
                config: None,
            }
        );
        assert_eq!(
            AuthCommand::parse(&os(&["list", "--json"])).unwrap(),
            AuthCommand::List {
                config: None,
                json: true,
            }
        );
        assert_eq!(
            AuthCommand::parse(&os(&["remove", "work", "--config", "a.toml"])).unwrap(),
            AuthCommand::Remove {
                name: "work".to_owned(),
                config: Some(PathBuf::from("a.toml")),
            }
        );
        assert_eq!(
            AuthCommand::parse(&os(&["path"])).unwrap(),
            AuthCommand::Path { config: None }
        );

        for (args, forbidden_echo) in [
            (vec![], None),
            (vec!["bogus"], None),
            (vec!["set"], None),
            (
                vec!["set", "work", "sk-secret-cli-leak"],
                Some("sk-secret-cli-leak"),
            ),
            (vec!["set", "bad.name"], Some("bad.name")),
            (vec!["set", "_leading"], Some("_leading")),
            (vec!["set", "sk=pasted"], Some("pasted")),
            (vec!["set", "work", "--no-config"], None),
            (vec!["set", "work", "--json"], None),
            (vec!["set", "work", "--stdin", "--stdin"], None),
            (vec!["set", "work", "--config"], None),
            (vec!["set", "work", "--config", ""], None),
            (vec!["list", "--no-config"], None),
            (vec!["list", "--stdin"], None),
            (vec!["list", "extra"], None),
            (vec!["remove"], None),
            (vec!["remove", "bad.name"], Some("bad.name")),
            (vec!["remove", "work", "--no-config"], None),
            (vec!["remove", "work", "--json"], None),
            (vec!["path", "--no-config"], None),
            (vec!["path", "extra"], None),
        ] {
            let err = AuthCommand::parse(&os(&args)).expect_err("expected parse failure");
            assert!(err.is_argument(), "args={args:?}: {err:?}");
            if let Some(secret) = forbidden_echo {
                assert!(
                    !err.message().contains(secret),
                    "error leaked input {secret:?}: {}",
                    err.message()
                );
            }
        }
    }

    #[test]
    fn read_secret_from_stdin_trims_crlf_and_rejects_empty_control_and_oversize() {
        let mut ok = Cursor::new(b"sk-test-value\r\n");
        assert_eq!(read_secret_from_stdin(&mut ok).unwrap(), "sk-test-value");

        for bad in [
            b"".as_slice(),
            b"\r\n".as_slice(),
            b"   \n".as_slice(),
            b"line1\nline2\n".as_slice(),
            b"sk\ttab\n".as_slice(),
            b"sk\0nul\n".as_slice(),
            &[0xff, 0xfe],
        ] {
            let mut cursor = Cursor::new(bad);
            assert!(read_secret_from_stdin(&mut cursor).is_err());
        }

        let oversized = vec![b'a'; MAX_STDIN_SECRET_BYTES + 1];
        let mut cursor = Cursor::new(oversized);
        let err = read_secret_from_stdin(&mut cursor).unwrap_err();
        assert!(err.contains("16 KiB"), "{err}");
    }
}
