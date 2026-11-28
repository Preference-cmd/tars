use super::{
    App,
    commands::Panel,
    jobs::{JobKind, Message, blocking},
};
use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tars_core::config::{
    Config, CredentialsStore, DiscoveredModel, PROVIDER_PRESETS, ProfileUpsertInput,
    default_config_path, derive_models_url, fetch_available_models, upsert_profile_in_config,
};
use tars_core::definition::{CredentialRef, Provider, valid_credential_name};

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
    /// Step 2: configure connection, profile name, masked API key, and optional max_tokens.
    Configure,
    /// Step 3: discover, filter, and multi-select models from the endpoint `/models` API.
    SelectModels,
}

/// Editable/selectable field kind in [`ProviderWizardStep::Configure`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ProviderFieldKind {
    Protocol,
    Endpoint,
    ProfileName,
    ApiKey,
    MaxTokens,
}

/// Asynchronous `/models` fetch status in [`ProviderWizardStep::SelectModels`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) enum ModelFetchStatus {
    #[default]
    Idle,
    Fetching,
    Loaded,
    Failed(String),
}

/// One selectable row in Step 3 (`SelectModels`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ModelCandidateRow {
    pub(crate) id: String,
    pub(crate) display_name: Option<String>,
    pub(crate) owned_by: Option<String>,
    pub(crate) is_custom_entry: bool,
}

