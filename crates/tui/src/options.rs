use std::{path::PathBuf, sync::Arc};
use tars_core::{
    config::{Config, ConfigSource, default_config_path, default_store_dir},
    definition::SessionDefinition,
};

/// Action requested by top-level interactive startup flags.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub(crate) enum StartupAction {
    #[default]
    Default,
    ContinueLatest,
    Resume(String),
    OpenSessionPicker,
}

pub(crate) struct Options {
    pub store: PathBuf,
    pub recovery_dir: PathBuf,
    pub config: Arc<Config>,
    pub profile: Option<String>,
    pub preset: Option<SessionDefinition>,
    pub config_path: Option<PathBuf>,
    pub workspace: Option<PathBuf>,
    pub code_execution_enabled: Option<bool>,
    pub no_config: bool,
    pub auto_onboard: bool,
    pub onboarding_config_path: Option<PathBuf>,
    pub startup_action: StartupAction,
}

pub(crate) fn parse(args: impl Iterator<Item = String>) -> Result<Option<Options>, String> {
    let mut args = args.peekable();
    let mut store = None;
    let mut recovery_dir = None;
    let mut config_path = None;
    let mut profile = None;
    let mut workspace = None;
    let mut no_config = false;
    let mut code_execution = false;
    let mut no_code_execution = false;
    let mut continue_flag: Option<&'static str> = None;
    let mut resume_flag: Option<(&'static str, StartupAction)> = None;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--help" | "-h" => {
                println!(
                    "TARS interactive terminal\n\n\
                     Usage: tars [--config FILE | --no-config] [--profile NAME]\n            \
                     [--store DIR] [--recovery-dir DIR] [--workspace DIR]\n            \
                     [-c | --continue | -r | --resume [SELECTOR]]\n            \
                     [--code-execution | --no-code-execution]\n\n\
                     When configuration is enabled, store defaults to ~/.tars/data (or $TARS_HOME/data)\n\
                     unless overridden via --store or TOML. With --no-config, --store DIR is required.\n\n\
                     Slash commands:\n  \
                     /doctor    Check configuration, credentials, storage, sandbox and tools\n  \
                     /init      Analyze the workspace and create or update AGENTS.md\n  \
                     /new       Create a session with model settings (F2)\n  \
                     /clear     Start a fresh session with the current model preset and workspace\n  \
                     /resume    Open the session picker or jump to a session by ID/prefix\n  \
                     /model     Switch model profile in a new session\n  \
                     /sessions  Find and open a saved or active session\n  \
                     /agents    List session agents and switch transcript view\n  \
                     /mode      Show or change permission mode (Shift-Tab cycles)\n  \
                     /compact   Summarize and replace the current context\n  \
                     /help      Keyboard shortcuts and session behavior\n\n\
                     Keys: / or F1 commands · Enter / Ctrl-S submit · Ctrl-J newline · Tab complete\n      \
                     PageUp/Down scroll · F3 open · F4 next · F5 refresh · F6 tools · F7 repair\n      \
                     Esc / Ctrl-C cancel · Ctrl-W close · Ctrl-Q quit · Ctrl-R retry · Ctrl-E export"
                );
                return Ok(None);
            }
            "--version" | "-V" => {
                println!("tars {}", env!("CARGO_PKG_VERSION"));
                return Ok(None);
            }
            "--no-config" if !no_config => no_config = true,
            "--no-config" => return Err("--no-config specified twice".into()),
            "--code-execution" if !code_execution => code_execution = true,
            "--no-code-execution" if !no_code_execution => no_code_execution = true,
            "--code-execution" => return Err("--code-execution specified twice".into()),
            "--no-code-execution" => return Err("--no-code-execution specified twice".into()),
            "-c" | "--continue" => {
                let flag = if arg == "-c" { "-c" } else { "--continue" };
                if let Some(prev) = continue_flag {
                    if prev == flag {
                        return Err(format!("{flag} specified twice"));
                    }
                    return Err(format!("{prev} and {flag} are mutually exclusive"));
                }
                continue_flag = Some(flag);
            }
            "-r" | "--resume" => {
                let flag = if arg == "-r" { "-r" } else { "--resume" };
                if let Some((prev, _)) = &resume_flag {
                    if *prev == flag {
                        return Err(format!("{flag} specified twice"));
                    }
                    return Err(format!("{prev} and {flag} are mutually exclusive"));
                }
                let action = match args.peek() {
                    Some(next) if !next.starts_with('-') => {
                        let selector = args.next().expect("peeked selector");
                        if selector.trim().is_empty() {
                            return Err(format!("{flag} selector must not be empty"));
                        }
                        StartupAction::Resume(selector)
                    }
                    _ => StartupAction::OpenSessionPicker,
                };
                resume_flag = Some((flag, action));
            }
            "--store" | "--recovery-dir" | "--config" | "--profile" | "--workspace" => {
                let value = args
                    .next()
                    .ok_or_else(|| format!("{arg} requires a value"))?;
                if value.is_empty() || value.starts_with("--") {
                    return Err(format!("{arg} requires a value"));
                }
                if arg == "--store" {
                    if store.replace(PathBuf::from(value)).is_some() {
                        return Err("--store specified twice".into());
                    }
                } else if arg == "--recovery-dir" {
                    if recovery_dir.replace(PathBuf::from(value)).is_some() {
                        return Err("--recovery-dir specified twice".into());
                    }
                } else if arg == "--config" {
                    if config_path.replace(PathBuf::from(value)).is_some() {
                        return Err("--config specified twice".into());
                    }
                } else if arg == "--workspace" {
                    if workspace.replace(PathBuf::from(value)).is_some() {
                        return Err("--workspace specified twice".into());
                    }
                } else if profile.replace(value).is_some() {
                    return Err("--profile specified twice".into());
                }
            }
            _ => return Err(format!("unknown argument: {arg}; use --help")),
        }
    }
    let startup_action = match (continue_flag, resume_flag) {
        (Some(_), Some(_)) => {
            return Err("--continue (-c) and --resume (-r) are mutually exclusive".into());
        }
        (Some(_), None) => StartupAction::ContinueLatest,
        (None, Some((_, action))) => action,
        (None, None) => StartupAction::Default,
    };
    let code_execution_enabled = match (code_execution, no_code_execution) {
        (true, true) => {
            return Err("--code-execution and --no-code-execution are mutually exclusive".into());
        }
        (true, false) => Some(true),
        (false, true) => Some(false),
        (false, false) => None,
    };
    let source = match (config_path, no_config) {
        (Some(_), true) => return Err("--config and --no-config are mutually exclusive".into()),
        (Some(path), false) => ConfigSource::File(path),
        (None, true) => ConfigSource::Disabled,
        (None, false) => ConfigSource::Discover,
    };
    let config = Arc::new(Config::load(source).map_err(|e| e.to_string())?);
    let preset = match (
        config
            .preset(profile.as_deref())
            .map_err(|e| e.to_string())?,
        code_execution_enabled,
    ) {
        (Some(_), Some(_)) => Some(
            config
                .definition(
                    profile.as_deref(),
                    tars_core::config::DefinitionOverrides {
                        code_execution_enabled,
                        ..Default::default()
                    },
                )
                .map_err(|e| e.to_string())?,
        ),
        (preset, _) => preset,
    };
    let config_path = config.source().map(PathBuf::from);
    let auto_onboard = !no_config && config_path.is_none() && preset.is_none();
    let onboarding_config_path = default_config_path();
    let store = store
        .or_else(|| config.store_dir().map(PathBuf::from))
        .or_else(|| (!no_config).then(default_store_dir).flatten())
        .ok_or("--store or a configured store directory is required")?;
    Ok(Some(Options {
        store,
        recovery_dir: recovery_dir
            .or_else(|| config.recovery_dir().map(PathBuf::from))
            .unwrap_or_else(|| PathBuf::from(".")),
        config,
        profile,
        preset,
        config_path,
        workspace,
        code_execution_enabled,
        no_config,
        auto_onboard,
        onboarding_config_path,
        startup_action,
    }))
}
