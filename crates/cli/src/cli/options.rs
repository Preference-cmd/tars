use super::configuration::{self, CliError, PreludeCliOptions};
use std::collections::HashMap;
use std::ffi::OsString;
use std::path::PathBuf;
use tars_core::definition::SessionDefinition;
use tars_core::external_tools::ExternalTools;

pub(super) struct RunOptions {
    pub definition: SessionDefinition,
    /// Resolved once from the loaded configuration and handed to the host.
    pub external_tools: ExternalTools,
    pub model_profiles: std::collections::BTreeMap<String, tars_core::agents::ModelProfile>,
    pub user_rules_path: Option<PathBuf>,
    pub input: Option<String>,
    pub stdin: bool,
    pub images: Vec<PathBuf>,
    pub load: Option<PathBuf>,
    pub save: Option<PathBuf>,
    pub json: bool,
}

impl RunOptions {
    pub fn parse(args: &[OsString]) -> Result<Self, CliError> {
        let mut values = HashMap::new();
        let mut skill_dirs = Vec::new();
        let mut images = Vec::new();
        let mut no_skills = false;
        let mut code_execution = false;
        let mut no_code_execution = false;
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
                "--no-skills" if !no_skills => no_skills = true,
                "--code-execution" if !code_execution => code_execution = true,
                "--no-code-execution" if !no_code_execution => no_code_execution = true,
                "--skill-dir" => {
                    let value = args.next().ok_or("option needs a value")?;
                    if value.is_empty() {
                        return Err("empty --skill-dir option".into());
                    }
                    skill_dirs.push(PathBuf::from(value));
                }
                "--image" => {
                    let value = args.next().ok_or("option needs a value")?;
                    if value.is_empty() {
                        return Err("empty --image option".into());
                    }
                    images.push(PathBuf::from(value));
                }
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
                | "--permission-mode"
                | "--system"
                | "--system-file"
                | "--dynamic-context"
                | "--dynamic-context-file" => {
                    let value = args.next().ok_or("option needs a value")?;
                    if (value.is_empty()
                        && !matches!(flag, "--tools" | "--mcp" | "--system" | "--dynamic-context"))
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
        let code_execution_enabled =
            configuration::resolve_code_execution(code_execution, no_code_execution)?;
        let config = configuration::load(&values, no_config)?;
        let external_tools = ExternalTools::resolve(&config);
        let model_profiles = config.model_profiles();
        let user_rules_path = config.config_dir().map(|dir| dir.join("permissions.toml"));
        let mut definition = configuration::definition(
            &config,
            &values,
            PreludeCliOptions {
                skill_dirs,
                no_skills,
            },
            code_execution_enabled,
        )?;
        // Transient `tars run` executes one initial user work plus up to 16
        // reactive auto-wakeups; keep retained work bounded while preserving all
        // observations until convergence completes.
        definition.retained_work_capacity = 17;
        let input = string("--input")?;
        super::managed_execution::validate_prompt_options(
            input.as_deref(),
            stdin,
            !images.is_empty(),
        )?;
        Ok(Self {
            definition,
            external_tools,
            model_profiles,
            user_rules_path,
            input,
            stdin,
            images,
            load: values.get("--load").map(PathBuf::from),
            save: values.get("--save").map(PathBuf::from),
            json,
        })
    }
}
