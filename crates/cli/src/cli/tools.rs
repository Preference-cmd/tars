//! External tool status and consent-gated installation commands.
//!
//! These commands only display shared state and collect the user's answer;
//! every check lives in [`tars_core::external_tools`]. `status` never uses the
//! network, and `install` downloads only after an explicit yes from the
//! terminal or `--yes` on the command line.

use super::command::output;
use super::configuration::CliError;
use std::ffi::OsString;
use std::io::{self, BufRead, IsTerminal, Write};
use std::path::PathBuf;
use std::process::ExitCode;
use tars_core::config::{Config, ConfigSource};
use tars_core::external_tools::{
    ExternalTools, InstallError, RIPGREP_VERSION, ToolSource, install_managed_rg,
};
use tars_core::sandbox::{Sandbox, SandboxStatus};

pub(super) const HELP: &str = "External tools used by TARS

Usage: tars tools status [--json] [--config <file> | --no-config]
       tars tools install rg [--yes] [--config <file> | --no-config]

Commands:
  status         Show each tool's source, path, version, state and the sandbox backend probe
  install rg     Download and install the managed ripgrep after confirmation

Options:
  --yes          Answer the download question without prompting
  --json         Structured output for status
  --config       Shared TOML configuration file
  --no-config    Ignore global configuration; conflicts with --config

status never downloads. install rg downloads only after the question is
answered yes or --yes is given; a non-interactive run without --yes refuses.
The managed copy is ripgrep 15.2.0 from github.com/BurntSushi/ripgrep,
verified against the SHA-256 compiled into TARS. The `[external_tools] rg`
setting selects managed (default), system, or an absolute executable path.
Run `tars tools status` to see where the executable comes from and what to do
when it is unavailable.
";

pub(super) enum ToolsCommand {
    Status { json: bool, selection: Selection },
    Install { yes: bool, selection: Selection },
}

impl ToolsCommand {
    pub(super) fn parse(args: &[OsString]) -> Result<Self, CliError> {
        let (command, rest) = args.split_first().ok_or("tools command is required")?;
        let command = command.to_str().ok_or("tools command must be UTF-8")?;
        match command {
            "status" => {
                let mut json = false;
                let mut selection = Selection::default();
                let mut rest = rest.iter();
                while let Some(flag) = rest.next() {
                    let flag = flag.to_str().ok_or("option must be UTF-8")?;
                    match flag {
                        "--json" if !json => json = true,
                        "--config" | "--no-config" => selection.insert(flag, &mut rest)?,
                        _ => {
                            return Err(
                                "status accepts only --json, --config and --no-config".into()
                            );
                        }
                    }
                }
                Ok(Self::Status { json, selection })
            }
            "install" => {
                let mut rest = rest.iter();
                let name = rest
                    .next()
                    .ok_or("install requires a tool name; the only tool is rg")?
                    .to_str()
                    .ok_or("tool name must be UTF-8")?;
                if name != "rg" {
                    return Err("only rg can be installed".into());
                }
                let mut yes = false;
                let mut selection = Selection::default();
                while let Some(flag) = rest.next() {
                    let flag = flag.to_str().ok_or("option must be UTF-8")?;
                    match flag {
                        "--yes" if !yes => yes = true,
                        "--config" | "--no-config" => selection.insert(flag, &mut rest)?,
                        _ => {
                            return Err(
                                "install accepts only --yes, --config and --no-config".into()
                            );
                        }
                    }
                }
                Ok(Self::Install { yes, selection })
            }
            _ => Err("unknown tools command; use tars tools --help".into()),
        }
    }
}

#[derive(Default)]
pub(super) struct Selection {
    config: Option<PathBuf>,
    no_config: bool,
}

impl Selection {
    fn insert(
        &mut self,
        flag: &str,
        values: &mut std::slice::Iter<'_, OsString>,
    ) -> Result<(), CliError> {
        match flag {
            "--config" if self.config.is_none() => {
                let path = values.next().ok_or("--config requires a path")?;
                self.config = Some(PathBuf::from(path));
            }
            "--no-config" if !self.no_config => self.no_config = true,
            _ => return Err(format!("{flag} was given more than once").into()),
        }
        Ok(())
    }

    fn load(&self) -> Result<Config, CliError> {
        let source = match (&self.config, self.no_config) {
            (Some(_), true) => {
                return Err(CliError::argument(
                    "--config and --no-config are mutually exclusive",
                ));
            }
            (Some(path), false) => ConfigSource::File(path.clone()),
            (None, true) => ConfigSource::Disabled,
            (None, false) => ConfigSource::Discover,
        };
        let config =
            Config::load(source).map_err(|error| CliError::configuration(error.to_string()))?;
        for warning in config.warnings() {
            eprintln!("{warning}");
        }
        Ok(config)
    }
}

