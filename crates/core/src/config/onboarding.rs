//! First-run configuration defaults, validation, rendering, and atomic creation.
//!
//! Writes only credential environment variable names (`api_key_env`), never
//! plaintext API keys, and refuses to overwrite an existing configuration file.

use super::{Config, ConfigError, ConfigSource, user_tars_dir};
use crate::definition::{Provider, WorkspaceTool, valid_environment_name, valid_http_endpoint};
use crate::fs_atomic::{ReplaceMode, atomic_write_bytes};
use crate::policy::PermissionMode;
use std::fmt::Write as _;
use std::io;
use std::path::{Path, PathBuf};

/// Return the default user-level `config.toml` path under [`user_tars_dir`].
pub fn default_config_path() -> Option<PathBuf> {
    user_tars_dir().map(|dir| dir.join("config.toml"))
}

/// Return the default user-level SQLite store directory (`<user_tars_dir>/data`).
pub fn default_store_dir() -> Option<PathBuf> {
    user_tars_dir().map(|dir| dir.join("data"))
}

/// Return the default workspace tools enabled for new interactive sessions on
/// Unix when sandbox availability is already known.
///
/// - On Unix with `sandbox_available == true`: all seven [`WorkspaceTool`]
///   capabilities (`list_directory`, `read_file`, `find_files`, `search_text`,
///   `write_file`, `edit_file`, `run_command`).
/// - On Unix with `sandbox_available == false`: the five non-sandboxed
///   filesystem tools (`list_directory`, `read_file`, `find_files`,
///   `write_file`, `edit_file`), excluding sandbox-gated `search_text` and
///   `run_command`.
/// - On non-Unix platforms: empty, until a platform workspace sandbox backend
///   exists.
pub fn default_tools_for_sandbox(sandbox_available: bool) -> Vec<WorkspaceTool> {
    if !cfg!(unix) {
        Vec::new()
    } else if sandbox_available {
        WorkspaceTool::ALL.to_vec()
    } else {
        vec![
            WorkspaceTool::ListDirectory,
            WorkspaceTool::ReadFile,
            WorkspaceTool::FindFiles,
            WorkspaceTool::WriteFile,
            WorkspaceTool::EditFile,
        ]
    }
}

/// Return the default workspace tools enabled for new interactive sessions on
/// the current platform.
///
/// - On macOS: all seven [`WorkspaceTool`] capabilities (Seatbelt sandbox
///   available).
/// - On Linux: adapts dynamically to [`crate::sandbox::Sandbox::open`]
///   availability (all seven tools when `bwrap` + user namespaces succeed; the
///   five non-sandboxed tools when unavailable).
/// - On other Unix platforms: directory listing, file reading, file finding,
///   writing, and editing (excluding sandbox-gated `search_text` and
///   `run_command`).
/// - On non-Unix platforms: empty, until a platform workspace sandbox backend
///   exists.
pub fn default_tools_for_current_platform() -> Vec<WorkspaceTool> {
    if cfg!(target_os = "macos") {
        default_tools_for_sandbox(true)
    } else if cfg!(target_os = "linux") {
        default_tools_for_sandbox(crate::sandbox::Sandbox::open().is_ok())
    } else {
        default_tools_for_sandbox(false)
    }
}

/// User-supplied fields for generating an initial `config.toml` profile.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OnboardingInput {
    /// Selected model wire protocol.
    pub provider: Provider,
    /// Model identifier for `[profiles.default]`.
    pub model: String,
    /// Environment variable name holding the provider API key (never a key value).
    pub api_key_env: String,
    /// Optional custom HTTP(S) endpoint URL; empty/whitespace-only is treated as `None`.
    pub endpoint: Option<String>,
    /// Optional maximum output tokens per model call (required and positive for Anthropic).
    pub max_tokens: Option<u32>,
    /// Initial workspace permission mode for the generated profile.
    pub permission_mode: PermissionMode,
}

