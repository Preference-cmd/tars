use crate::cli::configuration::{self, CliError, PreludeCliOptions};
use std::collections::HashMap;
use std::ffi::OsString;
use std::path::PathBuf;
use tars_core::definition::SessionDefinition;
use tars_core::external_tools::ExternalTools;

pub(crate) enum SessionCommand {
    ExportMaterials {
        record: PathBuf,
        output: PathBuf,
    },
    ImportLegacy {
        record: PathBuf,
        store: PathBuf,
        json: bool,
    },
    Create {
        store: PathBuf,
        // Boxed: the definition is far larger than the other variants.
        definition: Box<SessionDefinition>,
        /// Resolved once from the loaded configuration and handed to the host.
        external_tools: ExternalTools,
        model_profiles: std::collections::BTreeMap<String, tars_core::agents::ModelProfile>,
        user_rules_path: Option<PathBuf>,
        trust_workspace_rules: bool,
        trust_workspace_agents: bool,
        load: Option<PathBuf>,
        recovery_dir: PathBuf,
        json: bool,
    },
    Run {
        store: PathBuf,
        id: String,
        /// Resolved once from the loaded configuration and handed to the host.
        external_tools: ExternalTools,
        model_profiles: std::collections::BTreeMap<String, tars_core::agents::ModelProfile>,
        user_rules_path: Option<PathBuf>,
        trust_workspace_rules: bool,
        trust_workspace_agents: bool,
        input: Option<String>,
        permission_mode: Option<tars_core::permission::PermissionMode>,
        recovery_dir: PathBuf,
        json: bool,
    },
    List {
        store: PathBuf,
        json: bool,
    },
    Compact {
        store: PathBuf,
        id: String,
        recovery_dir: PathBuf,
        json: bool,
    },
    History {
        store: PathBuf,
        id: String,
        agent: Option<String>,
        last: Option<usize>,
        json: bool,
    },
    Show {
        store: PathBuf,
        id: String,
        agent: Option<String>,
        json: bool,
    },
    Repair {
        store: PathBuf,
        id: String,
        apply: bool,
        expected_revision: Option<u64>,
        recovery_dir: PathBuf,
        json: bool,
    },
}

