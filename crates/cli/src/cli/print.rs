//! Headless `tars -p` / `tars --print` argument parsing and execution.

use super::command::{failure, output};
use super::configuration::{self, CliError, PreludeCliOptions};
use super::execute::{TransientRunRequest, early_failure_with_format, run_transient};
use super::managed_execution;
use super::output_format::{OutputFormat, OutputFormatBuilder};
use std::collections::HashMap;
use std::ffi::OsString;
use std::io::{self, IsTerminal, Read, Write};
use std::path::PathBuf;
use std::process::ExitCode;
use tars_core::application::{SessionHost, SessionSelectorError};
use tars_core::definition::SessionDefinition;
use tars_core::external_tools::ExternalTools;
use tars_core::permission::{PermissionMode, escape_untrusted_text};

pub(super) const HELP: &str = "Run TARS headlessly for scripting and one-shot prompts

Usage: tars -p [prompt] [options]
       tars -p (-c | --resume <id|prefix|latest>) [prompt] [options]

Input:
  [prompt]            Optional positional prompt (use -- before prompts starting with -)
  --input TEXT        Explicit prompt text (conflicts with positional prompt and --stdin)
  --stdin             Read UTF-8 prompt from stdin until EOF (conflicts with positional prompt)
  --image PATH        Attach a local PNG/JPEG/GIF/WebP image (repeatable, max 8)
  When stdin is piped without --input or --stdin, stdin is read automatically and
  combined with any positional prompt as \"<prompt>\\n\\n<stdin>\". Interactive
  stdio approval prompts are disabled whenever piped stdin is consumed.

Resume options:
  -c, --continue      Continue the most recently updated saved session (same as --resume latest)
  --resume SELECTOR   Continue a saved session by ID, unique prefix, or 'latest'
  --store DIR         Session store directory (defaults to configured or $TARS_HOME/data)
  --trust-workspace-rules   Trust allow rules in <workspace>/.tars/permissions.toml
  --trust-workspace-agents  Trust workspace agent presets in <workspace>/.tars/agents/
  --recovery-dir DIR  Material recovery directory if saving fails

Output format options:
  --json              Output a single structured JSON result object
  --stream            Stream incremental text to stdout and status lines to stderr
  --stream-json       Stream NDJSON progress events and terminal result to stdout
  --output-format FMT text | json | stream-json (conflicts with --json/--stream/--stream-json)

Transient model & tool options (cannot be used with -c / --resume):
  --provider, --model, --profile, --api-key-env, --endpoint, --timeout-secs,
  --max-tokens, --reasoning-effort, --extra-body, --render-options,
  --max-model-attempts, --context-window, --auto-compact, --prompt-cache,
  --code-execution, --no-code-execution, --workspace, --tools,
  --command-network, --mcp, --system, --system-file, --dynamic-context,
  --dynamic-context-file, --skill-dir, --no-skills, --load, --save

Common options:
  --permission-mode   read-only | ask | accept-edits | full-access
  --config FILE       Shared TOML configuration file
  --no-config         Ignore global configuration (requires --store with -c/--resume)
  -h, --help          Show this help
";

/// Returns whether top-level `args` contain `-p` or `--print` before `--`,
/// skipping values consumed by value-taking flags.
pub(super) fn is_print_invocation(args: &[OsString]) -> bool {
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        let Some(token) = arg.to_str() else {
            continue;
        };
        if token == "--" {
            return false;
        }
        if matches!(token, "-p" | "--print") {
            return true;
        }
        if takes_value(token) {
            let _ = iter.next();
        }
    }
    false
}

fn has_help_flag(args: &[OsString]) -> bool {
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        let Some(token) = arg.to_str() else {
            continue;
        };
        if token == "--" {
            return false;
        }
        if matches!(token, "-h" | "--help") {
            return true;
        }
        if takes_value(token) {
            let _ = iter.next();
        }
    }
    false
}

