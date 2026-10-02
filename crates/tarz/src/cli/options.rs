use std::collections::HashMap;
use std::ffi::OsString;
use std::path::PathBuf;
use std::time::Duration;
use tarz::application::Provider;

pub(super) struct RunOptions {
    pub provider: Provider,
    pub model: String,
    pub key_env: String,
    pub endpoint: Option<String>,
    pub input: Option<String>,
    pub load: Option<PathBuf>,
    pub save: Option<PathBuf>,
    pub json: bool,
    pub timeout: Duration,
}

impl RunOptions {
    pub fn parse(args: &[OsString]) -> Result<Self, &'static str> {
        let mut values = HashMap::new();
        let mut stdin = false;
        let mut json = false;
        let mut args = args.iter();
        while let Some(flag) = args.next() {
            let flag = flag.to_str().ok_or("option must be UTF-8")?;
            match flag {
                "--stdin" if !stdin => stdin = true,
                "--json" if !json => json = true,
                "--provider" | "--model" | "--api-key-env" | "--endpoint" | "--input"
                | "--load" | "--save" | "--timeout-secs" => {
                    let value = args.next().ok_or("option needs a value")?;
                    if value.is_empty() || values.insert(flag, value).is_some() {
                        return Err("empty or duplicate option");
                    }
                }
                _ => return Err("unknown or duplicate option"),
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
        let provider = match string("--provider")?.as_deref() {
            Some("openai-chat") => Provider::OpenAiChat,
            Some("openai-responses") => Provider::OpenAiResponses,
            Some("anthropic") => Provider::Anthropic,
            _ => return Err("--provider must be openai-chat, openai-responses or anthropic"),
        };
        let model = string("--model")?.ok_or("--model is required")?;
        let input = string("--input")?;
        if stdin == input.is_some() {
            return Err("choose exactly one of --input or --stdin");
        }
        if model.trim().is_empty() || input.as_ref().is_some_and(|s| s.trim().is_empty()) {
            return Err("model and input must not be blank");
        }
        let seconds = string("--timeout-secs")?
            .map(|s| s.parse::<u64>())
            .transpose()
            .map_err(|_| "invalid timeout")?
            .unwrap_or(120);
        if !(1..=86400).contains(&seconds) {
            return Err("timeout must be 1–86400 seconds");
        }
        let key_env = string("--api-key-env")?.unwrap_or_else(|| {
            match provider {
                Provider::Anthropic => "ANTHROPIC_API_KEY",
                _ => "OPENAI_API_KEY",
            }
            .into()
        });
        if key_env.contains(['=', '\0']) {
            return Err("invalid environment variable name");
        }
        Ok(Self {
            provider,
            model,
            key_env,
            input,
            endpoint: string("--endpoint")?,
            load: values.get("--load").map(PathBuf::from),
            save: values.get("--save").map(PathBuf::from),
            json,
            timeout: Duration::from_secs(seconds),
        })
    }
}
