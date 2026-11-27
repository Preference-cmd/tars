use super::{
    App,
    commands::Panel,
    jobs::{JobKind, Message, blocking},
};
use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tars_core::config::{
    Config, CredentialsStore, PROVIDER_PRESETS, ProfileUpsertInput, default_config_path,
    upsert_profile_in_config,
};
use tars_core::definition::{Provider, valid_credential_name};

/// Cached read-only state of `credentials.toml` entry names (never secret values).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) enum CredentialStoreCache {
    /// Not yet loaded from disk.
    #[default]
    Unknown,
    /// Credentials store disabled (e.g., `--no-config` or unresolvable home dir).
    Unavailable,
    /// Successfully loaded sorted entry names without secret values.
    Loaded(Vec<String>),
    /// Store exists but failed validation (symlink, insecure permissions, syntax, or size).
    Error,
}

impl CredentialStoreCache {
    pub(crate) fn contains(&self, name: &str) -> bool {
        match self {
            Self::Loaded(names) => names.iter().any(|entry| entry == name),
            Self::Unknown | Self::Unavailable | Self::Error => false,
        }
    }
}

/// Active step of [`Panel::Provider`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ProviderWizardStep {
    /// Step 1: pick one of the 7 built-in presets or Custom.
    SelectPreset,
    /// Step 2: configure profile name, model, masked API key, and optional endpoint/max_tokens.
    Configure,
}

/// Editable/selectable field kind in [`ProviderWizardStep::Configure`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ProviderFieldKind {
    Protocol,
    Endpoint,
    ProfileName,
    Model,
    ApiKey,
    MaxTokens,
}

/// State for the two-step `/provider` preset & API key wizard.
pub(crate) struct ProviderWizardState {
    pub(crate) step: ProviderWizardStep,
    pub(crate) selected_preset: usize,
    pub(crate) is_custom: bool,
    pub(crate) preset_label: String,
    pub(crate) preset_endpoint: Option<String>,
    pub(crate) preset_has_max_tokens: bool,
    pub(crate) provider: Provider,
    pub(crate) profile_name: String,
    pub(crate) model: String,
    pub(crate) endpoint: String,
    pub(crate) api_key: String,
    pub(crate) ollama_default_hint: bool,
    pub(crate) max_tokens: String,
    pub(crate) auto_max_tokens: bool,
    pub(crate) active_field: usize,
    pub(crate) saving: bool,
    pub(crate) error: String,
}

impl Drop for ProviderWizardState {
    fn drop(&mut self) {
        clear_secret(&mut self.api_key);
    }
}

impl ProviderWizardState {
    pub(crate) fn new() -> Self {
        Self {
            step: ProviderWizardStep::SelectPreset,
            selected_preset: 0,
            is_custom: false,
            preset_label: String::new(),
            preset_endpoint: None,
            preset_has_max_tokens: false,
            provider: Provider::OpenAiChat,
            profile_name: String::new(),
            model: String::new(),
            endpoint: String::new(),
            api_key: String::new(),
            ollama_default_hint: false,
            max_tokens: String::new(),
            auto_max_tokens: false,
            active_field: 0,
            saving: false,
            error: String::new(),
        }
    }

    pub(crate) fn clear_secret(&mut self) {
        clear_secret(&mut self.api_key);
        self.ollama_default_hint = false;
    }

