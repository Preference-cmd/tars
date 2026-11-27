use super::configuration::{self, CliError, PreludeCliOptions};
use super::output_format::{OutputFormat, OutputFormatBuilder};
use std::collections::HashMap;
use std::ffi::OsString;
use std::path::PathBuf;
use tars_core::definition::SessionDefinition;
use tars_core::external_tools::ExternalTools;
use tars_core::permission::escape_untrusted_text;

pub(super) const HELP: &str = "Execute one transient work with a real model

Usage: tars run --provider <protocol> --model <id> (--input <text> | --stdin | --image <path>) [options]

Options:
  --provider          openai-chat | openai-responses | anthropic (or configured profile)
  --model             Model identifier (or configured profile)
  --input             User text; mutually exclusive with --stdin
  --stdin             Read UTF-8 user text until EOF
  --image             Attach a local PNG/JPEG/GIF/WebP image file (repeatable, max 8;
                      text may be omitted when at least one --image is provided)
  --api-key-env       Credential variable (default OPENAI_API_KEY / ANTHROPIC_API_KEY)
  --endpoint          Full HTTP endpoint override, including the path
  --timeout-secs      Work deadline, 1–86400 seconds (default 120)
  --max-tokens        Output token limit per model call (no default; required by anthropic)
  --reasoning-effort  Effort text sent as written, e.g. low, high (default: provider's)
  --extra-body        JSON object of further request fields, sent as written
  --render-options    JSON object of the protocol's render options, e.g. max_tokens_field
  --max-model-attempts  Model calls per round on transient or rate-limit failure (default 1)
  --context-window    Model context window in tokens; omitted leaves it unknown and
                      disables automatic compaction
  --auto-compact      on | off | PERCENT: automatic compaction switch, or a trigger
                      percentage in 10–95 that also enables it (default on, 70)
  --prompt-cache      on | off: prompt caching switch (default on)
  --code-execution    Enable sandboxed read-only run_code (conflicts with --no-code-execution)
  --no-code-execution Disable run_code even when enabled in configuration
  --workspace         Root directory for workspace tools; relative to current directory
  --tools             Comma-separated list_directory,read_file,search_text,find_files,write_file,edit_file,run_command (default empty)
  --command-network   Command network: none or open (default none)
  --mcp               Comma-separated ids from mcp.json (default: profile; empty disables)
  --permission-mode   read-only | ask | accept-edits | full-access (default: profile or ask)
  --system            Inline static custom system prompt (conflicts with --system-file)
  --system-file       UTF-8 static custom system prompt file, relative to cwd
  --dynamic-context       Inline dynamic custom context (conflicts with --dynamic-context-file)
  --dynamic-context-file  UTF-8 dynamic custom context file, relative to cwd
  --skill-dir         Additional skill directory, relative to cwd (repeatable)
  --no-skills         Disable skills discovery, <available_skills> injection, and load_skill
                      (conflicts with --skill-dir)
  --load              Import a Context material file into a new session
  --save              Replace a material file after completion or controlled interruption
  --json              Output a structured JSON result object
  --stream            Stream incremental response text to stdout and status to stderr
  --stream-json       Stream NDJSON progress events and terminal result to stdout
  --output-format     text | json | stream-json (conflicts with --json/--stream/--stream-json)
  --config            Shared TOML configuration file
  --profile           Named model preset (otherwise default_profile)
  --no-config         Ignore global configuration; conflicts with --config
  -h, --help          Show this help
";

pub(super) struct RunOptions {
    pub definition: SessionDefinition,
    /// Resolved once from the loaded configuration and handed to the host.
    pub external_tools: ExternalTools,
    pub model_profiles: std::collections::BTreeMap<String, tars_core::agents::ModelProfile>,
    pub user_rules_path: Option<PathBuf>,
    pub credentials_path: Option<PathBuf>,
    pub input: Option<String>,
    pub stdin: bool,
    pub images: Vec<PathBuf>,
    pub load: Option<PathBuf>,
    pub save: Option<PathBuf>,
    pub output_format: OutputFormat,
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
        let mut format_builder = OutputFormatBuilder::default();
        let mut no_config = false;
        let mut args = args.iter();
        while let Some(flag) = args.next() {
            let flag = flag.to_str().ok_or("option must be UTF-8")?;
            match flag {
                "--stdin" => {
                    if stdin {
                        return Err(CliError::argument("duplicate option: --stdin"));
                    }
                    stdin = true;
                }
                "--json" | "--stream" | "--stream-json" => {
                    format_builder.set_flag(flag)?;
                }
                "--output-format" => {
                    format_builder.set_output_format(&mut args)?;
                }
                "--no-config" => {
                    if no_config {
                        return Err(CliError::argument("duplicate option: --no-config"));
                    }
                    no_config = true;
                }
                "--no-skills" => {
                    if no_skills {
                        return Err(CliError::argument("duplicate option: --no-skills"));
                    }
                    no_skills = true;
                }
                "--code-execution" => {
                    if code_execution {
                        return Err(CliError::argument("duplicate option: --code-execution"));
                    }
                    code_execution = true;
                }
                "--no-code-execution" => {
                    if no_code_execution {
                        return Err(CliError::argument("duplicate option: --no-code-execution"));
                    }
                    no_code_execution = true;
                }
                "--skill-dir" => {
                    let value = args
                        .next()
                        .ok_or_else(|| CliError::argument("option --skill-dir requires a value"))?;
                    if value.is_empty() {
                        return Err(CliError::argument("option --skill-dir must not be empty"));
                    }
                    skill_dirs.push(PathBuf::from(value));
                }
                "--image" => {
                    let value = args
                        .next()
                        .ok_or_else(|| CliError::argument("option --image requires a value"))?;
                    if value.is_empty() {
                        return Err(CliError::argument("option --image must not be empty"));
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
                    let value = args.next().ok_or_else(|| {
                        CliError::argument(format!("option {flag} requires a value"))
                    })?;
                    if values.contains_key(flag) {
                        return Err(CliError::argument(format!("duplicate option: {flag}")));
                    }
                    if value.is_empty()
                        && !matches!(flag, "--tools" | "--mcp" | "--system" | "--dynamic-context")
                    {
                        return Err(CliError::argument(format!(
                            "option {flag} must not be empty"
                        )));
                    }
                    values.insert(flag, value);
                }
                other => {
                    return Err(CliError::argument(format!(
                        "unknown option: {}",
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
        let code_execution_enabled =
            configuration::resolve_code_execution(code_execution, no_code_execution)?;
        let config = configuration::load(&values, no_config)?;
        let external_tools = ExternalTools::resolve(&config);
        let model_profiles = config.model_profiles();
        let user_rules_path = config.config_dir().map(|dir| dir.join("permissions.toml"));
        let credentials_path = configuration::credentials_path_for(&config, no_config);
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
            credentials_path,
            input,
            stdin,
            images,
            load: values.get("--load").map(PathBuf::from),
            save: values.get("--save").map(PathBuf::from),
            output_format: format_builder.build(),
        })
    }
}