/// Errors produced when validating, rendering, or atomically creating an initial config.
#[derive(Debug, thiserror::Error)]
pub enum OnboardingError {
    /// Model identifier was empty/whitespace or contained control characters.
    #[error("model identifier must not be empty or contain control characters")]
    EmptyModel,
    /// Credential environment variable name was invalid or resembled a literal secret.
    ///
    /// The offending value is omitted so a pasted secret can never leak via
    /// `Display` or `Debug`.
    #[error(
        "invalid credential environment variable name; expected an env var name such as OPENAI_API_KEY, never a plaintext key"
    )]
    InvalidApiKeyEnv,
    /// Anthropic profiles require a positive `max_tokens` limit.
    #[error("Anthropic profiles require a positive max_tokens value")]
    AnthropicRequiresMaxTokens,
    /// Explicit `max_tokens` was zero.
    #[error("max_tokens must be greater than zero")]
    InvalidMaxTokens,
    /// Custom endpoint URL failed HTTP(S) validation.
    #[error("endpoint must be a valid HTTP(S) URL without credentials, query, or fragment")]
    InvalidEndpoint,
    /// Target `config.toml` already exists; onboarding never overwrites existing config.
    #[error("configuration file already exists at {0:?}")]
    AlreadyExists(PathBuf),
    /// Rendered configuration failed `Config` schema or definition validation.
    #[error(transparent)]
    Validation(#[from] ConfigError),
    /// Filesystem I/O failed while preparing directories or writing the file.
    #[error("configuration file {path:?}: {kind:?}")]
    Io {
        /// Path that failed.
        path: PathBuf,
        /// Original I/O error category.
        kind: io::ErrorKind,
    },
}

/// Validate `input` and render a valid `version = 1` TOML configuration file.
///
/// The rendered document includes `store = "data"`, `recovery_dir = "."`,
/// `default_profile = "default"`, and `[profiles.default]` with TOML-escaped
/// strings. It is verified in memory against the `Config` parser and definition
/// validator before returning.
pub fn render_initial_config(input: &OnboardingInput) -> Result<String, OnboardingError> {
    let model = input.model.trim();
    if model.is_empty() || model.chars().any(char::is_control) {
        return Err(OnboardingError::EmptyModel);
    }

    let api_key_env = input.api_key_env.trim();
    if !valid_environment_name(api_key_env) {
        return Err(OnboardingError::InvalidApiKeyEnv);
    }

    if input.max_tokens == Some(0) {
        return Err(OnboardingError::InvalidMaxTokens);
    }
    if input.provider == Provider::Anthropic && input.max_tokens.is_none() {
        return Err(OnboardingError::AnthropicRequiresMaxTokens);
    }

    let endpoint = input
        .endpoint
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty());
    if let Some(ep) = endpoint
        && (ep.chars().any(char::is_control) || !valid_http_endpoint(ep))
    {
        return Err(OnboardingError::InvalidEndpoint);
    }

    let mut toml = String::new();
    let _ = writeln!(toml, "version = 1");
    let _ = writeln!(toml, "store = \"data\"");
    let _ = writeln!(toml, "recovery_dir = \".\"");
    let _ = writeln!(toml, "default_profile = \"default\"");
    let _ = writeln!(toml);
    let _ = writeln!(toml, "[profiles.default]");
    let _ = writeln!(toml, "provider = {}", toml_string(input.provider.as_str()));
    let _ = writeln!(toml, "model = {}", toml_string(model));
    let _ = writeln!(toml, "api_key_env = {}", toml_string(api_key_env));
    if let Some(ep) = endpoint {
        let _ = writeln!(toml, "endpoint = {}", toml_string(ep));
    }
    if let Some(max_tokens) = input.max_tokens {
        let _ = writeln!(toml, "max_tokens = {max_tokens}");
    }
    let _ = writeln!(
        toml,
        "permission_mode = {}",
        toml_string(input.permission_mode.as_str())
    );

    // Guarantee the rendered TOML parses and validates identically to Config::load.
    let synthetic = Path::new("/tars-onboarding-validation/config.toml");
    let parsed = Config::parse_toml(&toml, synthetic)?;
    parsed.validate_profiles()?;

    Ok(toml)
}

/// Render and atomically write an initial `config.toml` at `path` using
/// `CreateNew` semantics, then load and return the validated [`Config`].
///
/// Creates missing parent directories automatically. Never overwrites an
/// existing file at `path`, returning [`OnboardingError::AlreadyExists`] instead.
pub fn write_initial_config(
    path: &Path,
    input: &OnboardingInput,
) -> Result<Config, OnboardingError> {
    let rendered = render_initial_config(input)?;

    if let Some(parent) = path.parent().filter(|dir| !dir.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent).map_err(|error| OnboardingError::Io {
            path: parent.to_path_buf(),
            kind: error.kind(),
        })?;
    }

    atomic_write_bytes(
        path,
        Some(".config.toml.pending-"),
        rendered.as_bytes(),
        ReplaceMode::CreateNew,
    )
    .map_err(|error| {
        if error.kind() == io::ErrorKind::AlreadyExists {
            OnboardingError::AlreadyExists(path.to_path_buf())
        } else {
            OnboardingError::Io {
                path: path.to_path_buf(),
                kind: error.kind(),
            }
        }
    })?;

    Ok(Config::load(ConfigSource::File(path.to_path_buf()))?)
}