    pub(crate) fn enter_step_2(&mut self) {
        self.clear_secret();
        self.error.clear();
        self.saving = false;
        self.active_field = 0;
        self.auto_max_tokens = false;

        if let Some(preset) = PROVIDER_PRESETS.get(self.selected_preset) {
            self.is_custom = false;
            self.preset_label = preset.label.to_owned();
            self.preset_endpoint = preset.endpoint.map(str::to_owned);
            self.preset_has_max_tokens = preset.default_max_tokens.is_some();
            self.provider = preset.provider;
            self.profile_name = preset.id.to_owned();
            self.model = preset.suggested_model.to_owned();
            self.endpoint = preset.endpoint.unwrap_or_default().to_owned();
            self.max_tokens = preset
                .default_max_tokens
                .map(|v| v.to_string())
                .unwrap_or_default();
            if preset.id == "ollama" {
                self.api_key = "ollama".to_owned();
                self.ollama_default_hint = true;
            }
            // Start focus on API Key (index 2) so users can paste immediately, or Model if ollama.
            self.active_field = if preset.id == "ollama" { 1 } else { 2 };
        } else {
            self.is_custom = true;
            self.preset_label = "Custom provider".to_owned();
            self.preset_endpoint = None;
            self.preset_has_max_tokens = false;
            self.provider = Provider::OpenAiChat;
            self.profile_name = "custom".to_owned();
            self.model.clear();
            self.endpoint.clear();
            self.max_tokens.clear();
            self.active_field = 0;
        }
        self.step = ProviderWizardStep::Configure;
    }

    pub(crate) fn back_to_step_1(&mut self) {
        self.clear_secret();
        self.error.clear();
        self.saving = false;
        self.step = ProviderWizardStep::SelectPreset;
    }

    pub(crate) fn fields(&self) -> Vec<ProviderFieldKind> {
        if self.is_custom {
            vec![
                ProviderFieldKind::Protocol,
                ProviderFieldKind::Endpoint,
                ProviderFieldKind::ProfileName,
                ProviderFieldKind::Model,
                ProviderFieldKind::ApiKey,
                ProviderFieldKind::MaxTokens,
            ]
        } else if self.provider == Provider::Anthropic || self.preset_has_max_tokens {
            vec![
                ProviderFieldKind::ProfileName,
                ProviderFieldKind::Model,
                ProviderFieldKind::ApiKey,
                ProviderFieldKind::MaxTokens,
            ]
        } else {
            vec![
                ProviderFieldKind::ProfileName,
                ProviderFieldKind::Model,
                ProviderFieldKind::ApiKey,
            ]
        }
    }

    pub(crate) fn current_field(&self) -> Option<ProviderFieldKind> {
        if self.step != ProviderWizardStep::Configure {
            return None;
        }
        let fields = self.fields();
        fields
            .get(self.active_field.min(fields.len().saturating_sub(1)))
            .copied()
    }

    pub(crate) fn masked_api_key(&self) -> String {
        "•".repeat(self.api_key.chars().count())
    }

    pub(crate) fn cycle_protocol(&mut self, forward: bool) {
        const PROTOCOLS: [Provider; 3] = [
            Provider::OpenAiChat,
            Provider::OpenAiResponses,
            Provider::Anthropic,
        ];
        let idx = PROTOCOLS
            .iter()
            .position(|&p| p == self.provider)
            .unwrap_or(0);
        let next_idx = if forward {
            (idx + 1) % PROTOCOLS.len()
        } else {
            (idx + PROTOCOLS.len() - 1) % PROTOCOLS.len()
        };
        let next = PROTOCOLS[next_idx];
        self.provider = next;
        if next == Provider::Anthropic {
            if self.max_tokens.trim().is_empty() {
                self.max_tokens = "8192".to_owned();
                self.auto_max_tokens = true;
            }
        } else if self.auto_max_tokens {
            self.max_tokens.clear();
            self.auto_max_tokens = false;
        }
    }