pub(super) fn run(command: ToolsCommand) -> ExitCode {
    match command {
        ToolsCommand::Status { json, selection } => status(json, &selection),
        ToolsCommand::Install { yes, selection } => install(yes, &selection),
    }
}

fn status(json: bool, selection: &Selection) -> ExitCode {
    let config = match selection.load() {
        Ok(config) => config,
        Err(error) => return command_failure(error),
    };
    let tools = ExternalTools::resolve(&config);
    let sandbox = sandbox_status();
    if json {
        output(&json_status(&tools, &sandbox))
    } else {
        output(&text_status(&tools, &sandbox))
    }
}

fn install(yes: bool, selection: &Selection) -> ExitCode {
    let config = match selection.load() {
        Ok(config) => config,
        Err(error) => return command_failure(error),
    };
    let tools = ExternalTools::resolve(&config);
    let rg = tools.rg();
    if rg.is_ready() {
        return output(&format!(
            "rg {} is already available at {}\n",
            rg.version.as_deref().unwrap_or(RIPGREP_VERSION),
            display_path(rg.path.as_deref())
        ));
    }
    if !matches!(rg.source, ToolSource::Managed) {
        return command_failure(CliError::configuration(format!(
            "rg is configured as {}; set [external_tools] rg = \"managed\" to install the managed copy",
            rg.source
        )));
    }
    let interactive = io::stdin().is_terminal();
    if !yes {
        if !interactive {
            return command_failure(CliError::configuration(
                "the download must be confirmed in a terminal; re-run with --yes to download without a prompt",
            ));
        }
        match confirm(&format!(
            "Download ripgrep {RIPGREP_VERSION} (about 2 MB from github.com/BurntSushi/ripgrep) and install it for search? [y/N] "
        )) {
            Ok(true) => {}
            Ok(false) => return output("Cancelled; nothing was downloaded.\n"),
            Err(error) => {
                return command_failure(CliError::configuration(format!(
                    "could not read the confirmation: {error}"
                )));
            }
        }
    }
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            return command_failure(CliError::configuration(format!(
                "could not start the download runtime: {error}"
            )));
        }
    };
    match runtime.block_on(install_managed_rg(&config)) {
        Ok(status) => output(&format!(
            "Installed rg {} at {}\n",
            status.version.as_deref().unwrap_or(RIPGREP_VERSION),
            display_path(status.path.as_deref())
        )),
        Err(error) => command_failure(install_failure(error)),
    }
}

fn install_failure(error: InstallError) -> CliError {
    CliError::configuration(error.to_string())
}

fn confirm(prompt: &str) -> io::Result<bool> {
    let mut stderr = io::stderr().lock();
    write!(stderr, "{prompt}")?;
    stderr.flush()?;
    let mut answer = String::new();
    io::stdin().lock().read_line(&mut answer)?;
    Ok(matches!(
        answer.trim().to_ascii_lowercase().as_str(),
        "y" | "yes"
    ))
}

fn sandbox_status() -> SandboxStatus {
    match Sandbox::open() {
        Ok(sandbox) => SandboxStatus::Available(sandbox.report().clone()),
        Err(error) => SandboxStatus::Unavailable(error),
    }
}

fn text_status(tools: &ExternalTools, sandbox: &SandboxStatus) -> String {
    let rg = tools.rg();
    let mut text = format!(
        "rg: {} ({})\n",
        if rg.is_ready() {
            "ready"
        } else {
            "unavailable"
        },
        rg.source.kind()
    );
    text.push_str(&format!("  path: {}\n", display_path(rg.path.as_deref())));
    text.push_str(&format!(
        "  version: {}\n",
        rg.version.as_deref().unwrap_or("-")
    ));
    if let Some(reason) = rg.unavailable_reason() {
        text.push_str(&format!("  reason: {reason}\n"));
    }
    text.push_str(&format!("sandbox: {}\n", sandbox.brief()));
    text
}

fn json_status(tools: &ExternalTools, sandbox: &SandboxStatus) -> String {
    let rg = tools.rg();
    let value = serde_json::json!({
        "tools": [{
            "name": rg.name,
            "source": rg.source.kind(),
            "path": rg.path,
            "version": rg.version,
            "status": if rg.is_ready() { "ready" } else { "unavailable" },
            "reason": rg.unavailable_reason(),
        }],
        "sandbox": {
            "available": sandbox.is_available(),
            "detail": sandbox.brief(),
        },
    });
    let text = serde_json::to_string(&value).unwrap_or_else(|_| "{}".to_owned());
    format!("{text}\n")
}

fn display_path(path: Option<&std::path::Path>) -> String {
    path.map(|path| path.display().to_string())
        .unwrap_or_else(|| "-".to_owned())
}

fn command_failure(error: CliError) -> ExitCode {
    let hint = if error.is_argument() {
        ". Use tars tools --help."
    } else {
        ""
    };
    let _ = writeln!(io::stderr().lock(), "TARS: {}{hint}", error.message());
    error.exit_code()
}