fn takes_value(flag: &str) -> bool {
    matches!(
        flag,
        "--config"
            | "--profile"
            | "--store"
            | "--recovery-dir"
            | "--provider"
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
            | "--dynamic-context-file"
            | "--skill-dir"
            | "--image"
            | "--resume"
            | "--output-format"
    )
}

pub(super) enum PrintCommand {
    Help,
    Transient(Box<TransientPrintOptions>),
    Resume(Box<ResumePrintOptions>),
}

pub(super) struct TransientPrintOptions {
    pub definition: SessionDefinition,
    pub external_tools: ExternalTools,
    pub model_profiles: std::collections::BTreeMap<String, tars_core::agents::ModelProfile>,
    pub user_rules_path: Option<PathBuf>,
    pub positional: Option<String>,
    pub input: Option<String>,
    pub stdin: bool,
    pub images: Vec<PathBuf>,
    pub load: Option<PathBuf>,
    pub save: Option<PathBuf>,
    pub output_format: OutputFormat,
}

pub(super) struct ResumePrintOptions {
    pub store: PathBuf,
    pub selector: String,
    pub external_tools: ExternalTools,
    pub model_profiles: std::collections::BTreeMap<String, tars_core::agents::ModelProfile>,
    pub user_rules_path: Option<PathBuf>,
    pub trust_workspace_rules: bool,
    pub trust_workspace_agents: bool,
    pub positional: Option<String>,
    pub input: Option<String>,
    pub stdin: bool,
    pub images: Vec<PathBuf>,
    pub permission_mode: Option<PermissionMode>,
    pub recovery_dir: PathBuf,
    pub output_format: OutputFormat,
}

