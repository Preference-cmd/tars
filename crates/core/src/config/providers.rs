//! Built-in model provider presets and safe `config.toml` profile updates.
//!
//! `upsert_profile_in_config` is invoked only on explicit user confirmation,
//! never silently on startup, and writes only `credential = "<name>"` references
//! (never plaintext API keys).

use super::{Config, ConfigError, ConfigSource, toml_string};
use crate::definition::{Provider, valid_credential_name, valid_http_endpoint};
use crate::fs_atomic::{ReplaceMode, atomic_write_bytes};
use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;
use std::io::{self, Read};
use std::path::Path;

const MAX_CONFIG_FILE_BYTES: usize = 1024 * 1024;

/// Built-in model provider preset for quick onboarding and `/provider` selection.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProviderPreset {
    /// Stable preset identifier and default profile/credential name.
    pub id: &'static str,
    /// Human-readable display label.
    pub label: &'static str,
    /// Wire protocol used by Causa.
    pub provider: Provider,
    /// Custom HTTP(S) endpoint URL, or `None` to use Causa's protocol default.
    pub endpoint: Option<&'static str>,
    /// Suggested default model identifier for this preset.
    pub suggested_model: &'static str,
    /// Default `max_tokens` limit when required or recommended by the provider.
    pub default_max_tokens: Option<u32>,
    /// Whether a remote API key is expected (`false` for local endpoints such as Ollama).
    pub requires_api_key: bool,
}

/// Built-in catalog of supported model provider presets.
pub const PROVIDER_PRESETS: &[ProviderPreset] = &[
    ProviderPreset {
        id: "openai",
        label: "OpenAI",
        provider: Provider::OpenAiResponses,
        endpoint: None,
        suggested_model: "gpt-5.4",
        default_max_tokens: None,
        requires_api_key: true,
    },
    ProviderPreset {
        id: "anthropic",
        label: "Anthropic",
        provider: Provider::Anthropic,
        endpoint: None,
        suggested_model: "claude-sonnet-4-6",
        default_max_tokens: Some(8192),
        requires_api_key: true,
    },
    ProviderPreset {
        id: "openrouter",
        label: "OpenRouter",
        provider: Provider::OpenAiChat,
        endpoint: Some("https://openrouter.ai/api/v1/chat/completions"),
        suggested_model: "anthropic/claude-sonnet-4.6",
        default_max_tokens: None,
        requires_api_key: true,
    },
    ProviderPreset {
        id: "deepseek",
        label: "DeepSeek",
        provider: Provider::OpenAiChat,
        endpoint: Some("https://api.deepseek.com/v1/chat/completions"),
        suggested_model: "deepseek-chat",
        default_max_tokens: None,
        requires_api_key: true,
    },
    ProviderPreset {
        id: "xai",
        label: "xAI (Grok)",
        provider: Provider::OpenAiChat,
        endpoint: Some("https://api.x.ai/v1/chat/completions"),
        suggested_model: "grok-3",
        default_max_tokens: None,
        requires_api_key: true,
    },
    ProviderPreset {
        id: "gemini",
        label: "Google Gemini",
        provider: Provider::OpenAiChat,
        endpoint: Some("https://generativelanguage.googleapis.com/v1beta/openai/chat/completions"),
        suggested_model: "gemini-2.5-pro",
        default_max_tokens: None,
        requires_api_key: true,
    },
    ProviderPreset {
        id: "ollama",
        label: "Ollama (Local)",
        provider: Provider::OpenAiChat,
        endpoint: Some("http://127.0.0.1:11434/v1/chat/completions"),
        suggested_model: "qwen3:8b",
        default_max_tokens: None,
        requires_api_key: false,
    },
];

/// Look up a built-in [`ProviderPreset`] by its stable `id`.
pub fn find_provider_preset(id: &str) -> Option<&'static ProviderPreset> {
    PROVIDER_PRESETS.iter().find(|preset| preset.id == id)
}

/// Explicit user-confirmed profile parameters for [`upsert_profile_in_config`].
///
/// Carries only a credential store reference name (`credential`), never a
/// plaintext API key.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProfileUpsertInput {
    /// Profile table name (`[profiles.<profile_name>]`).
    pub profile_name: String,
    /// Model wire protocol.
    pub provider: Provider,
    /// Non-empty model identifier without control characters.
    pub model: String,
    /// Optional list of selectable model identifiers for this profile.
    pub models: Vec<String>,
    /// Credential store entry name (`credential = "<name>"`).
    pub credential: String,
    /// Optional custom HTTP(S) endpoint URL; empty/whitespace-only is treated as `None`.
    pub endpoint: Option<String>,
    /// Optional maximum output tokens per model call (required and positive for Anthropic).
    pub max_tokens: Option<u32>,
}

struct NormalizedUpsert<'a> {
    profile_name: &'a str,
    provider: Provider,
    model: &'a str,
    models: Vec<&'a str>,
    credential: &'a str,
    endpoint: Option<&'a str>,
    max_tokens: Option<u32>,
}