    pub(crate) fn paste_single_line(&mut self, text: &str) {
        if self.step != ProviderWizardStep::Configure || self.saving {
            return;
        }
        let Some(field) = self.current_field() else {
            return;
        };
        if field == ProviderFieldKind::Protocol {
            return;
        }
        let sanitized: String = text.chars().filter(|c| !c.is_control()).collect();
        if sanitized.is_empty() {
            return;
        }
        match field {
            ProviderFieldKind::Protocol => {}
            ProviderFieldKind::Endpoint => self.endpoint.push_str(&sanitized),
            ProviderFieldKind::ProfileName => self.profile_name.push_str(&sanitized),
            ProviderFieldKind::Model => self.model.push_str(&sanitized),
            ProviderFieldKind::ApiKey => {
                self.api_key.push_str(&sanitized);
                self.ollama_default_hint = false;
            }
            ProviderFieldKind::MaxTokens => {
                self.max_tokens.push_str(&sanitized);
                self.auto_max_tokens = false;
            }
        }
    }
}

/// Overwrite heap capacity with NUL bytes before dropping/clearing a secret buffer.
pub(crate) fn clear_secret(secret: &mut String) {
    let cap = secret.capacity();
    secret.clear();
    if cap > 0 {
        secret.extend(std::iter::repeat_n('\0', cap));
        secret.clear();
        secret.shrink_to_fit();
    }
}

/// Wrapper ensuring a moved secret string is wiped on drop even on error or panic.
struct SecretBox(String);

impl Drop for SecretBox {
    fn drop(&mut self) {
        clear_secret(&mut self.0);
    }
}

/// Result delivered by [`JobKind::SaveProvider`] to [`App::complete`].
pub(crate) struct ProviderSavedState {
    pub(crate) config_path: PathBuf,
    pub(crate) credentials_path: PathBuf,
    pub(crate) profile_name: String,
    pub(crate) config: Config,
    pub(crate) credential_cache: CredentialStoreCache,
}

pub(crate) fn load_credential_cache(path: &Path) -> CredentialStoreCache {
    match CredentialsStore::load_optional(path) {
        Ok(Some(store)) => CredentialStoreCache::Loaded(store.list_names()),
        Ok(None) => CredentialStoreCache::Loaded(Vec::new()),
        Err(_) => CredentialStoreCache::Error,
    }
}

impl App {
    pub(crate) fn refresh_cached_credential_names(&mut self) {
        let Some(path) = self.credentials_path.clone() else {
            self.credential_store_cache = CredentialStoreCache::Unavailable;
            return;
        };
        if tokio::runtime::Handle::try_current().is_err() || !self.capacity() {
            return;
        }
        self.spawn_job(None, JobKind::LoadCredentialNames, async move {
            let cache = blocking(move || load_credential_cache(&path))
                .await
                .unwrap_or(CredentialStoreCache::Error);
            Message::CredentialNamesLoaded(cache)
        });
    }

    pub(crate) fn open_provider_wizard(&mut self) {
        if self.options.no_config {
            self.notice = "Disabled under --no-config; use /new or omit --no-config".into();
            return;
        }
        self.panel = Some(Panel::Provider(Box::new(ProviderWizardState::new())));
        self.refresh_cached_credential_names();
    }

    pub(super) fn provider_key(&mut self, key: KeyEvent) {
        let Some(Panel::Provider(mut wizard)) = self.panel.take() else {
            return;
        };
        if key.kind != KeyEventKind::Press {
            self.panel = Some(Panel::Provider(wizard));
            return;
        }
        if key.code == KeyCode::Esc {
            match wizard.step {
                ProviderWizardStep::SelectPreset => {
                    wizard.clear_secret();
                    return;
                }
                ProviderWizardStep::Configure => {
                    if wizard.saving {
                        self.panel = Some(Panel::Provider(wizard));
                        return;
                    }
                    wizard.back_to_step_1();
                    self.panel = Some(Panel::Provider(wizard));
                    return;
                }
            }
        }
        if wizard.saving {
            self.panel = Some(Panel::Provider(wizard));
            return;
        }

        let no_ctrl_alt = !key
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT);

