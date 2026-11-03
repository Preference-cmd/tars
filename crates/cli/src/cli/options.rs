use super::configuration::{self, CliError};
use std::collections::HashMap;
use std::ffi::OsString;
use std::path::PathBuf;
use tars_core::definition::SessionDefinition;
use tars_core::external_tools::ExternalTools;

pub(super) struct RunOptions {
    pub definition: SessionDefinition,
    /// Resolved once from the loaded configuration and handed to the host.
    pub external_tools: ExternalTools,
    pub input: Option<String>,
    pub load: Option<PathBuf>,
    pub save: Option<PathBuf>,
    pub json: bool,
}

impl RunOptions {
    pub fn parse(args: &[OsString]) -> Result<Self, CliError> {
        let mut values = HashMap::new();
        let mut stdin = false;
        let mut json = false;
        let mut no_config = false;
        let mut args = args.iter();
        while let Some(flag) = args.next() {
            let flag = flag.to_str().ok_or("option must be UTF-8")?;
            match flag {
                "--stdin" if !stdin => stdin = true,
                "--json" if !json => json = true,
                "--no-config" if !no_config => no_config = true,
                "--provider"
                | "--model"
                | "--api-key-env"
                | "--endpoint"
                | "--input"
                | "--workspace"
                | "--tools"
                | "--command-network"
                | "--mcp"
                | "--load"
                | "--save"
                | "--timeout-secs"
                | "--config"
                | "--profile"
                | "--max-tokens"
                | "--reasoning-effort"
                | "--extra-body"
                | "--render-options"
                | "--max-model-attempts"
                | "--context-window"
                | "--auto-compact"
                | "--prompt-cache"
                | "--permission-mode" => {
                    let value = args.next().ok_or("option needs a value")?;
                    if (value.is_empty() && !matches!(flag, "--tools" | "--mcp"))
                        || values.insert(flag, value).is_some()
                    {
                        return Err("empty or duplicate option".into());
                    }
                }
                _ => return Err("unknown or duplicate option".into()),
            }
        }
        let string = |key| -> Result<Option<String>, CliError> {
            values
                .get(key)
                .map(|value| {
                    value
                        .to_str()
                        .map(str::to_owned)
                        .ok_or_else(|| CliError::argument("text option must be UTF-8"))
                })
                .transpose()
        };
        let config = configuration::load(&values, no_config)?;
        let external_tools = ExternalTools::resolve(&config);
        let mut definition = configuration::definition(&config, &values)?;
        // This entry performs exactly one transient work; it does not retain an
        // interactive work history even when the preset has a larger capacity.
        definition.retained_work_capacity = 1;
        let input = string("--input")?;
        if stdin == input.is_some() {
            return Err("choose exactly one of --input or --stdin".into());
        }
        if input.as_ref().is_some_and(|s| s.trim().is_empty()) {
            return Err("input must not be blank".into());
        }
        Ok(Self {
            definition,
            external_tools,
            input,
            load: values.get("--load").map(PathBuf::from),
            save: values.get("--save").map(PathBuf::from),
            json,
        })
    }
}