/// Insert or update `[profiles.<profile_name>]` in `config_path` using only a
/// `credential = "<name>"` reference, validating the full resulting config
/// (including sibling `mcp.json` or `TARS_MCP_CONFIG`) before atomic write.
pub fn upsert_profile_in_config(
    config_path: &Path,
    input: &ProfileUpsertInput,
) -> Result<Config, ConfigError> {
    if config_path.as_os_str().is_empty() {
        return Err(ConfigError::EmptyPath);
    }
    let normalized = validate_upsert_input(input)?;
    let abs_path = std::path::absolute(config_path).map_err(|error| ConfigError::Io {
        path: config_path.to_path_buf(),
        kind: error.kind(),
    })?;

    // Check symlink_metadata BEFORE exists() so dangling symlinks fail closed.
    let lstat_meta = match std::fs::symlink_metadata(&abs_path) {
        Ok(meta) => Some(meta),
        Err(error) if error.kind() == io::ErrorKind::NotFound => None,
        Err(error) => {
            return Err(ConfigError::Io {
                path: config_path.to_path_buf(),
                kind: error.kind(),
            });
        }
    };

    let (rendered, replace_mode) = match lstat_meta {
        None => (render_new_config(&normalized), ReplaceMode::CreateNew),
        Some(meta) => {
            if meta.file_type().is_symlink() {
                return Err(ConfigError::SymlinkNotAllowed(config_path.to_path_buf()));
            }
            if !meta.file_type().is_file() {
                return Err(ConfigError::Io {
                    path: config_path.to_path_buf(),
                    kind: io::ErrorKind::InvalidInput,
                });
            }
            let existing = read_existing_config(&abs_path, config_path, &meta)?;
            let updated = merge_existing_config(&existing, &normalized)?;
            (updated, ReplaceMode::Overwrite)
        }
    };

    let mut candidate = Config::parse_toml(&rendered, &abs_path)?;
    candidate.load_mcp_for_base_dir(abs_path.parent())?;
    candidate.validate_profiles()?;

    if let Some(parent) = abs_path.parent().filter(|dir| !dir.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent).map_err(|error| ConfigError::Io {
            path: parent.to_path_buf(),
            kind: error.kind(),
        })?;
    }

    // Re-check symlink immediately before atomic rename when overwriting.
    if replace_mode == ReplaceMode::Overwrite
        && let Ok(meta) = std::fs::symlink_metadata(&abs_path)
        && meta.file_type().is_symlink()
    {
        return Err(ConfigError::SymlinkNotAllowed(config_path.to_path_buf()));
    }

    atomic_write_bytes(
        &abs_path,
        Some(".config.toml.pending-"),
        rendered.as_bytes(),
        replace_mode,
    )
    .map_err(|error| ConfigError::Io {
        path: config_path.to_path_buf(),
        kind: error.kind(),
    })?;

    Config::load(ConfigSource::File(abs_path))
}

fn validate_upsert_input(input: &ProfileUpsertInput) -> Result<NormalizedUpsert<'_>, ConfigError> {
    let profile_name = input.profile_name.trim();
    if profile_name.is_empty() {
        return Err(ConfigError::EmptyProfile);
    }
    if !valid_credential_name(profile_name) {
        return Err(ConfigError::InvalidProfileName);
    }

    let credential = input.credential.trim();
    if !valid_credential_name(credential) {
        return Err(ConfigError::Definition(
            crate::definition::DefinitionError::InvalidCredentialReference,
        ));
    }

    let model = input.model.trim();
    if model.is_empty() || model.chars().any(char::is_control) {
        return Err(ConfigError::Definition(
            crate::definition::DefinitionError::BlankModel,
        ));
    }

    let mut unique_models = Vec::with_capacity(1 + input.models.len());
    let mut seen_models = HashSet::with_capacity(1 + input.models.len());
    unique_models.push(model);
    seen_models.insert(model);
    for entry in &input.models {
        let trimmed = entry.trim();
        if trimmed.is_empty() || trimmed.chars().any(char::is_control) {
            return Err(ConfigError::InvalidProfileModels(profile_name.to_owned()));
        }
        if seen_models.insert(trimmed) {
            unique_models.push(trimmed);
        }
    }

    let endpoint = input
        .endpoint
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty());
    if let Some(ep) = endpoint
        && (ep.chars().any(char::is_control) || !valid_http_endpoint(ep))
    {
        return Err(ConfigError::Definition(
            crate::definition::DefinitionError::InvalidEndpoint,
        ));
    }

    if input.max_tokens == Some(0) {
        return Err(ConfigError::Definition(
            crate::definition::DefinitionError::InvalidMaxTokens,
        ));
    }
    if input.provider == Provider::Anthropic && input.max_tokens.is_none() {
        return Err(ConfigError::AnthropicRequiresMaxTokens);
    }

    Ok(NormalizedUpsert {
        profile_name,
        provider: input.provider,
        model,
        models: unique_models,
        credential,
        endpoint,
        max_tokens: input.max_tokens,
    })
}

fn render_new_config(input: &NormalizedUpsert<'_>) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "version = 1");
    let _ = writeln!(out, "store = \"data\"");
    let _ = writeln!(out, "recovery_dir = \".\"");
    let _ = writeln!(out, "default_profile = {}", toml_string(input.profile_name));
    out.push('\n');
    let _ = writeln!(out, "[profiles.{}]", input.profile_name);
    for line in rendered_profile_fields(input) {
        out.push_str(&line);
        out.push('\n');
    }
    out
}

fn format_models_line(models: &[&str]) -> Option<String> {
    if models.len() < 2 {
        return None;
    }
    let items: Vec<String> = models.iter().map(|model| toml_string(model)).collect();
    Some(format!("models = [{}]", items.join(", ")))
}

fn rendered_profile_fields(input: &NormalizedUpsert<'_>) -> Vec<String> {
    let mut lines = vec![
        format!("provider = {}", toml_string(input.provider.as_str())),
        format!("model = {}", toml_string(input.model)),
    ];
    if let Some(models_line) = format_models_line(&input.models) {
        lines.push(models_line);
    }
    lines.push(format!("credential = {}", toml_string(input.credential)));
    if let Some(ep) = input.endpoint {
        lines.push(format!("endpoint = {}", toml_string(ep)));
    }
    if let Some(max_tokens) = input.max_tokens {
        lines.push(format!("max_tokens = {max_tokens}"));
    }
    lines
}

