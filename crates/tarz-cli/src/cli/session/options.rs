use crate::cli::configuration;
use std::collections::HashMap;
use std::ffi::OsString;
use std::path::PathBuf;
use tarz_core::definition::SessionDefinition;

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
    pub(crate) fn parse(args: &[OsString]) -> Result<Self, String> {
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
            return Err("unknown session command".into());
        }
        let mut values = HashMap::new();
        let mut stdin = false;
        let mut json = false;
        let mut no_config = false;
        let mut args = args.iter();
        while let Some(flag) = args.next() {
            let flag = flag.to_str().ok_or("option must be UTF-8")?;
            match flag {
                "--json" if !json => json = true,
                "--no-config" if !no_config => no_config = true,
                "--stdin" if command == "run" && !stdin => stdin = true,
                "--store" | "--config" => {
                    insert(&mut values, flag, &mut args)?;
                }
                "--provider" | "--model" | "--api-key-env" | "--endpoint" | "--timeout-secs"
                | "--capacity" | "--max-model-rounds" | "--max-tool-calls" | "--load"
                | "--profile"
                    if command == "create" =>
                {
                    insert(&mut values, flag, &mut args)?;
                }
                "--input" | "--recovery-dir" if command == "run" => {
                    insert(&mut values, flag, &mut args)?;
                }
                _ => return Err("unknown, duplicate, or inapplicable session option".into()),
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
        let config = configuration::load(&values, no_config)?;
        let store = values
            .get("--store")
            .map(PathBuf::from)
            .or_else(|| config.store_dir().map(PathBuf::from))
            .ok_or("--store or a configured store directory is required")?;
        match command {
            "create" => {
                let definition = configuration::definition(&config, &values)?;
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
                    return Err("choose exactly one of --input or --stdin".into());
                }
                if input.as_ref().is_some_and(|value| value.trim().is_empty()) {
                    return Err("input must not be blank".into());
                }
                Ok(Self::Run {
                    store,
                    id: id.expect("run ID parsed"),
                    input,
                    recovery_dir: values
                        .get("--recovery-dir")
                        .map(PathBuf::from)
                        .or_else(|| config.recovery_dir().map(PathBuf::from))
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