impl PrintCommand {
    pub(super) fn parse(args: &[OsString]) -> Result<Self, CliError> {
        if has_help_flag(args) {
            return Ok(Self::Help);
        }
        let mut saw_print: Option<&'static str> = None;
        let mut continue_flag: Option<&'static str> = None;
        let mut resume_selector: Option<String> = None;
        let mut positional_prompt: Option<String> = None;
        let mut after_double_dash = false;

        let mut values: HashMap<&'static str, &OsString> = HashMap::new();
        let mut skill_dirs = Vec::new();
        let mut images = Vec::new();
        let mut no_skills = false;
        let mut code_execution = false;
        let mut no_code_execution = false;
        let mut stdin = false;
        let mut no_config = false;
        let mut trust_workspace_rules = false;
        let mut trust_workspace_agents = false;
        let mut format_builder = OutputFormatBuilder::default();

        let mut transient_only_flags: Vec<&'static str> = Vec::new();
        let mut resume_only_flags: Vec<&'static str> = Vec::new();

        let mut iter = args.iter();
        while let Some(arg) = iter.next() {
            let token = arg.to_str().ok_or("option must be UTF-8")?;
            if after_double_dash {
                record_positional(&mut positional_prompt, token)?;
                continue;
            }
            if token == "--" {
                after_double_dash = true;
                continue;
            }
            match token {
                "-p" | "--print" => {
                    let static_flag = if token == "-p" { "-p" } else { "--print" };
                    if let Some(prev) = saw_print {
                        if prev == static_flag {
                            return Err(CliError::argument(format!(
                                "duplicate option: {static_flag}"
                            )));
                        }
                        return Err(CliError::argument(format!(
                            "{prev} and {static_flag} are mutually exclusive"
                        )));
                    }
                    saw_print = Some(static_flag);
                }
                "-c" | "--continue" => {
                    let static_flag = if token == "-c" { "-c" } else { "--continue" };
                    if let Some(prev) = continue_flag {
                        if prev == static_flag {
                            return Err(CliError::argument(format!(
                                "duplicate option: {static_flag}"
                            )));
                        }
                        return Err(CliError::argument(format!(
                            "{prev} and {static_flag} are mutually exclusive"
                        )));
                    }
                    continue_flag = Some(static_flag);
                }
                "--resume" => {
                    let value = iter
                        .next()
                        .ok_or_else(|| CliError::argument("option --resume requires a value"))?;
                    if resume_selector.is_some() {
                        return Err(CliError::argument("duplicate option: --resume"));
                    }
                    let text = value
                        .to_str()
                        .ok_or_else(|| CliError::argument("text option must be UTF-8"))?;
                    if text.is_empty() {
                        return Err(CliError::argument("option --resume must not be empty"));
                    }
                    resume_selector = Some(text.to_owned());
                }
                "--json" => format_builder.set_flag("--json")?,
                "--stream" => format_builder.set_flag("--stream")?,
                "--stream-json" => format_builder.set_flag("--stream-json")?,
                "--output-format" => format_builder.set_output_format(&mut iter)?,
                "--stdin" => {
                    if stdin {
                        return Err(CliError::argument("duplicate option: --stdin"));
                    }
                    stdin = true;
                }
                "--no-config" => {
                    if no_config {
                        return Err(CliError::argument("duplicate option: --no-config"));
                    }
                    no_config = true;
                }
                "--trust-workspace-rules" => {
                    if trust_workspace_rules {
                        return Err(CliError::argument(
                            "duplicate option: --trust-workspace-rules",
                        ));
                    }
                    trust_workspace_rules = true;
                    resume_only_flags.push("--trust-workspace-rules");
                }
                "--trust-workspace-agents" => {
                    if trust_workspace_agents {
                        return Err(CliError::argument(
                            "duplicate option: --trust-workspace-agents",
                        ));
                    }
                    trust_workspace_agents = true;
                    resume_only_flags.push("--trust-workspace-agents");
                }
                "--no-skills" => {
                    if no_skills {
                        return Err(CliError::argument("duplicate option: --no-skills"));
                    }
                    no_skills = true;
                    transient_only_flags.push("--no-skills");
                }
                "--code-execution" => {
                    if code_execution {
                        return Err(CliError::argument("duplicate option: --code-execution"));
                    }
                    code_execution = true;
                    transient_only_flags.push("--code-execution");
                }
                "--no-code-execution" => {
                    if no_code_execution {
                        return Err(CliError::argument("duplicate option: --no-code-execution"));
                    }
                    no_code_execution = true;
                    transient_only_flags.push("--no-code-execution");
                }
                "--skill-dir" => {
                    let value = iter
                        .next()
                        .ok_or_else(|| CliError::argument("option --skill-dir requires a value"))?;
                    if value.is_empty() {
                        return Err(CliError::argument("option --skill-dir must not be empty"));
                    }
                    skill_dirs.push(PathBuf::from(value));
                    transient_only_flags.push("--skill-dir");
                }
                "--image" => {
                    let value = iter
                        .next()
                        .ok_or_else(|| CliError::argument("option --image requires a value"))?;
                    if value.is_empty() {
                        return Err(CliError::argument("option --image must not be empty"));
                    }
                    images.push(PathBuf::from(value));
                }
                "--config" | "--input" | "--permission-mode" => {
                    insert_value(&mut values, static_common_flag(token), &mut iter)?;
                }
                "--store" | "--recovery-dir" => {
                    let key = static_resume_only_flag(token);
                    insert_value(&mut values, key, &mut iter)?;
                    resume_only_flags.push(key);
                }
                "--provider"
                | "--model"
                | "--profile"
                | "--api-key-env"
                | "--endpoint"
                | "--timeout-secs"
                | "--max-tokens"
                | "--reasoning-effort"
                | "--extra-body"
                | "--render-options"
                | "--max-model-attempts"
                | "--context-window"
                | "--auto-compact"
                | "--prompt-cache"
                | "--workspace"
                | "--tools"
                | "--command-network"
                | "--mcp"
                | "--system"
                | "--system-file"
                | "--dynamic-context"
                | "--dynamic-context-file"
                | "--load"
                | "--save" => {
                    let key = static_transient_only_flag(token);
                    insert_value(&mut values, key, &mut iter)?;
                    transient_only_flags.push(key);
                }
                other if !other.starts_with('-') || other == "-" => {
                    record_positional(&mut positional_prompt, other)?;
                }
                other => {
                    return Err(CliError::argument(format!(
                        "unknown option: {}",
                        escape_untrusted_text(other)
                    )));
                }
            }
        }

        if let (Some(c_flag), Some(_)) = (continue_flag, &resume_selector) {
            return Err(CliError::argument(format!(
                "{c_flag} and --resume are mutually exclusive"
            )));
        }

        let is_resume = continue_flag.is_some() || resume_selector.is_some();
        if is_resume {
            if let Some(flag) = transient_only_flags.first() {
                return Err(CliError::argument(format!(
                    "option {flag} cannot be used with --continue/--resume"
                )));
            }
        } else if let Some(flag) = resume_only_flags.first() {
            return Err(CliError::argument(format!(
                "option {flag} can only be used with --continue or --resume"
            )));
        }

        let input = configuration::string(&values, "--input")?;
        if positional_prompt.is_some() && input.is_some() {
            return Err(CliError::argument(
                "positional prompt and --input are mutually exclusive",
            ));
        }
        if positional_prompt.is_some() && stdin {
            return Err(CliError::argument(
                "positional prompt and --stdin are mutually exclusive",
            ));
        }
        if input.is_some() && stdin {
            return Err(CliError::argument(
                "choose exactly one of --input or --stdin",
            ));
        }
        if images.is_empty() && input.as_deref().is_some_and(|v| v.trim().is_empty()) {
            return Err(CliError::argument("input must not be blank"));
        }

        let config = configuration::load(&values, no_config)?;
        let external_tools = ExternalTools::resolve(&config);
        let model_profiles = config.model_profiles();
        let user_rules_path = config.config_dir().map(|dir| dir.join("permissions.toml"));
        let output_format = format_builder.build();

        if is_resume {
            let selector = match resume_selector {
                Some(selector) => selector,
                None => "latest".to_owned(),
            };
            let store = values
                .get("--store")
                .map(PathBuf::from)
                .or_else(|| config.store_dir().map(PathBuf::from))
                .or_else(|| {
                    (!no_config)
                        .then(tars_core::config::default_store_dir)
                        .flatten()
                });
            let store = match store {
                Some(dir) => dir,
                None if no_config => {
                    return Err(CliError::argument("--store is required with --no-config"));
                }
                None => {
                    return Err(CliError::argument(
                        "--store or a configured store directory is required",
                    ));
                }
            };
            let permission_mode = configuration::string(&values, "--permission-mode")?
                .map(|value| {
                    value.parse::<PermissionMode>().map_err(|_| {
                        CliError::argument(
                            "--permission-mode must be read-only, ask, accept-edits, or full-access",
                        )
                    })
                })
                .transpose()?;
            let recovery_dir = values
                .get("--recovery-dir")
                .map(PathBuf::from)
                .or_else(|| config.recovery_dir().map(PathBuf::from))
                .unwrap_or_else(|| PathBuf::from("."));

            return Ok(Self::Resume(Box::new(ResumePrintOptions {
                store,
                selector,
                external_tools,
                model_profiles,
                user_rules_path,
                trust_workspace_rules,
                trust_workspace_agents,
                positional: positional_prompt,
                input,
                stdin,
                images,
                permission_mode,
                recovery_dir,
                output_format,
            })));
        }

        let code_execution_enabled =
            configuration::resolve_code_execution(code_execution, no_code_execution)?;
        let mut definition = configuration::definition(
            &config,
            &values,
            PreludeCliOptions {
                skill_dirs,
                no_skills,
            },
            code_execution_enabled,
        )?;
        definition.retained_work_capacity = 17;

        Ok(Self::Transient(Box::new(TransientPrintOptions {
            definition,
            external_tools,
            model_profiles,
            user_rules_path,
            positional: positional_prompt,
            input,
            stdin,
            images,
            load: values.get("--load").map(PathBuf::from),
            save: values.get("--save").map(PathBuf::from),
            output_format,
        })))
    }
}