fn read_existing_config(
    abs_path: &Path,
    display_path: &Path,
    lstat_meta: &std::fs::Metadata,
) -> Result<String, ConfigError> {
    let file = open_config_nofollow(abs_path, display_path, lstat_meta)?;
    let mut text = String::new();
    file.take(MAX_CONFIG_FILE_BYTES as u64 + 1)
        .read_to_string(&mut text)
        .map_err(|error| ConfigError::Io {
            path: display_path.to_path_buf(),
            kind: error.kind(),
        })?;
    if text.len() > MAX_CONFIG_FILE_BYTES {
        return Err(ConfigError::TooLarge);
    }
    Ok(text)
}

#[cfg(unix)]
fn open_config_nofollow(
    abs_path: &Path,
    display_path: &Path,
    lstat_meta: &std::fs::Metadata,
) -> Result<std::fs::File, ConfigError> {
    use rustix::fs::{Mode, OFlags, open};
    use std::os::unix::fs::MetadataExt;

    let fd = open(abs_path, OFlags::RDONLY | OFlags::NOFOLLOW, Mode::empty()).map_err(|errno| {
        if errno == rustix::io::Errno::LOOP {
            ConfigError::SymlinkNotAllowed(display_path.to_path_buf())
        } else {
            let io_err = io::Error::from_raw_os_error(errno.raw_os_error());
            ConfigError::Io {
                path: display_path.to_path_buf(),
                kind: io_err.kind(),
            }
        }
    })?;
    let file = std::fs::File::from(fd);
    let fstat_meta = file.metadata().map_err(|error| ConfigError::Io {
        path: display_path.to_path_buf(),
        kind: error.kind(),
    })?;
    if !fstat_meta.file_type().is_file() {
        return Err(ConfigError::Io {
            path: display_path.to_path_buf(),
            kind: io::ErrorKind::InvalidInput,
        });
    }
    if fstat_meta.dev() != lstat_meta.dev() || fstat_meta.ino() != lstat_meta.ino() {
        return Err(ConfigError::Io {
            path: display_path.to_path_buf(),
            kind: io::ErrorKind::InvalidInput,
        });
    }
    if fstat_meta.len() > MAX_CONFIG_FILE_BYTES as u64 {
        return Err(ConfigError::TooLarge);
    }
    Ok(file)
}

#[cfg(not(unix))]
fn open_config_nofollow(
    abs_path: &Path,
    display_path: &Path,
    _lstat_meta: &std::fs::Metadata,
) -> Result<std::fs::File, ConfigError> {
    let file = std::fs::File::open(abs_path).map_err(|error| ConfigError::Io {
        path: display_path.to_path_buf(),
        kind: error.kind(),
    })?;
    let meta = file.metadata().map_err(|error| ConfigError::Io {
        path: display_path.to_path_buf(),
        kind: error.kind(),
    })?;
    if !meta.file_type().is_file() {
        return Err(ConfigError::Io {
            path: display_path.to_path_buf(),
            kind: io::ErrorKind::InvalidInput,
        });
    }
    if meta.len() > MAX_CONFIG_FILE_BYTES as u64 {
        return Err(ConfigError::TooLarge);
    }
    Ok(file)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ActiveSection {
    TopLevel,
    ProfilesTable,
    TargetProfile,
    Other,
}

#[derive(Default)]
struct LexerState {
    in_ml_basic: bool,
    in_ml_literal: bool,
    bracket_depth: usize,
    brace_depth: usize,
}

impl LexerState {
    fn at_statement_boundary(&self) -> bool {
        !self.in_ml_basic && !self.in_ml_literal && self.bracket_depth == 0 && self.brace_depth == 0
    }

    fn advance_line(&mut self, line: &str) {
        self.advance_slice(line, 0);
    }

    fn advance_slice(&mut self, line: &str, start_byte: usize) {
        let bytes = line.as_bytes();
        let mut i = start_byte;
        let mut in_basic = false;
        let mut in_literal = false;
        let mut escaped = false;

        while i < bytes.len() {
            if self.in_ml_basic {
                if escaped {
                    escaped = false;
                    i += 1;
                    continue;
                }
                if bytes[i] == b'\\' {
                    escaped = true;
                    i += 1;
                    continue;
                }
                if bytes[i..].starts_with(b"\"\"\"") {
                    let mut run = 3;
                    while run < 5 && i + run < bytes.len() && bytes[i + run] == b'"' {
                        run += 1;
                    }
                    self.in_ml_basic = false;
                    i += run;
                    continue;
                }
                i += 1;
                continue;
            }
            if self.in_ml_literal {
                if bytes[i..].starts_with(b"'''") {
                    let mut run = 3;
                    while run < 5 && i + run < bytes.len() && bytes[i + run] == b'\'' {
                        run += 1;
                    }
                    self.in_ml_literal = false;
                    i += run;
                    continue;
                }
                i += 1;
                continue;
            }
            if in_basic {
                if escaped {
                    escaped = false;
                    i += 1;
                    continue;
                }
                if bytes[i] == b'\\' {
                    escaped = true;
                    i += 1;
                    continue;
                }
                if bytes[i] == b'"' {
                    in_basic = false;
                }
                i += 1;
                continue;
            }
            if in_literal {
                if bytes[i] == b'\'' {
                    in_literal = false;
                }
                i += 1;
                continue;
            }

            match bytes[i] {
                b'#' => break,
                b'"' => {
                    if bytes[i..].starts_with(b"\"\"\"") {
                        self.in_ml_basic = true;
                        i += 3;
                    } else {
                        in_basic = true;
                        i += 1;
                    }
                }
                b'\'' => {
                    if bytes[i..].starts_with(b"'''") {
                        self.in_ml_literal = true;
                        i += 3;
                    } else {
                        in_literal = true;
                        i += 1;
                    }
                }
                b'[' => {
                    self.bracket_depth += 1;
                    i += 1;
                }
                b']' => {
                    self.bracket_depth = self.bracket_depth.saturating_sub(1);
                    i += 1;
                }
                b'{' => {
                    self.brace_depth += 1;
                    i += 1;
                }
                b'}' => {
                    self.brace_depth = self.brace_depth.saturating_sub(1);
                    i += 1;
                }
                _ => i += 1,
            }
        }
    }
}

struct LinePiece<'a> {
    text: &'a str,
    eol: &'a str,
}

