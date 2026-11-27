use crate::cli::configuration::{self, CliError, PreludeCliOptions};
use crate::cli::output_format::{OutputFormat, OutputFormatBuilder};
use std::collections::HashMap;
use std::ffi::OsString;
use std::path::PathBuf;
use tars_core::definition::SessionDefinition;
use tars_core::external_tools::ExternalTools;
use tars_core::permission::escape_untrusted_text;

pub(crate) enum ExportMaterialsTarget {
    Record(PathBuf),
    Session { store: PathBuf, id: String },
}

pub(crate) enum SessionCommand {
    ExportMaterials {
        target: ExportMaterialsTarget,
        output: PathBuf,
        json: bool,
    },
    ExportMarkdown {
        store: PathBuf,
        id: String,
        agent: Option<String>,
        last: Option<usize>,
        output: Option<PathBuf>,
        json: bool,
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
        stdin: bool,
        images: Vec<PathBuf>,
        permission_mode: Option<tars_core::permission::PermissionMode>,
        recovery_dir: PathBuf,
        output_format: OutputFormat,
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
    Rename {
        store: PathBuf,
        id: String,
        title: Option<String>,
        json: bool,
    },
    Delete {
        store: PathBuf,
        id: String,
        json: bool,
    },
    Fork {
        store: PathBuf,
        id: String,
        title: Option<String>,
        json: bool,
    },
}

fn is_known_session_option(flag: &str) -> bool {
    matches!(
        flag,
        "--json"
            | "--stream"
            | "--stream-json"
            | "--output-format"
            | "--no-config"
            | "--clear"
            | "--trust-workspace-rules"
            | "--trust-workspace-agents"
            | "--no-skills"
            | "--code-execution"
            | "--no-code-execution"
            | "--stdin"
            | "--apply"
            | "--dry-run"
            | "--store"
            | "--config"
            | "--skill-dir"
            | "--provider"
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
            | "--input"
            | "--image"
            | "--last"
            | "--agent"
            | "--title"
            | "--permission-mode"
            | "--expected-revision"
            | "--record"
            | "--recovery-dir"
            | "--output"
    )
}

impl SessionCommand {
    pub(crate) fn parse(args: &[OsString]) -> Result<Self, CliError> {
        let (command, mut args) = args.split_first().ok_or("session command is required")?;
        let command = command.to_str().ok_or("session command must be UTF-8")?;
        if command == "export-materials" {
            return parse_export_materials(args);
        }
        let id = if matches!(
            command,
            "run"
                | "repair"
                | "compact"
                | "history"
                | "show"
                | "rename"
                | "delete"
                | "fork"
                | "export-markdown"
        ) {
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
            "create"
                | "run"
                | "list"
                | "repair"
                | "compact"
                | "history"
                | "show"
                | "rename"
                | "delete"
                | "fork"
                | "export-markdown"
                | "import-legacy"
        ) {
            return Err(CliError::argument(format!(
                "unknown session command: {}",
                escape_untrusted_text(command)
            )));
        }
        let mut values = HashMap::new();
        let mut skill_dirs = Vec::new();
        let mut images = Vec::new();
        let mut no_skills = false;
        let mut code_execution = false;
        let mut no_code_execution = false;
        let mut stdin = false;
        let mut json = false;
        let mut format_builder = OutputFormatBuilder::default();
        let mut no_config = false;
        let mut trust_workspace_rules = false;
        let mut trust_workspace_agents = false;
        let mut apply = false;
        let mut dry_run = false;
        let mut clear = false;
        let mut args = args.iter();
        while let Some(flag) = args.next() {
            let flag = flag.to_str().ok_or("option must be UTF-8")?;
            match flag {
                "--json" if command == "run" => format_builder.set_flag("--json")?,
                "--stream" if command == "run" => format_builder.set_flag("--stream")?,
                "--stream-json" if command == "run" => format_builder.set_flag("--stream-json")?,
                "--output-format" if command == "run" => {
                    format_builder.set_output_format(&mut args)?;
                }
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
                "--clear" if command == "rename" => {
                    if clear {
                        return Err(CliError::argument("duplicate option: --clear"));
                    }
                    clear = true;
                }
                "--trust-workspace-rules" if matches!(command, "create" | "run") => {
                    if trust_workspace_rules {
                        return Err(CliError::argument(
                            "duplicate option: --trust-workspace-rules",
                        ));
                    }
                    trust_workspace_rules = true;
                }
                "--trust-workspace-agents" if matches!(command, "create" | "run") => {
                    if trust_workspace_agents {
                        return Err(CliError::argument(
                            "duplicate option: --trust-workspace-agents",
                        ));
                    }
                    trust_workspace_agents = true;
                }
                "--no-skills" if command == "create" => {
                    if no_skills {
                        return Err(CliError::argument("duplicate option: --no-skills"));
                    }
                    no_skills = true;
                }
                "--code-execution" if command == "create" => {
                    if code_execution {
                        return Err(CliError::argument("duplicate option: --code-execution"));
                    }
                    code_execution = true;
                }
                "--no-code-execution" if command == "create" => {
                    if no_code_execution {
                        return Err(CliError::argument("duplicate option: --no-code-execution"));
                    }
                    no_code_execution = true;
                }
                "--stdin" if command == "run" => {
                    if stdin {
                        return Err(CliError::argument("duplicate option: --stdin"));
                    }
                    stdin = true;
                }
                "--apply" if command == "repair" => {
                    if apply {
                        return Err(CliError::argument("duplicate option: --apply"));
                    }
                    apply = true;
                }
                "--dry-run" if command == "repair" => {
                    if dry_run {
                        return Err(CliError::argument("duplicate option: --dry-run"));
                    }
                    dry_run = true;
                }
                "--store" | "--config" => {
                    insert(&mut values, flag, &mut args)?;
                }
                "--skill-dir" if command == "create" => {
                    let value = args
                        .next()
                        .ok_or_else(|| CliError::argument("option --skill-dir requires a value"))?;
                    if value.is_empty() {
                        return Err(CliError::argument("option --skill-dir must not be empty"));
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
                "--image" if command == "run" => {
                    let value = args
                        .next()
                        .ok_or_else(|| CliError::argument("option --image requires a value"))?;
                    if value.is_empty() {
                        return Err(CliError::argument("option --image must not be empty"));
                    }
                    images.push(PathBuf::from(value));
                }
                "--last" if matches!(command, "history" | "export-markdown") => {
                    insert(&mut values, flag, &mut args)?;
                }
                "--agent" if matches!(command, "show" | "history" | "export-markdown") => {
                    insert(&mut values, flag, &mut args)?;
                }
                "--output" if command == "export-markdown" => {
                    insert(&mut values, flag, &mut args)?;
                }
                "--title" if matches!(command, "rename" | "fork") => {
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
                other if is_known_session_option(other) => {
                    return Err(CliError::argument(format!(
                        "option {other} is not applicable to session {command}"
                    )));
                }
                other => {
                    return Err(CliError::argument(format!(
                        "unknown session option: {}",
                        escape_untrusted_text(other)
                    )));
                }
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
                crate::cli::managed_execution::validate_prompt_options(
                    input.as_deref(),
                    stdin,
                    !images.is_empty(),
                )?;
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
                    stdin,
                    images,
                    permission_mode,
                    recovery_dir: values
                        .get("--recovery-dir")
                        .map(PathBuf::from)
                        .or_else(|| config.recovery_dir().map(PathBuf::from))
                        .unwrap_or_else(|| PathBuf::from(".")),
                    output_format: format_builder.build(),
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
            "export-markdown" => {
                let last = string("--last")?
                    .map(|value| {
                        value
                            .parse::<usize>()
                            .ok()
                            .filter(|count| *count > 0)
                            .ok_or("--last must be a positive integer")
                    })
                    .transpose()?;
                Ok(Self::ExportMarkdown {
                    store,
                    id: id.expect("export-markdown ID parsed"),
                    agent: string("--agent")?,
                    last,
                    output: values.get("--output").map(PathBuf::from),
                    json,
                })
            }
            "show" => Ok(Self::Show {
                store,
                id: id.expect("show ID parsed"),
                agent: string("--agent")?,
                json,
            }),
            "rename" => {
                let title = string("--title")?;
                if clear == title.is_some() {
                    return Err("choose exactly one of --title or --clear".into());
                }
                Ok(Self::Rename {
                    store,
                    id: id.expect("rename ID parsed"),
                    title,
                    json,
                })
            }
            "delete" => Ok(Self::Delete {
                store,
                id: id.expect("delete ID parsed"),
                json,
            }),
            "fork" => Ok(Self::Fork {
                store,
                id: id.expect("fork ID parsed"),
                title: string("--title")?,
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
        .ok_or_else(|| CliError::argument(format!("option {flag} requires a value")))?;
    if values.contains_key(flag) {
        return Err(CliError::argument(format!("duplicate option: {flag}")));
    }
    if value.is_empty()
        && !matches!(
            flag,
            "--tools" | "--mcp" | "--system" | "--dynamic-context" | "--title"
        )
    {
        return Err(CliError::argument(format!(
            "option {flag} must not be empty"
        )));
    }
    values.insert(flag, value);
    Ok(())
}

fn parse_export_materials(args: &[OsString]) -> Result<SessionCommand, CliError> {
    let is_session_mode = args
        .first()
        .and_then(|value| value.to_str())
        .is_some_and(|value| !value.starts_with('-'));
    if !is_session_mode {
        let mut values = HashMap::new();
        let mut json = false;
        let mut options = args.iter();
        while let Some(flag) = options.next() {
            let flag = flag.to_str().ok_or("option must be UTF-8")?;
            match flag {
                "--json" => {
                    if json {
                        return Err(CliError::argument("duplicate option: --json"));
                    }
                    json = true;
                }
                "--record" | "--output" => insert(&mut values, flag, &mut options)?,
                other if is_known_session_option(other) => {
                    return Err(CliError::argument(format!(
                        "option {other} is not applicable to session export-materials"
                    )));
                }
                other => {
                    return Err(CliError::argument(format!(
                        "unknown session option: {}",
                        escape_untrusted_text(other)
                    )));
                }
            }
        }
        return Ok(SessionCommand::ExportMaterials {
            target: ExportMaterialsTarget::Record(
                values
                    .get("--record")
                    .map(PathBuf::from)
                    .ok_or("--record is required")?,
            ),
            output: values
                .get("--output")
                .map(PathBuf::from)
                .ok_or("--output is required")?,
            json,
        });
    }

    let (id, rest) = args.split_first().expect("checked first arg");
    let id = id
        .to_str()
        .filter(|value| !value.is_empty())
        .ok_or("conversation ID must be non-empty UTF-8")?
        .to_owned();
    let mut values = HashMap::new();
    let mut json = false;
    let mut no_config = false;
    let mut positional_output: Option<PathBuf> = None;
    let mut options = rest.iter();
    while let Some(arg) = options.next() {
        let token = arg.to_str().ok_or("option must be UTF-8")?;
        match token {
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
            "--store" | "--config" | "--output" => insert(&mut values, token, &mut options)?,
            "--record" => return Err("--record cannot be combined with a conversation ID".into()),
            other if !other.starts_with('-') => {
                if positional_output.is_some() {
                    return Err(CliError::argument(format!(
                        "unexpected positional argument: {}",
                        escape_untrusted_text(other)
                    )));
                }
                if other.is_empty() {
                    return Err(CliError::argument("output path must not be empty"));
                }
                positional_output = Some(PathBuf::from(arg));
            }
            other if is_known_session_option(other) => {
                return Err(CliError::argument(format!(
                    "option {other} is not applicable to session export-materials"
                )));
            }
            other => {
                return Err(CliError::argument(format!(
                    "unknown session option: {}",
                    escape_untrusted_text(other)
                )));
            }
        }
    }
    let output = match (values.get("--output").map(PathBuf::from), positional_output) {
        (Some(_), Some(_)) => return Err("choose either positional output or --output".into()),
        (Some(path), None) | (None, Some(path)) => path,
        (None, None) => return Err("--output or output path is required".into()),
    };
    let config = configuration::load(&values, no_config)?;
    let store = values
        .get("--store")
        .map(PathBuf::from)
        .or_else(|| config.store_dir().map(PathBuf::from))
        .ok_or("--store or a configured store directory is required")?;
    Ok(SessionCommand::ExportMaterials {
        target: ExportMaterialsTarget::Session { store, id },
        output,
        json,
    })
}
