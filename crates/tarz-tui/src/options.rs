use std::path::PathBuf;
use tarz_core::{
    config::{Config, ConfigSource},
    definition::SessionDefinition,
};

pub(crate) struct Options {
    pub store: PathBuf,
    pub recovery_dir: PathBuf,
    pub preset: Option<SessionDefinition>,
    pub config_path: Option<PathBuf>,
}

pub(crate) fn parse(args: impl Iterator<Item = String>) -> Result<Option<Options>, String> {
    let mut args = args.peekable();
    let mut store = None;
    let mut recovery_dir = None;
    let mut config_path = None;
    let mut profile = None;
    let mut no_config = false;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--help" | "-h" => {
                println!(
                    "Tarz TUI prototype\n\nUsage: tarz-tui [--config FILE | --no-config] [--profile NAME]\n                [--store DIR] [--recovery-dir DIR]\n\nStore is required via a flag or shared TOML configuration. Explicit directories override configuration.\nF2 new  F3 open  F4 next session  F5 refresh  Ctrl-S submit  Enter newline\nCtrl-C cancel  Ctrl-W close  Ctrl-Q quit  Ctrl-R retry save\nCtrl-E export  Ctrl-D discard (confirmation)  PageUp/Down scroll\n\nNew-session forms start from the selected model preset. Credentials use environment variable references.\nForm changes affect only the new session; they do not rewrite global configuration.\nRecovery exports never overwrite an existing file. GUI, streaming and installation are not included."
                );
                return Ok(None);
            }
            "--version" => {
                println!("tarz-tui {} (prototype)", env!("CARGO_PKG_VERSION"));
                return Ok(None);
            }
            "--no-config" if !no_config => no_config = true,
            "--store" | "--recovery-dir" | "--config" | "--profile" => {
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
                } else if profile.replace(value).is_some() {
                    return Err("--profile specified twice".into());
                }
            }
            _ => return Err(format!("unknown argument: {arg}; use --help")),
        }
    }
    let source = match (config_path, no_config) {
        (Some(_), true) => return Err("--config and --no-config are mutually exclusive".into()),
        (Some(path), false) => ConfigSource::File(path),
        (None, true) => ConfigSource::Disabled,
        (None, false) => ConfigSource::Discover,
    };
    let config = Config::load(source).map_err(|e| e.to_string())?;
    Ok(Some(Options {
        store: store
            .or_else(|| config.store_dir().map(PathBuf::from))
            .ok_or("--store or a configured store directory is required")?,
        recovery_dir: recovery_dir
            .or_else(|| config.recovery_dir().map(PathBuf::from))
            .unwrap_or_else(|| PathBuf::from(".")),
        preset: config
            .preset(profile.as_deref())
            .map_err(|e| e.to_string())?,
        config_path: config.source().map(PathBuf::from),
    }))
}