fn split_lines_preserving_eol(text: &str) -> Vec<LinePiece<'_>> {
    let mut lines = Vec::new();
    let bytes = text.as_bytes();
    let mut start = 0;
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\n' {
            if i > start && bytes[i - 1] == b'\r' {
                lines.push(LinePiece {
                    text: &text[start..i - 1],
                    eol: &text[i - 1..=i],
                });
            } else {
                lines.push(LinePiece {
                    text: &text[start..i],
                    eol: &text[i..=i],
                });
            }
            start = i + 1;
        }
        i += 1;
    }
    if start < text.len() {
        lines.push(LinePiece {
            text: &text[start..],
            eol: "",
        });
    }
    lines
}

fn merge_existing_config(
    existing: &str,
    input: &NormalizedUpsert<'_>,
) -> Result<String, ConfigError> {
    // Validate existing TOML syntax first so line/column syntax errors stay accurate.
    toml::from_str::<toml::Table>(existing).map_err(|error: toml::de::Error| {
        let offset = error
            .span()
            .map_or(0, |span| span.start)
            .min(existing.len());
        let prefix = &existing[..existing.floor_char_boundary(offset)];
        ConfigError::Syntax {
            line: prefix.bytes().filter(|byte| *byte == b'\n').count() + 1,
            column: prefix.rsplit('\n').next().unwrap_or("").chars().count() + 1,
        }
    })?;

    let eol = if existing.contains("\r\n") {
        "\r\n"
    } else {
        "\n"
    };
    let lines = split_lines_preserving_eol(existing);
    let mut lexer = LexerState::default();
    let mut section = ActiveSection::TopLevel;

    let mut has_default_profile = false;
    let mut first_table_line: Option<usize> = None;
    let mut last_top_level_content_line: Option<usize> = None;

    let mut target_header_line: Option<usize> = None;
    let mut target_last_managed_line: Option<usize> = None;
    let mut managed_spans: HashMap<&'static str, (usize, usize)> = HashMap::new();

    let mut i = 0;
    while i < lines.len() {
        let line = lines[i].text;
        if !lexer.at_statement_boundary() {
            lexer.advance_line(line);
            i += 1;
            continue;
        }

        let trimmed_offset = line.len() - line.trim_start_matches([' ', '\t']).len();
        let trimmed = &line[trimmed_offset..];
        if trimmed.is_empty() || trimmed.starts_with('#') {
            i += 1;
            continue;
        }

        if trimmed.starts_with("[[") {
            if first_table_line.is_none() {
                first_table_line = Some(i);
            }
            let segments = parse_table_header(trimmed, true)?;
            if segments.first().map(String::as_str) == Some("profiles") {
                return Err(ConfigError::UnsupportedProfileLayout);
            }
            section = ActiveSection::Other;
            i += 1;
            continue;
        }

        if trimmed.starts_with('[') {
            if first_table_line.is_none() {
                first_table_line = Some(i);
            }
            let segments = parse_table_header(trimmed, false)?;
            section = if segments.len() == 1 && segments[0] == "profiles" {
                ActiveSection::ProfilesTable
            } else if segments.len() == 2
                && segments[0] == "profiles"
                && segments[1] == input.profile_name
            {
                if target_header_line.is_some() {
                    return Err(ConfigError::UnsupportedProfileLayout);
                }
                target_header_line = Some(i);
                ActiveSection::TargetProfile
            } else if segments.len() >= 3
                && segments[0] == "profiles"
                && segments[1] == input.profile_name
                && !is_known_profile_subtable(&segments[2])
            {
                return Err(ConfigError::UnsupportedProfileLayout);
            } else {
                ActiveSection::Other
            };
            i += 1;
            continue;
        }

        let (lhs, after_eq_rel) = parse_assignment_lhs(trimmed)?;
        let start_line = i;
        lexer.advance_slice(line, trimmed_offset + after_eq_rel);
        while !lexer.at_statement_boundary() && i + 1 < lines.len() {
            i += 1;
            lexer.advance_line(lines[i].text);
        }
        let end_line = i;

        match section {
            ActiveSection::TopLevel => {
                last_top_level_content_line = Some(end_line);
                if lhs.len() == 1 && lhs[0] == "default_profile" {
                    has_default_profile = true;
                } else if lhs.first().map(String::as_str) == Some("profiles") {
                    return Err(ConfigError::UnsupportedProfileLayout);
                }
            }
            ActiveSection::ProfilesTable => {
                return Err(ConfigError::UnsupportedProfileLayout);
            }
            ActiveSection::TargetProfile => {
                if lhs.len() == 1
                    && let Some(key) = managed_key_name(&lhs[0])
                {
                    managed_spans.insert(key, (start_line, end_line));
                    target_last_managed_line = Some(end_line);
                }
            }
            ActiveSection::Other => {}
        }
        i += 1;
    }

    let mut out = String::with_capacity(existing.len() + 256);
    let default_profile_line = if !has_default_profile {
        Some(format!(
            "default_profile = {}",
            toml_string(input.profile_name)
        ))
    } else {
        None
    };

    let top_insert_after = last_top_level_content_line;
    let mut default_inserted = default_profile_line.is_none();

    if let Some(header_line) = target_header_line {
        let mut replacement_by_start: HashMap<usize, Option<String>> = HashMap::new();
        let mut skip_lines = vec![false; lines.len()];
        let mut placed_keys: Vec<&'static str> = Vec::new();

        let desired = desired_key_lines(input);
        for (key, maybe_line) in &desired {
            if let Some(&(start, end)) = managed_spans.get(key) {
                for slot in &mut skip_lines[start..=end] {
                    *slot = true;
                }
                replacement_by_start.insert(start, maybe_line.clone());
                if maybe_line.is_some() {
                    placed_keys.push(key);
                }
            }
        }

        // Remove api_key_env (or replace it with credential if credential was absent).
        if let Some(&(start, end)) = managed_spans.get("api_key_env") {
            for slot in &mut skip_lines[start..=end] {
                *slot = true;
            }
            if !placed_keys.contains(&"credential") {
                let cred_line = desired
                    .iter()
                    .find(|(k, _)| *k == "credential")
                    .and_then(|(_, v)| v.clone());
                replacement_by_start.insert(start, cred_line);
                placed_keys.push("credential");
            } else {
                replacement_by_start.insert(start, None);
            }
        }

        let extra_insert_after = target_last_managed_line.unwrap_or(header_line);
        let missing_lines: Vec<String> = desired
            .into_iter()
            .filter(|(k, v)| v.is_some() && !placed_keys.contains(k))
            .filter_map(|(_, v)| v)
            .collect();

        for (idx, line_piece) in lines.iter().enumerate() {
            if !default_inserted && first_table_line == Some(idx) && top_insert_after.is_none() {
                if let Some(ref line_text) = default_profile_line {
                    out.push_str(line_text);
                    out.push_str(eol);
                    out.push_str(eol);
                }
                default_inserted = true;
            }

            if let Some(maybe_replacement) = replacement_by_start.get(&idx) {
                if let Some(replacement) = maybe_replacement {
                    out.push_str(replacement);
                    out.push_str(eol);
                }
            } else if !skip_lines[idx] {
                out.push_str(line_piece.text);
                if !line_piece.eol.is_empty() {
                    out.push_str(line_piece.eol);
                } else if idx == extra_insert_after && !missing_lines.is_empty() {
                    out.push_str(eol);
                }
            }

            if !default_inserted && top_insert_after == Some(idx) {
                if let Some(ref line_text) = default_profile_line {
                    out.push_str(line_text);
                    out.push_str(eol);
                }
                default_inserted = true;
            }

            if idx == extra_insert_after {
                for missing in &missing_lines {
                    out.push_str(missing);
                    out.push_str(eol);
                }
            }
        }

        if !default_inserted && let Some(ref line_text) = default_profile_line {
            if !out.is_empty() && !out.ends_with('\n') {
                out.push_str(eol);
            }
            out.push_str(line_text);
            out.push_str(eol);
        }
    } else {
        for (idx, line_piece) in lines.iter().enumerate() {
            if !default_inserted && first_table_line == Some(idx) && top_insert_after.is_none() {
                if let Some(ref line_text) = default_profile_line {
                    out.push_str(line_text);
                    out.push_str(eol);
                    out.push_str(eol);
                }
                default_inserted = true;
            }
            out.push_str(line_piece.text);
            out.push_str(line_piece.eol);
            if !default_inserted && top_insert_after == Some(idx) {
                if line_piece.eol.is_empty() {
                    out.push_str(eol);
                }
                if let Some(ref line_text) = default_profile_line {
                    out.push_str(line_text);
                    out.push_str(eol);
                }
                default_inserted = true;
            }
        }
        if !default_inserted && let Some(ref line_text) = default_profile_line {
            if !out.is_empty() && !out.ends_with('\n') {
                out.push_str(eol);
            }
            out.push_str(line_text);
            out.push_str(eol);
        }
        if !out.is_empty() {
            if !out.ends_with('\n') {
                out.push_str(eol);
            }
            if !out.ends_with("\n\n") && !out.ends_with("\r\n\r\n") {
                out.push_str(eol);
            }
        }
        let _ = write!(out, "[profiles.{}]{eol}", input.profile_name);
        for field_line in rendered_profile_fields(input) {
            out.push_str(&field_line);
            out.push_str(eol);
        }
    }

    if !out.ends_with('\n') {
        out.push_str(eol);
    }
    Ok(out)
}