        match wizard.step {
            ProviderWizardStep::SelectPreset => {
                let total = PROVIDER_PRESETS.len() + 1;
                match key.code {
                    KeyCode::Up => {
                        wizard.selected_preset = wizard.selected_preset.saturating_sub(1);
                    }
                    KeyCode::Down => {
                        wizard.selected_preset = (wizard.selected_preset + 1).min(total - 1);
                    }
                    KeyCode::Char('k' | 'K') if no_ctrl_alt => {
                        wizard.selected_preset = wizard.selected_preset.saturating_sub(1);
                    }
                    KeyCode::Char('j' | 'J') if no_ctrl_alt => {
                        wizard.selected_preset = (wizard.selected_preset + 1).min(total - 1);
                    }
                    KeyCode::Enter => {
                        wizard.enter_step_2();
                    }
                    _ => {}
                }
                self.panel = Some(Panel::Provider(wizard));
            }
            ProviderWizardStep::Configure => {
                if super::super::input::control(key, 's')
                    || (key.code == KeyCode::Enter && no_ctrl_alt)
                {
                    self.panel = Some(Panel::Provider(wizard));
                    self.submit_provider_wizard();
                    return;
                }
                let fields = wizard.fields();
                let count = fields.len();
                wizard.active_field = wizard.active_field.min(count.saturating_sub(1));
                let active_kind = fields[wizard.active_field];

                match key.code {
                    KeyCode::Tab | KeyCode::Down => {
                        wizard.active_field = (wizard.active_field + 1) % count;
                    }
                    KeyCode::BackTab | KeyCode::Up => {
                        wizard.active_field = (wizard.active_field + count - 1) % count;
                    }
                    KeyCode::Left if active_kind == ProviderFieldKind::Protocol && no_ctrl_alt => {
                        wizard.cycle_protocol(false);
                    }
                    KeyCode::Right | KeyCode::Char(' ')
                        if active_kind == ProviderFieldKind::Protocol && no_ctrl_alt =>
                    {
                        wizard.cycle_protocol(true);
                    }
                    KeyCode::Backspace if active_kind != ProviderFieldKind::Protocol => {
                        match active_kind {
                            ProviderFieldKind::Protocol => {}
                            ProviderFieldKind::Endpoint => {
                                wizard.endpoint.pop();
                            }
                            ProviderFieldKind::ProfileName => {
                                wizard.profile_name.pop();
                            }
                            ProviderFieldKind::Model => {
                                wizard.model.pop();
                            }
                            ProviderFieldKind::ApiKey => {
                                wizard.api_key.pop();
                                wizard.ollama_default_hint = false;
                            }
                            ProviderFieldKind::MaxTokens => {
                                wizard.max_tokens.pop();
                                wizard.auto_max_tokens = false;
                            }
                        }
                    }
                    KeyCode::Char(c)
                        if active_kind != ProviderFieldKind::Protocol
                            && no_ctrl_alt
                            && !c.is_control() =>
                    {
                        match active_kind {
                            ProviderFieldKind::Protocol => {}
                            ProviderFieldKind::Endpoint => wizard.endpoint.push(c),
                            ProviderFieldKind::ProfileName => wizard.profile_name.push(c),
                            ProviderFieldKind::Model => wizard.model.push(c),
                            ProviderFieldKind::ApiKey => {
                                wizard.api_key.push(c);
                                wizard.ollama_default_hint = false;
                            }
                            ProviderFieldKind::MaxTokens => {
                                wizard.max_tokens.push(c);
                                wizard.auto_max_tokens = false;
                            }
                        }
                    }
                    _ => {}
                }
                self.panel = Some(Panel::Provider(wizard));
            }
        }
    }

    pub(super) fn submit_provider_wizard(&mut self) {
        if self.options.no_config {
            if let Some(Panel::Provider(wizard)) = self.panel.as_mut() {
                wizard.clear_secret();
                wizard.error = "Disabled under --no-config; use /new or omit --no-config".into();
            }
            self.notice = "Disabled under --no-config; use /new or omit --no-config".into();
            return;
        }
        if !self.capacity() {
            return;
        }
        let existing_names = self.credential_store_cache.clone();
        let target_config_path = self
            .options
            .config_path
            .clone()
            .or_else(default_config_path);
        let Some(Panel::Provider(wizard)) = self.panel.as_mut() else {
            return;
        };
        if wizard.saving {
            return;
        }
        let Some(config_path) = target_config_path else {
            wizard.error = "Cannot resolve config.toml path; pass --config FILE".into();
            return;
        };
        let Some(credentials_path) =
            tars_core::config::credentials_path_for_config(Some(&config_path))
        else {
            wizard.error = "Cannot resolve credentials.toml path".into();
            return;
        };

        let profile_name = wizard.profile_name.trim().to_owned();
        if !valid_credential_name(&profile_name) {
            wizard.error =
                "Profile name must be 1–64 ASCII letters/digits/'_'/'-' starting with letter/digit"
                    .into();
            return;
        }
        let model = wizard.model.trim().to_owned();
        if model.is_empty() || model.chars().any(char::is_control) {
            wizard.error = "Model identifier must not be blank".into();
            return;
        }

        let endpoint_trimmed = wizard.endpoint.trim();
        let endpoint = if endpoint_trimmed.is_empty() {
            None
        } else {
            if endpoint_trimmed.chars().any(char::is_whitespace)
                || endpoint_trimmed.chars().any(char::is_control)
                || (!endpoint_trimmed.starts_with("http://")
                    && !endpoint_trimmed.starts_with("https://"))
            {
                wizard.error = "Endpoint must be a valid http:// or https:// URL".into();
                return;
            }
            Some(endpoint_trimmed.to_owned())
        };

        let max_tokens_trimmed = wizard.max_tokens.trim();
        let max_tokens = if max_tokens_trimmed.is_empty() {
            None
        } else {
            match max_tokens_trimmed.parse::<u32>().ok().filter(|v| *v > 0) {
                Some(v) => Some(v),
                None => {
                    wizard.error = "Max tokens must be a positive integer".into();
                    return;
                }
            }
        };
        if wizard.provider == Provider::Anthropic && max_tokens.is_none() {
            wizard.error = "Anthropic requires max_tokens (e.g. 8192)".into();
            return;
        }

        let trimmed_key = wizard.api_key.trim().to_owned();
        if trimmed_key.chars().any(char::is_control) {
            wizard.error = "API key must not contain control characters".into();
            return;
        }
        let has_existing_cred = existing_names.contains(&profile_name);
        if trimmed_key.is_empty() && !has_existing_cred {
            wizard.error = "API key is required".into();
            return;
        }

        let secret_box = (!trimmed_key.is_empty()).then_some(SecretBox(trimmed_key));
        wizard.clear_secret();
        wizard.saving = true;
        wizard.error.clear();

        let input = ProfileUpsertInput {
            profile_name: profile_name.clone(),
            provider: wizard.provider,
            model,
            credential: profile_name.clone(),
            endpoint,
            max_tokens,
        };

        self.spawn_job(None, JobKind::SaveProvider, async move {
            let res = blocking(move || -> Result<ProviderSavedState, String> {
                if let Some(secret) = secret_box {
                    CredentialsStore::set(&credentials_path, &profile_name, &secret.0)
                        .map_err(|e| e.to_string())?;
                }
                let config =
                    upsert_profile_in_config(&config_path, &input).map_err(|e| e.to_string())?;
                let credential_cache = load_credential_cache(&credentials_path);
                Ok(ProviderSavedState {
                    config_path,
                    credentials_path,
                    profile_name,
                    config,
                    credential_cache,
                })
            })
            .await
            .map_err(|e| e.to_string())
            .and_then(|inner| inner);
            Message::ProviderSaved(res)
        });
    }

    pub(super) fn provider_saved(&mut self, result: Result<ProviderSavedState, String>) {
        match result {
            Ok(saved) => {
                self.options.config = Arc::new(saved.config);
                self.options.config_path = Some(saved.config_path.clone());
                self.options.profile = Some(saved.profile_name.clone());
                self.options.preset = match (
                    self.options.config.preset(Some(&saved.profile_name)),
                    self.options.code_execution_enabled,
                ) {
                    (Ok(Some(_)), Some(_)) => self
                        .options
                        .config
                        .definition(
                            Some(&saved.profile_name),
                            tars_core::config::DefinitionOverrides {
                                code_execution_enabled: self.options.code_execution_enabled,
                                ..Default::default()
                            },
                        )
                        .ok(),
                    (Ok(preset), _) => preset,
                    (Err(_), _) => None,
                };
                self.options.credentials_path = Some(saved.credentials_path.clone());
                self.credentials_path = Some(saved.credentials_path.clone());
                self.user_rules_path = self
                    .options
                    .config
                    .config_dir()
                    .map(|dir| dir.join("permissions.toml"));
                self.credential_store_cache = saved.credential_cache;

                // Update runtime config in place so existing open entries in
                // HostInner.entries remain registered and saveable.
                self.host.update_runtime_config(
                    self.credentials_path.clone(),
                    self.user_rules_path.clone(),
                    self.options.config.model_profiles(),
                );

                if matches!(self.panel, Some(Panel::Provider(_))) {
                    self.panel = None;
                }

                let has_open_or_busy = self
                    .entries
                    .iter()
                    .any(|e| e.session.is_some() || e.busy.is_some());
                let trust_blocks_autostart = self.folder_trust.is_some()
                    || self.invalid_startup_workspace
                    || (self.options.check_folder_trust
                        && self.startup_workspace_root().is_some()
                        && self.trusted_workspace.is_none());

                if !has_open_or_busy
                    && !trust_blocks_autostart
                    && !self.quitting
                    && self.confirmation.is_none()
                    && self.form.is_none()
                    && let Some(preset) = self.options.preset.clone()
                {
                    let workspace_root = self
                        .trusted_workspace
                        .clone()
                        .or_else(|| self.options.workspace.clone())
                        .or_else(|| preset.workspace_root.clone())
                        .or_else(|| {
                            (!self.options.check_folder_trust)
                                .then(|| std::env::current_dir().ok())
                                .flatten()
                        });
                    let tools = if preset.tools.is_empty() && workspace_root.is_some() {
                        Some(self.default_workspace_tools())
                    } else {
                        None
                    };
                    match self.resolve_preset_definition(
                        self.options.profile.as_deref(),
                        &preset,
                        workspace_root,
                        tools,
                    ) {
                        Ok(definition) => {
                            if let Err(err) = self.prepare_and_open_definition(definition) {
                                self.notice = format!(
                                    "Saved provider {} · could not auto-open session: {} · run /new",
                                    crate::transcript::display_single_line(&saved.profile_name),
                                    crate::transcript::display_single_line(&err),
                                );
                            } else {
                                self.notice = format!(
                                    "Saved provider {} · started session",
                                    crate::transcript::display_single_line(&saved.profile_name),
                                );
                            }
                        }
                        Err(err) => {
                            self.notice = format!(
                                "Saved provider {} · could not auto-open session: {} · run /new",
                                crate::transcript::display_single_line(&saved.profile_name),
                                crate::transcript::display_single_line(&err),
                            );
                        }
                    }
                } else {
                    self.notice = format!(
                        "Saved provider {} · ready for /new or /model",
                        crate::transcript::display_single_line(&saved.profile_name),
                    );
                }
            }
            Err(err) => {
                let escaped = tars_core::permission::escape_untrusted_text(&err);
                if let Some(Panel::Provider(wizard)) = self.panel.as_mut() {
                    wizard.saving = false;
                    wizard.error = escaped;
                } else {
                    self.notice = format!("Failed to save provider: {escaped}");
                }
            }
        }
    }
}