fn record_positional(slot: &mut Option<String>, token: &str) -> Result<(), CliError> {
    if slot.is_some() {
        return Err(CliError::argument(format!(
            "unexpected positional argument: {}",
            escape_untrusted_text(token)
        )));
    }
    *slot = Some(token.to_owned());
    Ok(())
}

fn insert_value<'a>(
    values: &mut HashMap<&'static str, &'a OsString>,
    flag: &'static str,
    args: &mut std::slice::Iter<'a, OsString>,
) -> Result<(), CliError> {
    let value = args
        .next()
        .ok_or_else(|| CliError::argument(format!("option {flag} requires a value")))?;
    if values.contains_key(flag) {
        return Err(CliError::argument(format!("duplicate option: {flag}")));
    }
    if value.is_empty() && !matches!(flag, "--tools" | "--mcp" | "--system" | "--dynamic-context") {
        return Err(CliError::argument(format!(
            "option {flag} must not be empty"
        )));
    }
    values.insert(flag, value);
    Ok(())
}

fn static_common_flag(token: &str) -> &'static str {
    match token {
        "--config" => "--config",
        "--input" => "--input",
        "--permission-mode" => "--permission-mode",
        _ => unreachable!("unexpected common flag {token}"),
    }
}

fn static_resume_only_flag(token: &str) -> &'static str {
    match token {
        "--store" => "--store",
        "--recovery-dir" => "--recovery-dir",
        _ => unreachable!("unexpected resume-only flag {token}"),
    }
}