fn desired_key_lines(input: &NormalizedUpsert<'_>) -> Vec<(&'static str, Option<String>)> {
    vec![
        (
            "provider",
            Some(format!(
                "provider = {}",
                toml_string(input.provider.as_str())
            )),
        ),
        (
            "model",
            Some(format!("model = {}", toml_string(input.model))),
        ),
        ("models", format_models_line(&input.models)),
        (
            "credential",
            Some(format!("credential = {}", toml_string(input.credential))),
        ),
        (
            "endpoint",
            input
                .endpoint
                .map(|ep| format!("endpoint = {}", toml_string(ep))),
        ),
        (
            "max_tokens",
            input.max_tokens.map(|mt| format!("max_tokens = {mt}")),
        ),
    ]
}

fn managed_key_name(key: &str) -> Option<&'static str> {
    match key {
        "provider" => Some("provider"),
        "model" => Some("model"),
        "models" => Some("models"),
        "credential" => Some("credential"),
        "api_key_env" => Some("api_key_env"),
        "endpoint" => Some("endpoint"),
        "max_tokens" => Some("max_tokens"),
        _ => None,
    }
}

fn is_known_profile_subtable(name: &str) -> bool {
    matches!(
        name,
        "extra_body"
            | "render_options"
            | "tool_limits"
            | "write_limits"
            | "command"
            | "code_execution"
    )
}

fn parse_table_header(trimmed: &str, array_table: bool) -> Result<Vec<String>, ConfigError> {
    let mut rest = if array_table {
        trimmed
            .strip_prefix("[[")
            .ok_or(ConfigError::UnsupportedProfileLayout)?
    } else {
        trimmed
            .strip_prefix('[')
            .ok_or(ConfigError::UnsupportedProfileLayout)?
    };
    let mut segments = Vec::new();
    loop {
        rest = rest.trim_start_matches([' ', '\t']);
        let (segment, after_seg) = parse_key_segment(rest)?;
        segments.push(segment);
        rest = after_seg.trim_start_matches([' ', '\t']);
        if let Some(after_dot) = rest.strip_prefix('.') {
            rest = after_dot;
            continue;
        }
        let closing = if array_table { "]]" } else { "]" };
        if let Some(after_close) = rest.strip_prefix(closing) {
            let tail = after_close.trim_start_matches([' ', '\t']);
            if tail.is_empty() || tail.starts_with('#') {
                return Ok(segments);
            }
        }
        return Err(ConfigError::UnsupportedProfileLayout);
    }
}

