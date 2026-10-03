use std::collections::HashMap;
use std::ffi::OsString;
use std::path::PathBuf;
use tarz_core::definition::{CredentialRef, Provider, SessionDefinition};

pub(crate) enum SessionCommand {
    Create {
        store: PathBuf,
        definition: SessionDefinition,
        load: Option<PathBuf>,
        json: bool,
    },
    Run {
        store: PathBuf,
        id: String,
        input: Option<String>,
        recovery_dir: PathBuf,
        json: bool,
    },
    List {
        store: PathBuf,
        json: bool,
    },
}

impl SessionCommand {
    pub(crate) fn parse(args: &[OsString]) -> Result<Self, &'static str> {
        let (command, mut args) = args.split_first().ok_or("session command is required")?;
        let command = command.to_str().ok_or("session command must be UTF-8")?;
        let id = if command == "run" {
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
        if !matches!(command, "create" | "run" | "list") {
            return Err("unknown session command");
        }
        let mut values = HashMap::new();
        let mut stdin = false;
        let mut json = false;
        let mut args = args.iter();
        while let Some(flag) = args.next() {
            let flag = flag.to_str().ok_or("option must be UTF-8")?;
            match flag {
                "--json" if !json => json = true,
                "--stdin" if command == "run" && !stdin => stdin = true,
                "--store" => {
                    insert(&mut values, flag, &mut args)?;
                }
                "--provider" | "--model" | "--api-key-env" | "--endpoint" | "--timeout-secs"
                | "--capacity" | "--max-model-rounds" | "--max-tool-calls" | "--load"
                    if command == "create" =>
                {
                    insert(&mut values, flag, &mut args)?;
                }
                "--input" | "--recovery-dir" if command == "run" => {
                    insert(&mut values, flag, &mut args)?;
                }
                _ => return Err("unknown, duplicate, or inapplicable session option"),
            }
        }
        let string = |key| -> Result<Option<String>, &'static str> {
            values
                .get(key)
                .map(|value| {
                    value
                        .to_str()
                        .map(str::to_owned)
                        .ok_or("text option must be UTF-8")
                })
                .transpose()
        };
        let store = PathBuf::from(*values.get("--store").ok_or("--store is required")?);
        match command {
            "create" => {
                let provider = match string("--provider")?.as_deref() {
                    Some("openai-chat") => Provider::OpenAiChat,
                    Some("openai-responses") => Provider::OpenAiResponses,
                    Some("anthropic") => Provider::Anthropic,
                    _ => {
                        return Err(
                            "--provider must be openai-chat, openai-responses or anthropic",
                        );
                    }
                };
                let model = string("--model")?.ok_or("--model is required")?;
                let env = string("--api-key-env")?.unwrap_or_else(|| match provider {
                    Provider::Anthropic => "ANTHROPIC_API_KEY".into(),
                    _ => "OPENAI_API_KEY".into(),
                });
                let number = |key, default| -> Result<u64, &'static str> {
                    string(key)?
                        .map(|value| value.parse().map_err(|_| "invalid numeric session option"))
                        .unwrap_or(Ok(default))
                };
                let timeout = number("--timeout-secs", 120)?;
                if !(1..=86400).contains(&timeout) {
                    return Err("timeout must be 1–86400 seconds");
                }
                let definition = SessionDefinition {
                    provider,
                    model,
                    endpoint: string("--endpoint")?,
                    credential: CredentialRef::Environment(env),
                    work_timeout_secs: Some(timeout),
                    retained_work_capacity: number("--capacity", 256)?
                        .try_into()
                        .map_err(|_| "capacity is too large")?,
                    max_model_rounds: number("--max-model-rounds", 10)?
                        .try_into()
                        .map_err(|_| "model round limit is too large")?,
                    max_tool_calls: number("--max-tool-calls", 64)?
                        .try_into()
                        .map_err(|_| "tool call limit is too large")?,
                };
                definition
                    .validate()
                    .map_err(|_| "invalid session definition")?;
                Ok(Self::Create {
                    store,
                    definition,
                    load: values.get("--load").map(PathBuf::from),
                    json,
                })
            }
            "run" => {
                let input = string("--input")?;
                if stdin == input.is_some() {
                    return Err("choose exactly one of --input or --stdin");
                }
                if input.as_ref().is_some_and(|value| value.trim().is_empty()) {
                    return Err("input must not be blank");
                }
                Ok(Self::Run {
                    store,
                    id: id.expect("run ID parsed"),
                    input,
                    recovery_dir: values
                        .get("--recovery-dir")
                        .map(PathBuf::from)
                        .unwrap_or_else(|| PathBuf::from(".")),
                    json,
                })
            }
            _ => Ok(Self::List { store, json }),
        }
    }
}

fn insert<'a>(
    values: &mut HashMap<&'a str, &'a OsString>,
    flag: &'a str,
    args: &mut std::slice::Iter<'a, OsString>,
) -> Result<(), &'static str> {
    let value = args.next().ok_or("option needs a value")?;
    if value.is_empty() || values.insert(flag, value).is_some() {
        return Err("empty or duplicate option");
    }
    Ok(())
}