fn static_transient_only_flag(token: &str) -> &'static str {
    match token {
        "--provider" => "--provider",
        "--model" => "--model",
        "--profile" => "--profile",
        "--api-key-env" => "--api-key-env",
        "--endpoint" => "--endpoint",
        "--timeout-secs" => "--timeout-secs",
        "--max-tokens" => "--max-tokens",
        "--reasoning-effort" => "--reasoning-effort",
        "--extra-body" => "--extra-body",
        "--render-options" => "--render-options",
        "--max-model-attempts" => "--max-model-attempts",
        "--context-window" => "--context-window",
        "--auto-compact" => "--auto-compact",
        "--prompt-cache" => "--prompt-cache",
        "--workspace" => "--workspace",
        "--tools" => "--tools",
        "--command-network" => "--command-network",
        "--mcp" => "--mcp",
        "--system" => "--system",
        "--system-file" => "--system-file",
        "--dynamic-context" => "--dynamic-context",
        "--dynamic-context-file" => "--dynamic-context-file",
        "--load" => "--load",
        "--save" => "--save",
        _ => unreachable!("unexpected transient-only flag {token}"),
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(super) struct PreparedPrintInput {
    pub input: Option<String>,
    pub used_stdin_input: bool,
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum PrintInputError {
    Argument(&'static str),
    Io(&'static str),
}

pub(super) fn prepare_print_input(
    positional: Option<String>,
    input_flag: Option<String>,
    stdin_flag: bool,
    has_images: bool,
    stdin_is_terminal: bool,
    reader: &mut dyn Read,
) -> Result<PreparedPrintInput, PrintInputError> {
    if stdin_flag {
        let mut text = String::new();
        reader
            .read_to_string(&mut text)
            .map_err(|_| PrintInputError::Io("cannot read UTF-8 stdin"))?;
        if text.trim().is_empty() {
            if has_images {
                return Ok(PreparedPrintInput {
                    input: None,
                    used_stdin_input: true,
                });
            }
            return Err(PrintInputError::Argument("input must not be blank"));
        }
        return Ok(PreparedPrintInput {
            input: Some(text),
            used_stdin_input: true,
        });
    }

    if let Some(text) = input_flag {
        if text.trim().is_empty() {
            if has_images {
                return Ok(PreparedPrintInput {
                    input: None,
                    used_stdin_input: false,
                });
            }
            return Err(PrintInputError::Argument("input must not be blank"));
        }
        return Ok(PreparedPrintInput {
            input: Some(text),
            used_stdin_input: false,
        });
    }

    if !stdin_is_terminal {
        let mut stdin_text = String::new();
        reader
            .read_to_string(&mut stdin_text)
            .map_err(|_| PrintInputError::Io("cannot read UTF-8 stdin"))?;
        let stdin_blank = stdin_text.trim().is_empty();
        return match (positional, stdin_blank) {
            (Some(p), true) => {
                if p.trim().is_empty() {
                    if has_images {
                        Ok(PreparedPrintInput {
                            input: None,
                            used_stdin_input: true,
                        })
                    } else {
                        Err(PrintInputError::Argument("input must not be blank"))
                    }
                } else {
                    Ok(PreparedPrintInput {
                        input: Some(p),
                        used_stdin_input: true,
                    })
                }
            }
            (Some(p), false) => {
                let combined = if p.trim().is_empty() {
                    stdin_text
                } else {
                    format!("{p}\n\n{stdin_text}")
                };
                Ok(PreparedPrintInput {
                    input: Some(combined),
                    used_stdin_input: true,
                })
            }
            (None, true) => {
                if has_images {
                    Ok(PreparedPrintInput {
                        input: None,
                        used_stdin_input: true,
                    })
                } else {
                    Err(PrintInputError::Argument("input must not be blank"))
                }
            }
            (None, false) => Ok(PreparedPrintInput {
                input: Some(stdin_text),
                used_stdin_input: true,
            }),
        };
    }

    match positional {
        Some(p) if !p.trim().is_empty() => Ok(PreparedPrintInput {
            input: Some(p),
            used_stdin_input: false,
        }),
        Some(_) | None if has_images => Ok(PreparedPrintInput {
            input: None,
            used_stdin_input: false,
        }),
        Some(_) => Err(PrintInputError::Argument("input must not be blank")),
        None => Err(PrintInputError::Argument(
            "provide a prompt argument, --input, --stdin, or --image",
        )),
    }
}

pub(super) fn run(command: PrintCommand) -> ExitCode {
    match command {
        PrintCommand::Help => output(HELP),
        PrintCommand::Transient(opts) => {
            let has_images = !opts.images.is_empty();
            let stdin = io::stdin();
            let stdin_is_terminal = stdin.is_terminal();
            let mut lock = stdin.lock();
            let prepared_input = match prepare_print_input(
                opts.positional,
                opts.input,
                opts.stdin,
                has_images,
                stdin_is_terminal,
                &mut lock,
            ) {
                Ok(prepared) => prepared,
                Err(PrintInputError::Argument(msg)) => {
                    let _ = writeln!(io::stderr().lock(), "TARS: {msg}. Use tars -p --help.");
                    return ExitCode::from(2);
                }
                Err(PrintInputError::Io(msg)) => {
                    return early_failure_with_format(opts.output_format, msg);
                }
            };
            drop(lock);
            run_transient(TransientRunRequest {
                definition: opts.definition,
                external_tools: opts.external_tools,
                model_profiles: opts.model_profiles,
                user_rules_path: opts.user_rules_path,
                input: prepared_input.input,
                used_stdin_input: prepared_input.used_stdin_input,
                images: opts.images,
                load: opts.load,
                save: opts.save,
                output_format: opts.output_format,
            })
        }
        PrintCommand::Resume(opts) => run_resume(*opts),
    }
}

fn run_resume(opts: ResumePrintOptions) -> ExitCode {
    let prepared_images = match managed_execution::prepare_images(&opts.images) {
        Ok(images) => images,
        Err(error) => return early_failure_with_format(opts.output_format, &error),
    };
    let stdin = io::stdin();
    let stdin_is_terminal = stdin.is_terminal();
    let mut lock = stdin.lock();
    let prepared_input = match prepare_print_input(
        opts.positional,
        opts.input,
        opts.stdin,
        !prepared_images.is_empty(),
        stdin_is_terminal,
        &mut lock,
    ) {
        Ok(prepared) => prepared,
        Err(PrintInputError::Argument(msg)) => {
            let _ = writeln!(io::stderr().lock(), "TARS: {msg}. Use tars -p --help.");
            return ExitCode::from(2);
        }
        Err(PrintInputError::Io(msg)) => {
            return early_failure_with_format(opts.output_format, msg);
        }
    };
    drop(lock);

    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => return failure(error),
    };
    let (code, abandoned) = runtime.block_on(async move {
        let host = SessionHost::new(Some(opts.store.clone()));
        let conversation_id = match host.resolve_session_selector(&opts.selector).await {
            Ok(id) => id,
            Err(SessionSelectorError::EmptySelector) => {
                let _ = writeln!(
                    io::stderr().lock(),
                    "TARS: session selector must not be empty. Use tars -p --help."
                );
                return (ExitCode::from(2), false);
            }
            Err(error) => {
                return (
                    early_failure_with_format(opts.output_format, &error.to_string()),
                    false,
                );
            }
        };
        super::session::run_saved_with_format(
            opts.store,
            conversation_id.as_str().to_owned(),
            opts.external_tools,
            opts.model_profiles,
            opts.user_rules_path,
            opts.trust_workspace_rules,
            opts.trust_workspace_agents,
            prepared_input.input,
            prepared_input.used_stdin_input,
            prepared_images,
            opts.permission_mode,
            opts.recovery_dir,
            opts.output_format,
        )
        .await
    });
    if abandoned {
        runtime.shutdown_background();
    }
    code
}

#[cfg(test)]
mod tests {
    use super::{
        PreparedPrintInput, PrintCommand, PrintInputError, is_print_invocation, prepare_print_input,
    };
    use std::ffi::OsString;
    use std::io::Cursor;

    fn os(args: &[&str]) -> Vec<OsString> {
        args.iter().map(OsString::from).collect()
    }

    #[test]
    fn print_routing_respects_value_flags_and_double_dash() {
        assert!(is_print_invocation(&os(&["-p", "hi"])));
        assert!(is_print_invocation(&os(&["--profile", "fast", "-p", "hi"])));
        assert!(is_print_invocation(&os(&["--print", "--help"])));
        assert!(is_print_invocation(&os(&["-p", "--", "-leading"])));

        // `-p` consumed as a flag value or after `--` does not trigger print mode.
        assert!(!is_print_invocation(&os(&["demo", "--save", "-p"])));
        assert!(!is_print_invocation(&os(&["--config", "-p"])));
        assert!(!is_print_invocation(&os(&["--", "-p"])));
        assert!(!is_print_invocation(&os(&["--profile", "fast"])));
    }

    #[test]
    fn print_parser_handles_transient_resume_double_dash_and_rejections() {
        // Double-dash allows leading-dash positional prompt.
        let cmd = PrintCommand::parse(&os(&[
            "--no-config",
            "--provider",
            "openai-chat",
            "--model",
            "m",
            "-p",
            "--",
            "-leading-dash",
        ]))
        .unwrap();
        match cmd {
            PrintCommand::Transient(opts) => {
                assert_eq!(opts.positional.as_deref(), Some("-leading-dash"));
            }
            _ => panic!("expected transient"),
        }

        // `-p -- --help` treats `--help` as positional prompt, not help flag.
        let cmd = PrintCommand::parse(&os(&[
            "--no-config",
            "--provider",
            "openai-chat",
            "--model",
            "m",
            "-p",
            "--",
            "--help",
        ]))
        .unwrap();
        assert!(matches!(cmd, PrintCommand::Transient(_)));

        // `-p --help` returns Help.
        assert!(matches!(
            PrintCommand::parse(&os(&["-p", "--help"])).unwrap(),
            PrintCommand::Help
        ));

        // Counterexamples (exit 2):
        for (args, expected) in [
            (vec!["-p", "a", "b"], "unexpected positional argument: b"),
            (
                vec!["-p", "a", "--input", "b"],
                "positional prompt and --input are mutually exclusive",
            ),
            (
                vec!["-p", "a", "--stdin"],
                "positional prompt and --stdin are mutually exclusive",
            ),
            (
                vec!["-p", "-c", "--resume", "abc"],
                "-c and --resume are mutually exclusive",
            ),
            (
                vec!["-p", "--no-config", "-c"],
                "--store is required with --no-config",
            ),
            (
                vec!["-p", "--store", "/tmp/s", "hi"],
                "option --store can only be used with --continue or --resume",
            ),
            (
                vec!["-p", "--trust-workspace-rules", "hi"],
                "option --trust-workspace-rules can only be used with --continue or --resume",
            ),
            (
                vec!["-p", "-c", "--store", "/tmp/s", "--provider", "openai-chat"],
                "option --provider cannot be used with --continue/--resume",
            ),
            (
                vec![
                    "-p",
                    "--resume",
                    "abc",
                    "--store",
                    "/tmp/s",
                    "--workspace",
                    ".",
                ],
                "option --workspace cannot be used with --continue/--resume",
            ),
            (vec!["-p", "-p"], "duplicate option: -p"),
            (vec!["-p", "--bogus"], "unknown option: --bogus"),
        ] {
            let err = PrintCommand::parse(&os(&args)).err().expect("must fail");
            assert!(err.is_argument());
            assert!(
                err.message().contains(expected),
                "args={args:?}: expected '{expected}' in '{}'",
                err.message()
            );
        }
    }

    #[test]
    fn prepare_print_input_combines_piped_stdin_and_rejects_blank() {
        // Piped stdin without positional -> uses stdin, disables stdio approvals.
        let mut r = Cursor::new(b"hello\n");
        assert_eq!(
            prepare_print_input(None, None, false, false, false, &mut r),
            Ok(PreparedPrintInput {
                input: Some("hello\n".into()),
                used_stdin_input: true,
            })
        );

        // Piped stdin + positional -> "<prompt>\n\n<stdin>".
        let mut r = Cursor::new(b"diff");
        assert_eq!(
            prepare_print_input(Some("review".into()), None, false, false, false, &mut r),
            Ok(PreparedPrintInput {
                input: Some("review\n\ndiff".into()),
                used_stdin_input: true,
            })
        );

        // Blank piped stdin + positional -> positional only, still marks used_stdin_input = true.
        let mut r = Cursor::new(b"   \n");
        assert_eq!(
            prepare_print_input(Some("review".into()), None, false, false, false, &mut r),
            Ok(PreparedPrintInput {
                input: Some("review".into()),
                used_stdin_input: true,
            })
        );

        // Empty piped stdin + no positional + no image -> Argument error (exit 2).
        let mut r = Cursor::new(b"   ");
        assert_eq!(
            prepare_print_input(None, None, false, false, false, &mut r),
            Err(PrintInputError::Argument("input must not be blank"))
        );

        // Empty piped stdin + image -> allowed.
        let mut r = Cursor::new(b"");
        assert_eq!(
            prepare_print_input(None, None, false, true, false, &mut r),
            Ok(PreparedPrintInput {
                input: None,
                used_stdin_input: true,
            })
        );

        // TTY with nothing -> Argument error (exit 2).
        let mut r = Cursor::new(b"");
        assert_eq!(
            prepare_print_input(None, None, false, false, true, &mut r),
            Err(PrintInputError::Argument(
                "provide a prompt argument, --input, --stdin, or --image"
            ))
        );

        // Non-UTF-8 piped stdin -> Io error (exit 1).
        let mut r = Cursor::new([0xff, 0xfe]);
        assert_eq!(
            prepare_print_input(None, None, false, false, false, &mut r),
            Err(PrintInputError::Io("cannot read UTF-8 stdin"))
        );
    }
}