fn parse_assignment_lhs(trimmed: &str) -> Result<(Vec<String>, usize), ConfigError> {
    let total_len = trimmed.len();
    let mut rest = trimmed;
    let mut segments = Vec::new();
    loop {
        rest = rest.trim_start_matches([' ', '\t']);
        let (segment, after_seg) = parse_key_segment(rest)?;
        segments.push(segment);
        rest = after_seg.trim_start_matches([' ', '\t']);
        if let Some(after_dot) = rest.strip_prefix('.') {
            rest = after_dot;
            continue;
        }
        if let Some(after_eq) = rest.strip_prefix('=') {
            let offset = total_len - after_eq.len();
            return Ok((segments, offset));
        }
        return Err(ConfigError::UnsupportedProfileLayout);
    }
}

fn parse_key_segment(input: &str) -> Result<(String, &str), ConfigError> {
    if let Some(after_open) = input.strip_prefix('"') {
        let mut out = String::new();
        let mut chars = after_open.char_indices();
        while let Some((idx, ch)) = chars.next() {
            match ch {
                '\\' => {
                    let (_, esc) = chars.next().ok_or(ConfigError::UnsupportedProfileLayout)?;
                    out.push(match esc {
                        '\\' => '\\',
                        '"' => '"',
                        'n' => '\n',
                        'r' => '\r',
                        't' => '\t',
                        other => other,
                    });
                }
                '"' => return Ok((out, &after_open[idx + 1..])),
                other => out.push(other),
            }
        }
        return Err(ConfigError::UnsupportedProfileLayout);
    }

    if let Some(after_open) = input.strip_prefix('\'') {
        if let Some(end) = after_open.find('\'') {
            return Ok((after_open[..end].to_owned(), &after_open[end + 1..]));
        }
        return Err(ConfigError::UnsupportedProfileLayout);
    }

    let len = input
        .bytes()
        .take_while(|b| b.is_ascii_alphanumeric() || *b == b'_' || *b == b'-')
        .count();
    if len == 0 {
        return Err(ConfigError::UnsupportedProfileLayout);
    }
    Ok((input[..len].to_owned(), &input[len..]))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::definition::{CredentialRef, DefinitionError};

    #[test]
    fn provider_presets_cover_all_seven_providers_and_lookup() {
        let ids: Vec<&str> = PROVIDER_PRESETS.iter().map(|p| p.id).collect();
        assert_eq!(
            ids,
            vec![
                "openai",
                "anthropic",
                "openrouter",
                "deepseek",
                "xai",
                "gemini",
                "ollama",
            ]
        );

        let openai = find_provider_preset("openai").unwrap();
        assert_eq!(openai.provider, Provider::OpenAiResponses);
        assert_eq!(openai.endpoint, None);
        assert_eq!(openai.suggested_model, "gpt-5.4");
        assert_eq!(openai.default_max_tokens, None);
        assert!(openai.requires_api_key);

        let anthropic = find_provider_preset("anthropic").unwrap();
        assert_eq!(anthropic.provider, Provider::Anthropic);
        assert_eq!(anthropic.endpoint, None);
        assert_eq!(anthropic.suggested_model, "claude-sonnet-4-6");
        assert_eq!(anthropic.default_max_tokens, Some(8192));
        assert!(anthropic.requires_api_key);

        let ollama = find_provider_preset("ollama").unwrap();
        assert_eq!(ollama.provider, Provider::OpenAiChat);
        assert_eq!(
            ollama.endpoint,
            Some("http://127.0.0.1:11434/v1/chat/completions")
        );
        assert_eq!(ollama.suggested_model, "qwen3:8b");
        assert!(!ollama.requires_api_key);

        assert!(find_provider_preset("custom").is_none());
    }

    #[test]
    fn upsert_profile_in_config_creates_new_config_with_credential_reference_only() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("nested").join("config.toml");

        let config = upsert_profile_in_config(
            &config_path,
            &ProfileUpsertInput {
                profile_name: "openai".into(),
                provider: Provider::OpenAiResponses,
                model: "gpt-5.4".into(),
                models: Vec::new(),
                credential: "openai".into(),
                endpoint: Some("   ".into()),
                max_tokens: None,
            },
        )
        .unwrap();

        let def = config.preset(None).unwrap().unwrap();
        assert_eq!(def.provider, Provider::OpenAiResponses);
        assert_eq!(def.model, "gpt-5.4");
        assert_eq!(def.endpoint, None);
        assert_eq!(def.credential, CredentialRef::Store("openai".into()));

        let text = std::fs::read_to_string(&config_path).unwrap();
        assert!(text.contains("default_profile = \"openai\""));
        assert!(text.contains("[profiles.openai]"));
        assert!(text.contains("credential = \"openai\""));
        assert!(!text.contains("api_key"));
    }

    #[test]
    fn upsert_profile_in_config_preserves_comments_multiline_strings_and_subtables() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("config.toml");
        let initial = r#"# Top-level comment
version = 1
store = "data"
recovery_dir = "."

[prelude]
# Keep fake profile header inside multiline string intact
system_prompt = """
[profiles.openai]
provider = "anthropic"
model = "decoy-inside-prelude"
"""

[profiles.openai]
# Keep profile comment
provider = "openai-chat"
model = "gpt-4o"
api_key_env = "OPENAI_API_KEY"
endpoint = "https://legacy.example.invalid/v1"
temperature = 0.5
tools = [
  # keep array formatting
]