/// State for the three-step `/provider` preset, connection, and model discovery wizard.
pub(crate) struct ProviderWizardState {
    pub(crate) step: ProviderWizardStep,
    pub(crate) selected_preset: usize,
    pub(crate) is_custom: bool,
    pub(crate) preset_label: String,
    pub(crate) preset_endpoint: Option<String>,
    pub(crate) preset_has_max_tokens: bool,
    pub(crate) preset_requires_api_key: bool,
    pub(crate) provider: Provider,
    pub(crate) profile_name: String,
    pub(crate) initial_profile_name: String,
    pub(crate) endpoint: String,
    pub(crate) api_key: String,
    pub(crate) ollama_default_hint: bool,
    pub(crate) max_tokens: String,
    pub(crate) auto_max_tokens: bool,
    pub(crate) active_field: usize,
    pub(crate) saving: bool,
    pub(crate) error: String,
    pub(crate) existing_credential: Option<CredentialRef>,
    pub(crate) suggested_model: Option<String>,
    pub(crate) has_existing_models: bool,
    pub(crate) user_modified_selection: bool,
    pub(crate) model_fetch_status: ModelFetchStatus,
    pub(crate) fetched_models: Vec<DiscoveredModel>,
    pub(crate) model_query: String,
    pub(crate) model_cursor: usize,
    pub(crate) selected_models: Vec<String>,
    pub(crate) fetch_generation: u64,
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
            preset_requires_api_key: true,
            provider: Provider::OpenAiChat,
            profile_name: String::new(),
            initial_profile_name: String::new(),
            endpoint: String::new(),
            api_key: String::new(),
            ollama_default_hint: false,
            max_tokens: String::new(),
            auto_max_tokens: false,
            active_field: 0,
            saving: false,
            error: String::new(),
            existing_credential: None,
            suggested_model: None,
            has_existing_models: false,
            user_modified_selection: false,
            model_fetch_status: ModelFetchStatus::Idle,
            fetched_models: Vec::new(),
            model_query: String::new(),
            model_cursor: 0,
            selected_models: Vec::new(),
            fetch_generation: 0,
        }
    }

    pub(crate) fn clear_secret(&mut self) {
        clear_secret(&mut self.api_key);
        self.ollama_default_hint = false;
    }

    pub(crate) fn enter_step_2(&mut self, config: &Config) {
        self.clear_secret();
        self.error.clear();
        self.saving = false;
        self.auto_max_tokens = false;
        self.fetch_generation = self.fetch_generation.wrapping_add(1);
        self.model_fetch_status = ModelFetchStatus::Idle;
        self.fetched_models.clear();
        self.model_query.clear();
        self.model_cursor = 0;
        self.selected_models.clear();
        self.existing_credential = None;
        self.suggested_model = None;
        self.has_existing_models = false;
        self.user_modified_selection = false;

        let profiles = config.model_profiles();
        if let Some(preset) = PROVIDER_PRESETS.get(self.selected_preset) {
            self.is_custom = false;
            self.preset_label = preset.label.to_owned();
            self.preset_endpoint = preset.endpoint.map(str::to_owned);
            self.preset_has_max_tokens = preset.default_max_tokens.is_some();
            self.preset_requires_api_key = preset.requires_api_key;
            self.provider = preset.provider;
            self.profile_name = preset.id.to_owned();
            self.initial_profile_name = preset.id.to_owned();
            self.suggested_model =
                (!preset.suggested_model.is_empty()).then(|| preset.suggested_model.to_owned());

            if let Some(existing) = profiles.get(preset.id) {
                self.endpoint = existing
                    .endpoint
                    .clone()
                    .unwrap_or_else(|| preset.endpoint.unwrap_or_default().to_owned());
                self.max_tokens = existing
                    .max_tokens
                    .map(|v| v.to_string())
                    .unwrap_or_default();
                self.existing_credential = Some(existing.credential.clone());
                let avail = existing.available_models();
                self.has_existing_models = !avail.is_empty();
                self.selected_models = avail;
            } else {
                self.endpoint = preset.endpoint.unwrap_or_default().to_owned();
                self.max_tokens = preset
                    .default_max_tokens
                    .map(|v| v.to_string())
                    .unwrap_or_default();
                if preset.id == "ollama" {
                    self.api_key = "ollama".to_owned();
                    self.ollama_default_hint = true;
                }
            }
            // Start focus on API Key (index 1) for key-requiring presets, or ProfileName (index 0) for ollama.
            self.active_field = if preset.requires_api_key { 1 } else { 0 };
        } else {
            self.is_custom = true;
            self.preset_label = "Custom provider".to_owned();
            self.preset_endpoint = None;
            self.preset_has_max_tokens = false;
            self.preset_requires_api_key = true;
            self.provider = Provider::OpenAiChat;
            self.profile_name = "custom".to_owned();
            self.initial_profile_name = "custom".to_owned();
            self.endpoint.clear();
            self.max_tokens.clear();
            self.active_field = 0;
        }
        self.step = ProviderWizardStep::Configure;
    }

    pub(crate) fn enter_step_3(&mut self) {
        self.step = ProviderWizardStep::SelectModels;
        self.error.clear();
        self.model_query.clear();
        self.model_cursor = 0;
        self.fetched_models.clear();
        self.user_modified_selection = false;
        self.model_fetch_status = ModelFetchStatus::Fetching;
        self.fetch_generation = self.fetch_generation.wrapping_add(1);
        if self.selected_models.is_empty()
            && let Some(suggested) = &self.suggested_model
        {
            self.selected_models.push(suggested.clone());
        }
    }

    pub(crate) fn back_to_step_2(&mut self) {
        self.step = ProviderWizardStep::Configure;
        self.fetch_generation = self.fetch_generation.wrapping_add(1);
        self.model_fetch_status = ModelFetchStatus::Idle;
        self.error.clear();
    }

    pub(crate) fn back_to_step_1(&mut self) {
        self.clear_secret();
        self.fetch_generation = self.fetch_generation.wrapping_add(1);
        self.model_fetch_status = ModelFetchStatus::Idle;
        self.fetched_models.clear();
        self.selected_models.clear();
        self.existing_credential = None;
        self.suggested_model = None;
        self.has_existing_models = false;
        self.user_modified_selection = false;
        self.model_query.clear();
        self.model_cursor = 0;
        self.error.clear();
        self.saving = false;
        self.step = ProviderWizardStep::SelectPreset;
    }

    pub(crate) fn effective_existing_credential(&self, config: &Config) -> Option<CredentialRef> {
        let current_name = self.profile_name.trim();
        if current_name == self.initial_profile_name
            && let Some(cred) = &self.existing_credential
        {
            return Some(cred.clone());
        }
        config
            .model_profiles()
            .get(current_name)
            .map(|profile| profile.credential.clone())
    }

    pub(crate) fn fields(&self) -> Vec<ProviderFieldKind> {
        if self.is_custom {
            vec![
                ProviderFieldKind::Protocol,
                ProviderFieldKind::Endpoint,
                ProviderFieldKind::ProfileName,
                ProviderFieldKind::ApiKey,
                ProviderFieldKind::MaxTokens,
            ]
        } else if self.provider == Provider::Anthropic || self.preset_has_max_tokens {
            vec![
                ProviderFieldKind::ProfileName,
                ProviderFieldKind::ApiKey,
                ProviderFieldKind::MaxTokens,
            ]
        } else {
            vec![ProviderFieldKind::ProfileName, ProviderFieldKind::ApiKey]
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
        if self.saving {
            return;
        }
        let sanitized: String = text.chars().filter(|c| !c.is_control()).collect();
        if sanitized.is_empty() {
            return;
        }
        match self.step {
            ProviderWizardStep::SelectPreset => {}
            ProviderWizardStep::Configure => {
                let Some(field) = self.current_field() else {
                    return;
                };
                if field == ProviderFieldKind::Protocol {
                    return;
                }
                match field {
                    ProviderFieldKind::Protocol => {}
                    ProviderFieldKind::Endpoint => self.endpoint.push_str(&sanitized),
                    ProviderFieldKind::ProfileName => self.profile_name.push_str(&sanitized),
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
            ProviderWizardStep::SelectModels => {
                self.model_query.push_str(&sanitized);
                self.model_cursor = 0;
                self.error.clear();
            }
        }
    }

    pub(crate) fn visible_model_candidates(&self) -> Vec<ModelCandidateRow> {
        let mut base: Vec<ModelCandidateRow> =
            Vec::with_capacity(self.fetched_models.len() + self.selected_models.len() + 1);
        let mut seen = HashSet::new();
        for model in &self.fetched_models {
            if seen.insert(model.id.clone()) {
                base.push(ModelCandidateRow {
                    id: model.id.clone(),
                    display_name: model.display_name.clone(),
                    owned_by: model.owned_by.clone(),
                    is_custom_entry: false,
                });
            }
        }
        for selected in &self.selected_models {
            if seen.insert(selected.clone()) {
                base.push(ModelCandidateRow {
                    id: selected.clone(),
                    display_name: None,
                    owned_by: None,
                    is_custom_entry: false,
                });
            }
        }
        if self.fetched_models.is_empty()
            && let Some(suggested) = &self.suggested_model
            && seen.insert(suggested.clone())
        {
            base.push(ModelCandidateRow {
                id: suggested.clone(),
                display_name: None,
                owned_by: None,
                is_custom_entry: false,
            });
        }

        let trimmed = self.model_query.trim();
        let query_lower = trimmed.to_lowercase();
        let mut filtered: Vec<ModelCandidateRow> = base
            .into_iter()
            .filter(|row| {
                query_lower.is_empty()
                    || row.id.to_lowercase().contains(&query_lower)
                    || row
                        .display_name
                        .as_deref()
                        .is_some_and(|name| name.to_lowercase().contains(&query_lower))
            })
            .collect();

        if !trimmed.is_empty()
            && !trimmed.chars().any(char::is_whitespace)
            && !trimmed.chars().any(char::is_control)
            && !filtered.iter().any(|row| row.id == trimmed)
        {
            filtered.push(ModelCandidateRow {
                id: trimmed.to_owned(),
                display_name: None,
                owned_by: None,
                is_custom_entry: true,
            });
        }
        filtered
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

enum FetchSecretSource {
    Inline(SecretBox),
    Store {
        credentials_path: PathBuf,
        credential_name: String,
    },
    Env(String),
    None,
}

struct ValidatedConnection {
    config_path: PathBuf,
    credentials_path: PathBuf,
    profile_name: String,
    provider: Provider,
    endpoint: Option<String>,
    max_tokens: Option<u32>,
    trimmed_key: String,
    has_store_cred: bool,
    store_cred_name: String,
    env_var_name: Option<String>,
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

fn validate_step_2_fields(
    wizard: &mut ProviderWizardState,
    config: &Config,
    existing_names: &CredentialStoreCache,
    target_config_path: Option<PathBuf>,
) -> Option<ValidatedConnection> {
    let Some(config_path) = target_config_path else {
        wizard.error = "Cannot resolve config.toml path; pass --config FILE".into();
        return None;
    };
    let Some(credentials_path) = tars_core::config::credentials_path_for_config(Some(&config_path))
    else {
        wizard.error = "Cannot resolve credentials.toml path".into();
        return None;
    };

    let profile_name = wizard.profile_name.trim().to_owned();
    if !valid_credential_name(&profile_name) {
        wizard.error =
            "Profile name must be 1–64 ASCII letters/digits/'_'/'-' starting with letter/digit"
                .into();
        return None;
    }

    let endpoint_trimmed = wizard.endpoint.trim();
    let endpoint = if endpoint_trimmed.is_empty() {
        None
    } else {
        if endpoint_trimmed.chars().any(char::is_whitespace)
            || endpoint_trimmed.chars().any(char::is_control)
            || (!endpoint_trimmed.starts_with("http://")
                && !endpoint_trimmed.starts_with("https://"))
            || derive_models_url(wizard.provider, Some(endpoint_trimmed)).is_err()
        {
            wizard.error = "Endpoint must be a valid http:// or https:// URL".into();
            return None;
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
                return None;
            }
        }
    };
    if wizard.provider == Provider::Anthropic && max_tokens.is_none() {
        wizard.error = "Anthropic requires max_tokens (e.g. 8192)".into();
        return None;
    }

    let trimmed_key = wizard.api_key.trim().to_owned();
    if trimmed_key.chars().any(char::is_control) {
        wizard.error = "API key must not contain control characters".into();
        return None;
    }

    let existing_cred = wizard.effective_existing_credential(config);
    let (has_store_cred, store_cred_name, env_var_name) = match &existing_cred {
        Some(CredentialRef::Store(name)) => (existing_names.contains(name), name.clone(), None),
        Some(CredentialRef::Environment(var)) => (false, profile_name.clone(), Some(var.clone())),
        None => (
            existing_names.contains(&profile_name),
            profile_name.clone(),
            None,
        ),
    };

    let needs_key = wizard.is_custom || wizard.preset_requires_api_key;
    if trimmed_key.is_empty() && needs_key && !has_store_cred && env_var_name.is_none() {
        wizard.error = "API key is required".into();
        return None;
    }

    Some(ValidatedConnection {
        config_path,
        credentials_path,
        profile_name,
        provider: wizard.provider,
        endpoint,
        max_tokens,
        trimmed_key,
        has_store_cred,
        store_cred_name,
        env_var_name,
    })
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
            self.notice =
                "Disabled under --no-config; omit --no-config or pass --config FILE".into();
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
                ProviderWizardStep::SelectModels => {
                    if wizard.saving {
                        self.panel = Some(Panel::Provider(wizard));
                        return;
                    }
                    wizard.back_to_step_2();
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
                    KeyCode::Enter if no_ctrl_alt => {
                        wizard.enter_step_2(&self.options.config);
                    }
                    _ => {}
                }
                self.panel = Some(Panel::Provider(wizard));
            }
            ProviderWizardStep::Configure => {
                if super::super::input::control(key, 's') {
                    self.panel = Some(Panel::Provider(wizard));
                    self.quick_save_provider_step_2();
                    return;
                }
                if key.code == KeyCode::Enter && no_ctrl_alt {
                    self.panel = Some(Panel::Provider(wizard));
                    self.advance_provider_to_step_3();
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
            ProviderWizardStep::SelectModels => {
                if super::super::input::control(key, 'r') {
                    self.panel = Some(Panel::Provider(wizard));
                    self.retry_fetch_provider_models();
                    return;
                }
                if key.code == KeyCode::Enter && no_ctrl_alt {
                    self.panel = Some(Panel::Provider(wizard));
                    self.confirm_provider_step_3();
                    return;
                }
                let candidates = wizard.visible_model_candidates();
                let count = candidates.len();
                wizard.model_cursor = wizard.model_cursor.min(count.saturating_sub(1));
                match key.code {
                    KeyCode::Up => {
                        wizard.model_cursor = wizard.model_cursor.saturating_sub(1);
                    }
                    KeyCode::Down => {
                        if count > 0 {
                            wizard.model_cursor = (wizard.model_cursor + 1).min(count - 1);
                        }
                    }
                    KeyCode::Char(' ') if no_ctrl_alt => {
                        if let Some(candidate) = candidates.get(wizard.model_cursor) {
                            wizard.user_modified_selection = true;
                            if wizard.selected_models.iter().any(|m| m == &candidate.id) {
                                wizard.selected_models.retain(|m| m != &candidate.id);
                            } else {
                                wizard.selected_models.push(candidate.id.clone());
                            }
                            wizard.error.clear();
                        }
                    }
                    KeyCode::Backspace => {
                        wizard.model_query.pop();
                        wizard.model_cursor = 0;
                        wizard.error.clear();
                    }
                    KeyCode::Char(c) if no_ctrl_alt && !c.is_control() && c != ' ' => {
                        wizard.model_query.push(c);
                        wizard.model_cursor = 0;
                        wizard.error.clear();
                    }
                    _ => {}
                }
                self.panel = Some(Panel::Provider(wizard));
            }
        }
    }

    fn check_not_no_config(&mut self) -> bool {
        if self.options.no_config {
            if let Some(Panel::Provider(wizard)) = self.panel.as_mut() {
                wizard.clear_secret();
                wizard.error =
                    "Disabled under --no-config; omit --no-config or pass --config FILE".into();
            }
            self.notice =
                "Disabled under --no-config; omit --no-config or pass --config FILE".into();
            return false;
        }
        true
    }

    fn advance_provider_to_step_3(&mut self) {
        if !self.check_not_no_config() {
            return;
        }
        let existing_names = self.credential_store_cache.clone();
        let target_config_path = self
            .options
            .config_path
            .clone()
            .or_else(default_config_path);
        let config = Arc::clone(&self.options.config);
        let Some(Panel::Provider(wizard)) = self.panel.as_mut() else {
            return;
        };
        if wizard.saving {
            return;
        }
        let Some(validated) =
            validate_step_2_fields(wizard, &config, &existing_names, target_config_path)
        else {
            return;
        };
        wizard.enter_step_3();
        let generation = wizard.fetch_generation;
        self.spawn_fetch_models(validated, generation);
    }

    fn retry_fetch_provider_models(&mut self) {
        if !self.check_not_no_config() {
            return;
        }
        let existing_names = self.credential_store_cache.clone();
        let target_config_path = self
            .options
            .config_path
            .clone()
            .or_else(default_config_path);
        let config = Arc::clone(&self.options.config);
        let Some(Panel::Provider(wizard)) = self.panel.as_mut() else {
            return;
        };
        if wizard.saving {
            return;
        }
        let Some(validated) =
            validate_step_2_fields(wizard, &config, &existing_names, target_config_path)
        else {
            return;
        };
        wizard.error.clear();
        wizard.model_fetch_status = ModelFetchStatus::Fetching;
        wizard.fetch_generation = wizard.fetch_generation.wrapping_add(1);
        let generation = wizard.fetch_generation;
        self.spawn_fetch_models(validated, generation);
    }

    fn spawn_fetch_models(&mut self, validated: ValidatedConnection, generation: u64) {
        if tokio::runtime::Handle::try_current().is_err() || !self.capacity() {
            if let Some(Panel::Provider(wizard)) = self.panel.as_mut()
                && wizard.fetch_generation == generation
            {
                wizard.model_fetch_status = ModelFetchStatus::Idle;
            }
            return;
        }
        let secret_source = if !validated.trimmed_key.is_empty() {
            FetchSecretSource::Inline(SecretBox(validated.trimmed_key))
        } else if validated.has_store_cred {
            FetchSecretSource::Store {
                credentials_path: validated.credentials_path,
                credential_name: validated.store_cred_name,
            }
        } else if let Some(var_name) = validated.env_var_name {
            FetchSecretSource::Env(var_name)
        } else {
            FetchSecretSource::None
        };
        let provider = validated.provider;
        let endpoint = validated.endpoint;

        self.spawn_job(None, JobKind::FetchProviderModels, async move {
            let resolved_secret: Option<SecretBox> = match secret_source {
                FetchSecretSource::Inline(secret) => Some(secret),
                FetchSecretSource::Store {
                    credentials_path,
                    credential_name,
                } => blocking(move || {
                    CredentialsStore::load_optional(&credentials_path)
                        .ok()
                        .flatten()
                        .and_then(|store| store.get(&credential_name).map(str::to_owned))
                        .map(SecretBox)
                })
                .await
                .ok()
                .flatten(),
                FetchSecretSource::Env(var_name) => std::env::var(&var_name)
                    .ok()
                    .filter(|val| !val.trim().is_empty())
                    .map(SecretBox),
                FetchSecretSource::None => None,
            };
            let result = fetch_available_models(
                provider,
                endpoint.as_deref(),
                resolved_secret.as_ref().map(|s| s.0.as_str()),
            )
            .await
            .map_err(|err| err.to_string());
            drop(resolved_secret);
            Message::ProviderModelsFetched { generation, result }
        });
    }

    fn quick_save_provider_step_2(&mut self) {
        if !self.check_not_no_config() {
            return;
        }
        let existing_names = self.credential_store_cache.clone();
        let target_config_path = self
            .options
            .config_path
            .clone()
            .or_else(default_config_path);
        let config = Arc::clone(&self.options.config);
        let Some(Panel::Provider(wizard)) = self.panel.as_mut() else {
            return;
        };
        if wizard.saving {
            return;
        }
        let Some(validated) =
            validate_step_2_fields(wizard, &config, &existing_names, target_config_path)
        else {
            return;
        };
        let quick_models = if !wizard.selected_models.is_empty() {
            Some(wizard.selected_models.clone())
        } else {
            wizard.suggested_model.clone().map(|model| vec![model])
        };
        let Some(models_to_save) = quick_models else {
            wizard.enter_step_3();
            let generation = wizard.fetch_generation;
            self.spawn_fetch_models(validated, generation);
            return;
        };
        self.save_provider_wizard(validated, models_to_save);
    }

    fn confirm_provider_step_3(&mut self) {
        if !self.check_not_no_config() {
            return;
        }
        let existing_names = self.credential_store_cache.clone();
        let target_config_path = self
            .options
            .config_path
            .clone()
            .or_else(default_config_path);
        let config = Arc::clone(&self.options.config);
        let Some(Panel::Provider(wizard)) = self.panel.as_mut() else {
            return;
        };
        if wizard.saving {
            return;
        }
        let Some(validated) =
            validate_step_2_fields(wizard, &config, &existing_names, target_config_path)
        else {
            return;
        };
        let candidates = wizard.visible_model_candidates();
        let highlighted = candidates.get(wizard.model_cursor);
        let custom_selected_without_toggle = !wizard.user_modified_selection
            && !wizard.has_existing_models
            && highlighted.is_some_and(|row| row.is_custom_entry);

        let models_to_save: Vec<String> = if custom_selected_without_toggle {
            vec![highlighted.expect("checked above").id.clone()]
        } else if !wizard.selected_models.is_empty() {
            wizard.selected_models.clone()
        } else if let Some(candidate) = highlighted {
            vec![candidate.id.clone()]
        } else {
            let trimmed_query = wizard.model_query.trim();
            if !trimmed_query.is_empty()
                && !trimmed_query.chars().any(char::is_whitespace)
                && !trimmed_query.chars().any(char::is_control)
            {
                vec![trimmed_query.to_owned()]
            } else {
                wizard.error = "Select or enter at least one model".into();
                return;
            }
        };
        self.save_provider_wizard(validated, models_to_save);
    }

    fn save_provider_wizard(
        &mut self,
        validated: ValidatedConnection,
        models_to_save: Vec<String>,
    ) {
        if !self.capacity() {
            return;
        }
        let Some(Panel::Provider(wizard)) = self.panel.as_mut() else {
            return;
        };
        let mut normalized_models = Vec::with_capacity(models_to_save.len());
        let mut seen = HashSet::with_capacity(models_to_save.len());
        for raw in models_to_save {
            let trimmed = raw.trim().to_owned();
            if trimmed.is_empty() || trimmed.chars().any(char::is_control) {
                wizard.error = "Model identifier must not be blank".into();
                return;
            }
            if seen.insert(trimmed.clone()) {
                normalized_models.push(trimmed);
            }
        }
        if normalized_models.is_empty() {
            wizard.error = "Select or enter at least one model".into();
            return;
        }

        let (secret_box, credential_name) = if !validated.trimmed_key.is_empty() {
            (
                Some(SecretBox(validated.trimmed_key)),
                validated.profile_name.clone(),
            )
        } else if validated.has_store_cred {
            (None, validated.store_cred_name)
        } else if let Some(var) = &validated.env_var_name {
            wizard.error = format!(
                "Enter an API key to migrate env:{var} into credentials.toml (Esc to go back)"
            );
            return;
        } else {
            wizard.error = "API key is required".into();
            return;
        };

        wizard.clear_secret();
        wizard.saving = true;
        wizard.error.clear();

        let primary_model = normalized_models[0].clone();
        let input = ProfileUpsertInput {
            profile_name: validated.profile_name.clone(),
            provider: validated.provider,
            model: primary_model,
            models: normalized_models,
            credential: credential_name,
            endpoint: validated.endpoint,
            max_tokens: validated.max_tokens,
        };
        let config_path = validated.config_path;
        let credentials_path = validated.credentials_path;
        let profile_name = validated.profile_name;

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

    pub(super) fn provider_models_fetched(
        &mut self,
        generation: u64,
        result: Result<Vec<DiscoveredModel>, String>,
    ) {
        let Some(Panel::Provider(wizard)) = self.panel.as_mut() else {
            return;
        };
        if wizard.step != ProviderWizardStep::SelectModels || wizard.fetch_generation != generation
        {
            return;
        }
        match result {
            Ok(models) => {
                wizard.model_fetch_status = ModelFetchStatus::Loaded;
                wizard.fetched_models = models;
                if !wizard.has_existing_models
                    && !wizard.user_modified_selection
                    && !wizard.fetched_models.is_empty()
                    && let Some(suggested) = &wizard.suggested_model
                {
                    if wizard.fetched_models.iter().any(|m| m.id == *suggested) {
                        wizard.selected_models = vec![suggested.clone()];
                    } else {
                        wizard.selected_models.clear();
                    }
                }
                wizard.model_cursor = 0;
                wizard.error.clear();
            }
            Err(err) => {
                let escaped = tars_core::permission::escape_untrusted_text(&err);
                wizard.model_fetch_status = ModelFetchStatus::Failed(escaped);
                wizard.model_cursor = 0;
            }
        }
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