fn toml_string(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for ch in value.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\u{0008}' => out.push_str("\\b"),
            '\t' => out.push_str("\\t"),
            '\n' => out.push_str("\\n"),
            '\u{000C}' => out.push_str("\\f"),
            '\r' => out.push_str("\\r"),
            c if (c as u32) < 0x20 || c == '\u{007F}' => {
                let _ = write!(out, "\\u{:04X}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::definition::CredentialRef;

    fn sample_input(provider: Provider) -> OnboardingInput {
        OnboardingInput {
            provider,
            model: match provider {
                Provider::Anthropic => "claude-sonnet-4-5".into(),
                Provider::OpenAiResponses => "gpt-5".into(),
                Provider::OpenAiChat => "gpt-4o".into(),
            },
            api_key_env: match provider {
                Provider::Anthropic => "ANTHROPIC_API_KEY".into(),
                _ => "OPENAI_API_KEY".into(),
            },
            endpoint: None,
            max_tokens: (provider == Provider::Anthropic).then_some(4096),
            permission_mode: PermissionMode::Ask,
        }
    }

    #[test]
    fn renders_and_round_trips_all_three_providers() {
        for provider in [
            Provider::OpenAiResponses,
            Provider::OpenAiChat,
            Provider::Anthropic,
        ] {
            let dir = tempfile::tempdir().unwrap();
            let cfg_path = dir.path().join("config.toml");
            let input = sample_input(provider);
            let config = write_initial_config(&cfg_path, &input).unwrap();
            assert_eq!(config.source(), Some(cfg_path.as_path()));
            assert_eq!(config.store_dir(), Some(dir.path().join("data").as_path()));
            assert_eq!(config.recovery_dir(), Some(dir.path().join(".").as_path()));
            let def = config.preset(None).unwrap().unwrap();
            assert_eq!(def.provider, provider);
            assert_eq!(def.model, input.model);
            assert_eq!(
                def.credential,
                CredentialRef::Environment(input.api_key_env.clone())
            );
            assert_eq!(def.max_tokens, input.max_tokens);
            assert_eq!(def.initial_permission_mode(), PermissionMode::Ask);
        }
    }

    #[test]
    fn escapes_special_toml_characters_in_model_and_endpoint() {
        let dir = tempfile::tempdir().unwrap();
        let cfg_path = dir.path().join("config.toml");
        let input = OnboardingInput {
            provider: Provider::OpenAiChat,
            model: "  org/model\"quoted\\slash  ".into(),
            api_key_env: " CUSTOM_KEY_1 ".into(),
            endpoint: Some("  https://api.example.com/v1/\"path\"  ".into()),
            max_tokens: Some(2048),
            permission_mode: PermissionMode::AcceptEdits,
        };
        let rendered = render_initial_config(&input).unwrap();
        assert!(rendered.contains("api_key_env = \"CUSTOM_KEY_1\""));
        assert!(!rendered.contains("sk-"));

        let config = write_initial_config(&cfg_path, &input).unwrap();
        let def = config.preset(None).unwrap().unwrap();
        assert_eq!(def.model, "org/model\"quoted\\slash");
        assert_eq!(
            def.endpoint.as_deref(),
            Some("https://api.example.com/v1/\"path\"")
        );
        assert_eq!(def.max_tokens, Some(2048));
        assert_eq!(def.initial_permission_mode(), PermissionMode::AcceptEdits);
    }

    #[test]
    fn blank_endpoint_is_omitted_as_none() {
        let input = OnboardingInput {
            endpoint: Some("   ".into()),
            ..sample_input(Provider::OpenAiResponses)
        };
        let rendered = render_initial_config(&input).unwrap();
        assert!(!rendered.contains("endpoint"));
    }

    #[test]
    fn rejects_empty_and_control_char_models() {
        for bad_model in ["", "   ", "gpt\n5", "gpt\r5", "gpt\x005"] {
            let input = OnboardingInput {
                model: bad_model.into(),
                ..sample_input(Provider::OpenAiChat)
            };
            let err = render_initial_config(&input).unwrap_err();
            assert!(matches!(err, OnboardingError::EmptyModel), "got {err:?}");
        }
    }

    #[test]
    fn rejects_invalid_api_key_env_and_never_echoes_secret_like_values() {
        for bad_env in [
            "",
            "   ",
            "sk-live-secret-do-not-echo",
            "123_INVALID",
            "OPENAI API KEY",
            "OPENAI-API-KEY",
        ] {
            let input = OnboardingInput {
                api_key_env: bad_env.into(),
                ..sample_input(Provider::OpenAiResponses)
            };
            let err = render_initial_config(&input).unwrap_err();
            assert!(
                matches!(err, OnboardingError::InvalidApiKeyEnv),
                "got {err:?}"
            );
            assert!(!err.to_string().contains("sk-live-secret-do-not-echo"));
            assert!(!format!("{err:?}").contains("sk-live-secret-do-not-echo"));
        }
    }

    #[test]
    fn validates_anthropic_and_positive_max_tokens() {
        let missing = OnboardingInput {
            max_tokens: None,
            ..sample_input(Provider::Anthropic)
        };
        assert!(matches!(
            render_initial_config(&missing),
            Err(OnboardingError::AnthropicRequiresMaxTokens)
        ));

        let zero_anthropic = OnboardingInput {
            max_tokens: Some(0),
            ..sample_input(Provider::Anthropic)
        };
        assert!(matches!(
            render_initial_config(&zero_anthropic),
            Err(OnboardingError::InvalidMaxTokens)
        ));

        let zero_openai = OnboardingInput {
            max_tokens: Some(0),
            ..sample_input(Provider::OpenAiChat)
        };
        assert!(matches!(
            render_initial_config(&zero_openai),
            Err(OnboardingError::InvalidMaxTokens)
        ));
    }

    #[test]
    fn rejects_invalid_endpoints() {
        for bad_url in [
            "not-a-url",
            "ftp://example.com/v1",
            "https://user:pass@example.com/v1",
            "https://example.com/v1?foo=bar",
            "https://example.com/v1#frag",
            "https://example.com/v1 with space",
        ] {
            let input = OnboardingInput {
                endpoint: Some(bad_url.into()),
                ..sample_input(Provider::OpenAiResponses)
            };
            let err = render_initial_config(&input).unwrap_err();
            assert!(
                matches!(err, OnboardingError::InvalidEndpoint),
                "for {bad_url}: got {err:?}"
            );
        }
    }

    #[test]
    fn write_initial_config_creates_nested_parent_dirs() {
        let dir = tempfile::tempdir().unwrap();
        let nested = dir.path().join("a").join("b").join("config.toml");
        let config =
            write_initial_config(&nested, &sample_input(Provider::OpenAiResponses)).unwrap();
        assert!(nested.is_file());
        assert_eq!(config.source(), Some(nested.as_path()));
    }

    #[test]
    fn write_initial_config_refuses_to_overwrite_existing_file_or_leave_temps() {
        let dir = tempfile::tempdir().unwrap();
        let cfg_path = dir.path().join("config.toml");
        std::fs::write(&cfg_path, "# existing user config\n").unwrap();

        let err = write_initial_config(&cfg_path, &sample_input(Provider::OpenAiChat)).unwrap_err();
        assert!(matches!(err, OnboardingError::AlreadyExists(ref p) if p == &cfg_path));
        assert_eq!(
            std::fs::read_to_string(&cfg_path).unwrap(),
            "# existing user config\n"
        );

        let entries: Vec<String> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(entries, vec!["config.toml"]);
    }

    #[test]
    fn default_tools_for_sandbox_adapts_to_availability() {
        let with_sandbox = default_tools_for_sandbox(true);
        let without_sandbox = default_tools_for_sandbox(false);
        if cfg!(unix) {
            assert_eq!(with_sandbox, WorkspaceTool::ALL);
            assert_eq!(
                without_sandbox,
                vec![
                    WorkspaceTool::ListDirectory,
                    WorkspaceTool::ReadFile,
                    WorkspaceTool::FindFiles,
                    WorkspaceTool::WriteFile,
                    WorkspaceTool::EditFile,
                ]
            );
        } else {
            assert!(with_sandbox.is_empty());
            assert!(without_sandbox.is_empty());
        }
    }

    #[test]
    fn platform_default_tools_match_target_capabilities() {
        let tools = default_tools_for_current_platform();
        if cfg!(target_os = "macos") {
            assert_eq!(tools, WorkspaceTool::ALL);
        } else if cfg!(target_os = "linux") {
            let expected = default_tools_for_sandbox(crate::sandbox::Sandbox::open().is_ok());
            assert_eq!(tools, expected);
        } else if cfg!(unix) {
            assert_eq!(
                tools,
                vec![
                    WorkspaceTool::ListDirectory,
                    WorkspaceTool::ReadFile,
                    WorkspaceTool::FindFiles,
                    WorkspaceTool::WriteFile,
                    WorkspaceTool::EditFile,
                ]
            );
        } else {
            assert!(tools.is_empty());
        }
    }
}