impl SessionCommand {
    pub(crate) fn parse(args: &[OsString]) -> Result<Self, CliError> {
        let (command, mut args) = args.split_first().ok_or("session command is required")?;
        let command = command.to_str().ok_or("session command must be UTF-8")?;
        if command == "export-materials" {
            let mut values = HashMap::new();
            let mut options = args.iter();
            while let Some(flag) = options.next() {
                let flag = flag.to_str().ok_or("option must be UTF-8")?;
                if !matches!(flag, "--record" | "--output") {
                    return Err("export-materials accepts only --record and --output".into());
                }
                insert(&mut values, flag, &mut options)?;
            }
            return Ok(Self::ExportMaterials {
                record: values
                    .get("--record")
                    .map(PathBuf::from)
                    .ok_or("--record is required")?,
                output: values
                    .get("--output")
                    .map(PathBuf::from)
                    .ok_or("--output is required")?,
            });
        }
        let id = if matches!(command, "run" | "repair" | "compact" | "history" | "show") {
            let (id, rest) = args.split_first().ok_or("conversation ID is required")?;
            args = rest;
            Some(
                id.to_str()
                    .ok_or("conversation ID must be UTF-8")?
                    .to_owned(),
            )
        } else {
            None
        };
        if !matches!(
            command,
            "create" | "run" | "list" | "repair" | "compact" | "history" | "show" | "import-legacy"
        ) {
            return Err("unknown session command".into());
        }
        let mut values = HashMap::new();
        let mut skill_dirs = Vec::new();
        let mut no_skills = false;
        let mut code_execution = false;
        let mut no_code_execution = false;
        let mut stdin = false;
        let mut json = false;
        let mut no_config = false;
        let mut trust_workspace_rules = false;
        let mut trust_workspace_agents = false;
        let mut apply = false;
        let mut dry_run = false;
        let mut args = args.iter();
        while let Some(flag) = args.next() {
            let flag = flag.to_str().ok_or("option must be UTF-8")?;
            match flag {
                "--json" if !json => json = true,
                "--no-config" if !no_config => no_config = true,
                "--trust-workspace-rules"
                    if matches!(command, "create" | "run") && !trust_workspace_rules =>
                {
                    trust_workspace_rules = true;
                }
                "--trust-workspace-agents"
                    if matches!(command, "create" | "run") && !trust_workspace_agents =>
                {
                    trust_workspace_agents = true;
                }
                "--no-skills" if command == "create" && !no_skills => no_skills = true,
                "--code-execution" if command == "create" && !code_execution => {
                    code_execution = true;
                }
                "--no-code-execution" if command == "create" && !no_code_execution => {
                    no_code_execution = true;
                }
                "--stdin" if command == "run" && !stdin => stdin = true,
                "--apply" if command == "repair" && !apply => apply = true,
                "--dry-run" if command == "repair" && !dry_run => dry_run = true,
                "--store" | "--config" => {
                    insert(&mut values, flag, &mut args)?;
                }
                "--skill-dir" if command == "create" => {
                    let value = args
                        .next()
                        .ok_or_else(|| CliError::argument("option needs a value"))?;
                    if value.is_empty() {
                        return Err(CliError::argument("empty --skill-dir option"));
                    }
                    skill_dirs.push(PathBuf::from(value));
                }
                "--provider"
                | "--model"
                | "--api-key-env"
                | "--endpoint"
                | "--timeout-secs"
                | "--capacity"
                | "--max-model-rounds"
                | "--max-tool-calls"
                | "--load"
                | "--max-tokens"
                | "--reasoning-effort"
                | "--extra-body"
                | "--render-options"
                | "--max-model-attempts"
                | "--profile"
                | "--workspace"
                | "--tools"
                | "--command-network"
                | "--mcp"
                | "--context-window"
                | "--auto-compact"
                | "--prompt-cache"
                | "--system"
                | "--system-file"
                | "--dynamic-context"
                | "--dynamic-context-file"
                    if command == "create" =>
                {
                    insert(&mut values, flag, &mut args)?;
                }
                "--input" if command == "run" => {
                    insert(&mut values, flag, &mut args)?;
                }
                "--last" if command == "history" => {
                    insert(&mut values, flag, &mut args)?;
                }
                "--agent" if matches!(command, "show" | "history") => {
                    insert(&mut values, flag, &mut args)?;
                }
                "--permission-mode" if matches!(command, "create" | "run") => {
                    insert(&mut values, flag, &mut args)?;
                }
                "--expected-revision" if command == "repair" => {
                    insert(&mut values, flag, &mut args)?;
                }
                "--record" if command == "import-legacy" => {
                    insert(&mut values, flag, &mut args)?;
                }
                "--recovery-dir" if matches!(command, "create" | "run" | "repair" | "compact") => {
                    insert(&mut values, flag, &mut args)?;
                }
                _ => return Err("unknown, duplicate, or inapplicable session option".into()),
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
        let model_profiles = config.model_profiles();
        let user_rules_path = config.config_dir().map(|dir| dir.join("permissions.toml"));
        let store = values
            .get("--store")
            .map(PathBuf::from)
            .or_else(|| config.store_dir().map(PathBuf::from))
            .ok_or("--store or a configured store directory is required")?;
        match command {
            "repair" => {
                if apply && dry_run {
                    return Err("--apply conflicts with --dry-run".into());
                }
                let expected_revision = string("--expected-revision")?
                    .map(|value| {
                        value
                            .parse::<u64>()
                            .ok()
                            .filter(|revision| *revision > 0)
                            .ok_or("--expected-revision must be a positive integer")
                    })
                    .transpose()?;
                if apply != expected_revision.is_some() {
                    return Err(
                        "--apply requires --expected-revision; preview does not accept a revision"
                            .into(),
                    );
                }
                Ok(Self::Repair {
                    store,
                    id: id.expect("repair ID parsed"),
                    apply,
                    expected_revision,
                    recovery_dir: values
                        .get("--recovery-dir")
                        .map(PathBuf::from)
                        .or_else(|| config.recovery_dir().map(PathBuf::from))
                        .unwrap_or_else(|| PathBuf::from(".")),
                    json,
                })
            }
            "create" => {
                let code_execution_enabled =
                    configuration::resolve_code_execution(code_execution, no_code_execution)?;
                let definition = configuration::definition(
                    &config,
                    &values,
                    PreludeCliOptions {
                        skill_dirs,
                        no_skills,
                    },
                    code_execution_enabled,
                )?;
                Ok(Self::Create {
                    store,
                    definition: Box::new(definition),
                    external_tools: ExternalTools::resolve(&config),
                    model_profiles,
                    user_rules_path,
                    trust_workspace_rules,
                    trust_workspace_agents,
                    load: values.get("--load").map(PathBuf::from),
                    recovery_dir: values
                        .get("--recovery-dir")
                        .map(PathBuf::from)
                        .or_else(|| config.recovery_dir().map(PathBuf::from))
                        .unwrap_or_else(|| PathBuf::from(".")),
                    json,
                })
            }
            "run" => {
                let input = string("--input")?;
                if stdin == input.is_some() {
                    return Err("choose exactly one of --input or --stdin".into());
                }
                if input.as_ref().is_some_and(|value| value.trim().is_empty()) {
                    return Err("input must not be blank".into());
                }
                let permission_mode = string("--permission-mode")?
                    .map(|value| {
                        value
                            .parse::<tars_core::permission::PermissionMode>()
                            .map_err(|_| "--permission-mode must be read-only, ask, accept-edits, or full-access".to_owned())
                    })
                    .transpose()?;
                Ok(Self::Run {
                    store,
                    id: id.expect("run ID parsed"),
                    external_tools: ExternalTools::resolve(&config),
                    model_profiles,
                    user_rules_path,
                    trust_workspace_rules,
                    trust_workspace_agents,
                    input,
                    permission_mode,
                    recovery_dir: values
                        .get("--recovery-dir")
                        .map(PathBuf::from)
                        .or_else(|| config.recovery_dir().map(PathBuf::from))
                        .unwrap_or_else(|| PathBuf::from(".")),
                    json,
                })
            }
            "compact" => Ok(Self::Compact {
                store,
                id: id.expect("compact ID parsed"),
                recovery_dir: values
                    .get("--recovery-dir")
                    .map(PathBuf::from)
                    .or_else(|| config.recovery_dir().map(PathBuf::from))
                    .unwrap_or_else(|| PathBuf::from(".")),
                json,
            }),
            "history" => {
                let last = string("--last")?
                    .map(|value| {
                        value
                            .parse::<usize>()
                            .ok()
                            .filter(|count| *count > 0)
                            .ok_or("--last must be a positive integer")
                    })
                    .transpose()?;
                Ok(Self::History {
                    store,
                    id: id.expect("history ID parsed"),
                    agent: string("--agent")?,
                    last,
                    json,
                })
            }
            "show" => Ok(Self::Show {
                store,
                id: id.expect("show ID parsed"),
                agent: string("--agent")?,
                json,
            }),
            "import-legacy" => Ok(Self::ImportLegacy {
                record: values
                    .get("--record")
                    .map(PathBuf::from)
                    .ok_or("--record is required")?,
                store,
                json,
            }),
            _ => Ok(Self::List { store, json }),
        }
    }
}

fn insert<'a>(
    values: &mut HashMap<&'a str, &'a OsString>,
    flag: &'a str,
    args: &mut std::slice::Iter<'a, OsString>,
) -> Result<(), CliError> {
    let value = args
        .next()
        .ok_or_else(|| CliError::argument("option needs a value"))?;
    if (value.is_empty() && !matches!(flag, "--tools" | "--mcp" | "--system" | "--dynamic-context"))
        || values.insert(flag, value).is_some()
    {
        return Err(CliError::argument("empty or duplicate option"));
    }
    Ok(())
}