[profiles.openai.extra_body]
reasoning = "high"

[profiles.other]
provider = "anthropic"
model = "claude-sonnet-4-6"
credential = "anthropic"
max_tokens = 8192
"#;
        std::fs::write(&config_path, initial).unwrap();

        let config = upsert_profile_in_config(
            &config_path,
            &ProfileUpsertInput {
                profile_name: "openai".into(),
                provider: Provider::OpenAiResponses,
                model: "gpt-5.4".into(),
                models: Vec::new(),
                credential: "openai".into(),
                endpoint: None,
                max_tokens: None,
            },
        )
        .unwrap();

        let updated = std::fs::read_to_string(&config_path).unwrap();
        assert!(updated.contains("# Top-level comment"));
        assert!(updated.contains("default_profile = \"openai\""));
        assert!(updated.contains("model = \"decoy-inside-prelude\""));
        assert!(updated.contains("# Keep profile comment"));
        assert!(updated.contains("temperature = 0.5"));
        assert!(updated.contains("[profiles.openai.extra_body]\nreasoning = \"high\""));
        assert!(updated.contains("[profiles.other]"));
        assert!(!updated.contains("api_key_env"));
        assert!(!updated.contains("https://legacy.example.invalid/v1"));

        let def = config.preset(Some("openai")).unwrap().unwrap();
        assert_eq!(def.provider, Provider::OpenAiResponses);
        assert_eq!(def.model, "gpt-5.4");
        assert_eq!(def.endpoint, None);
        assert_eq!(def.credential, CredentialRef::Store("openai".into()));
        assert_eq!(def.temperature.map(|t| t.as_f32()), Some(0.5));
        assert_eq!(
            def.extra_body.get("reasoning").and_then(|v| v.as_str()),
            Some("high")
        );
    }

    #[test]
    fn upsert_profile_in_config_appends_profile_and_validates_with_sibling_mcp_json() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("config.toml");
        let mcp_path = dir.path().join("mcp.json");

        std::fs::write(
            &mcp_path,
            r#"{
  "mcpServers": {
    "echo": {
      "command": "echo",
      "args": ["ok"]
    }
  }
}"#,
        )
        .unwrap();

        std::fs::write(
            &config_path,
            r#"version = 1
default_profile = "base"

[profiles.base]
provider = "openai-chat"
model = "gpt-4o"
credential = "base"
mcp = ["echo"]
"#,
        )
        .unwrap();

        let config = upsert_profile_in_config(
            &config_path,
            &ProfileUpsertInput {
                profile_name: "deepseek".into(),
                provider: Provider::OpenAiChat,
                model: "deepseek-chat".into(),
                models: Vec::new(),
                credential: "deepseek".into(),
                endpoint: Some("https://api.deepseek.com/v1/chat/completions".into()),
                max_tokens: None,
            },
        )
        .unwrap();

        let deepseek = config.preset(Some("deepseek")).unwrap().unwrap();
        assert_eq!(deepseek.credential, CredentialRef::Store("deepseek".into()));
        assert_eq!(
            deepseek.endpoint.as_deref(),
            Some("https://api.deepseek.com/v1/chat/completions")
        );
        // Existing default_profile must stay "base".
        let base = config.preset(None).unwrap().unwrap();
        assert_eq!(base.mcp_servers.len(), 1);
        assert_eq!(base.mcp_servers[0].id, "echo");

        // If an existing profile references an unknown MCP server, upsert must fail
        // without corrupting config.toml.
        let broken_toml = r#"version = 1
default_profile = "base"

[profiles.base]
provider = "openai-chat"
model = "gpt-4o"
credential = "base"
mcp = ["missing_server"]
"#;
        std::fs::write(&config_path, broken_toml).unwrap();
        let err = upsert_profile_in_config(
            &config_path,
            &ProfileUpsertInput {
                profile_name: "deepseek".into(),
                provider: Provider::OpenAiChat,
                model: "deepseek-chat".into(),
                models: Vec::new(),
                credential: "deepseek".into(),
                endpoint: None,
                max_tokens: None,
            },
        )
        .unwrap_err();
        assert!(matches!(err, ConfigError::UnknownMcpServer(ref id) if id == "missing_server"));
        assert_eq!(std::fs::read_to_string(&config_path).unwrap(), broken_toml);
    }

    #[test]
    fn upsert_profile_in_config_rejects_invalid_inputs_and_unsupported_layouts() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("config.toml");

        let valid_input = ProfileUpsertInput {
            profile_name: "openai".into(),
            provider: Provider::OpenAiResponses,
            model: "gpt-5.4".into(),
            models: Vec::new(),
            credential: "openai".into(),
            endpoint: None,
            max_tokens: None,
        };

        // Validation checks before disk writes
        assert!(matches!(
            upsert_profile_in_config(
                &config_path,
                &ProfileUpsertInput {
                    profile_name: "bad.name".into(),
                    ..valid_input.clone()
                },
            )
            .unwrap_err(),
            ConfigError::InvalidProfileName
        ));
        assert!(matches!(
            upsert_profile_in_config(
                &config_path,
                &ProfileUpsertInput {
                    credential: "bad.cred".into(),
                    ..valid_input.clone()
                },
            )
            .unwrap_err(),
            ConfigError::Definition(DefinitionError::InvalidCredentialReference)
        ));
        assert!(matches!(
            upsert_profile_in_config(
                &config_path,
                &ProfileUpsertInput {
                    provider: Provider::Anthropic,
                    max_tokens: None,
                    ..valid_input.clone()
                },
            )
            .unwrap_err(),
            ConfigError::AnthropicRequiresMaxTokens
        ));

        for unsupported in [
            "version = 1\nprofiles = { openai = { provider = \"openai-chat\", model = \"m\" } }\n",
            "version = 1\n[profiles]\nopenai = { provider = \"openai-chat\", model = \"m\" }\n",
            "version = 1\n[[profiles]]\nprovider = \"openai-chat\"\nmodel = \"m\"\n",
        ] {
            std::fs::write(&config_path, unsupported).unwrap();
            let err = upsert_profile_in_config(&config_path, &valid_input).unwrap_err();
            assert!(
                matches!(err, ConfigError::UnsupportedProfileLayout),
                "expected UnsupportedProfileLayout for {unsupported:?}, got {err:?}"
            );
            assert_eq!(std::fs::read_to_string(&config_path).unwrap(), unsupported);
        }

        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;
            let real = dir.path().join("real.toml");
            std::fs::write(&real, "version = 1\n").unwrap();
            let live_link = dir.path().join("live_config.toml");
            symlink(&real, &live_link).unwrap();
            assert!(matches!(
                upsert_profile_in_config(&live_link, &valid_input).unwrap_err(),
                ConfigError::SymlinkNotAllowed(_)
            ));
            assert_eq!(std::fs::read_to_string(&real).unwrap(), "version = 1\n");

            let dangling_link = dir.path().join("dangling_config.toml");
            symlink(dir.path().join("absent.toml"), &dangling_link).unwrap();
            assert!(matches!(
                upsert_profile_in_config(&dangling_link, &valid_input).unwrap_err(),
                ConfigError::SymlinkNotAllowed(_)
            ));
            assert!(!dir.path().join("absent.toml").exists());
        }
    }

    #[test]
    fn upsert_profile_in_config_writes_multi_model_and_cleans_stale_multiline_models() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("config.toml");
        std::fs::write(
            &config_path,
            r#"# Keep top comment
version = 1
default_profile = "openrouter"

[profiles.openrouter]
# Keep profile comment
provider = "openai-chat"
model = "old-model"
models = [
  "old-model",
  "legacy-a",
]
credential = "openrouter"
temperature = 0.2
"#,
        )
        .unwrap();

        // Update with >= 2 unique models: replaces multiline models, puts primary first, deduplicates.
        let cfg_multi = upsert_profile_in_config(
            &config_path,
            &ProfileUpsertInput {
                profile_name: "openrouter".into(),
                provider: Provider::OpenAiChat,
                model: "anthropic/claude-sonnet-4.6".into(),
                models: vec![
                    "openai/gpt-5.4".into(),
                    "anthropic/claude-sonnet-4.6".into(),
                    " openai/gpt-5.4 ".into(),
                    "google/gemini-2.5-pro".into(),
                ],
                credential: "openrouter".into(),
                endpoint: Some("https://openrouter.ai/api/v1/chat/completions".into()),
                max_tokens: None,
            },
        )
        .unwrap();
        let text_multi = std::fs::read_to_string(&config_path).unwrap();
        assert!(text_multi.contains("# Keep top comment"));
        assert!(text_multi.contains("# Keep profile comment"));
        assert!(text_multi.contains("temperature = 0.2"));
        assert!(text_multi.contains(
            r#"models = ["anthropic/claude-sonnet-4.6", "openai/gpt-5.4", "google/gemini-2.5-pro"]"#
        ));
        assert!(!text_multi.contains("legacy-a"));
        assert_eq!(
            cfg_multi.model_profiles()["openrouter"].available_models(),
            vec![
                "anthropic/claude-sonnet-4.6",
                "openai/gpt-5.4",
                "google/gemini-2.5-pro",
            ]
        );

        // Update back to single model (only primary in models): omits models and strips old line.
        let cfg_single = upsert_profile_in_config(
            &config_path,
            &ProfileUpsertInput {
                profile_name: "openrouter".into(),
                provider: Provider::OpenAiChat,
                model: "openai/gpt-5.4".into(),
                models: vec!["openai/gpt-5.4".into()],
                credential: "openrouter".into(),
                endpoint: Some("https://openrouter.ai/api/v1/chat/completions".into()),
                max_tokens: None,
            },
        )
        .unwrap();
        let text_single = std::fs::read_to_string(&config_path).unwrap();
        assert!(text_single.contains("# Keep profile comment"));
        assert!(text_single.contains("temperature = 0.2"));
        assert!(!text_single.contains("models ="));
        assert_eq!(
            cfg_single.model_profiles()["openrouter"].available_models(),
            vec!["openai/gpt-5.4"]
        );

        // Multiline models removal on update to empty models list.
        std::fs::write(
            &config_path,
            r#"version = 1
default_profile = "openrouter"

[profiles.openrouter]
provider = "openai-chat"
model = "m1"
credential = "openrouter"
models = [
  "m1",
  "m2",
]
"#,
        )
        .unwrap();
        let cfg_cleaned = upsert_profile_in_config(
            &config_path,
            &ProfileUpsertInput {
                profile_name: "openrouter".into(),
                provider: Provider::OpenAiChat,
                model: "m1".into(),
                models: Vec::new(),
                credential: "openrouter".into(),
                endpoint: None,
                max_tokens: None,
            },
        )
        .unwrap();
        let cleaned_text = std::fs::read_to_string(&config_path).unwrap();
        assert!(!cleaned_text.contains("models"));
        assert!(!cleaned_text.contains("m2"));
        assert_eq!(
            cfg_cleaned.model_profiles()["openrouter"].available_models(),
            vec!["m1"]
        );

        // Validation rejects blank or control characters in input.models before touching disk.
        for bad in ["", "   ", "bad\nmodel"] {
            let err = upsert_profile_in_config(
                &config_path,
                &ProfileUpsertInput {
                    profile_name: "openrouter".into(),
                    provider: Provider::OpenAiChat,
                    model: "m1".into(),
                    models: vec![bad.into()],
                    credential: "openrouter".into(),
                    endpoint: None,
                    max_tokens: None,
                },
            )
            .unwrap_err();
            assert!(matches!(err, ConfigError::InvalidProfileModels(ref p) if p == "openrouter"));
        }
    }
}
