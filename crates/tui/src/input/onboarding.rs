use super::{edit, editor, paste};
use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui_textarea::{CursorMove, TextArea};
use std::path::PathBuf;
use tars_core::{
    config::{OnboardingInput, render_initial_config},
    definition::Provider,
    permission::PermissionMode,
};

pub(crate) const ONBOARDING_FIELD_NAMES: [&str; 6] = [
    "Provider",
    "Model",
    "API key env var",
    "Endpoint (optional)",
    "Max tokens (required for anthropic)",
    "Workspace",
];

const PROVIDERS: [Provider; 3] = [
    Provider::OpenAiResponses,
    Provider::OpenAiChat,
    Provider::Anthropic,
];

pub(crate) struct OnboardingForm {
    pub fields: Vec<TextArea<'static>>,
    pub selected: usize,
    pub provider_index: usize,
    pub max_tokens_touched: bool,
    pub error: String,
}

impl OnboardingForm {
    pub(crate) fn new(cwd: Option<PathBuf>) -> Self {
        let workspace = cwd
            .map(|path| path.display().to_string())
            .unwrap_or_default();
        let mut ws_area = TextArea::from([workspace]);
        ws_area.move_cursor(CursorMove::End);
        let mut env_area = TextArea::from(["OPENAI_API_KEY"]);
        env_area.move_cursor(CursorMove::End);
        Self {
            fields: vec![
                TextArea::from(["openai-responses"]),
                editor(),
                env_area,
                editor(),
                editor(),
                ws_area,
            ],
            selected: 1,
            provider_index: 0,
            max_tokens_touched: false,
            error: String::new(),
        }
    }

    pub(crate) fn provider(&self) -> Provider {
        PROVIDERS[self.provider_index]
    }

    pub(crate) fn cycle_provider(&mut self, forward: bool) {
        let previous = self.provider();
        self.provider_index = if forward {
            (self.provider_index + 1) % PROVIDERS.len()
        } else {
            (self.provider_index + PROVIDERS.len() - 1) % PROVIDERS.len()
        };
        let next = self.provider();
        self.fields[0] = TextArea::from([next.as_str()]);

        let current_env = self.fields[2].lines().join("");
        let trimmed_env = current_env.trim();
        if trimmed_env == "OPENAI_API_KEY" || trimmed_env == "ANTHROPIC_API_KEY" {
            let next_env = match next {
                Provider::Anthropic => "ANTHROPIC_API_KEY",
                Provider::OpenAiResponses | Provider::OpenAiChat => "OPENAI_API_KEY",
            };
            let mut env_area = TextArea::from([next_env]);
            env_area.move_cursor(CursorMove::End);
            self.fields[2] = env_area;
        }

        if !self.max_tokens_touched {
            if next == Provider::Anthropic && previous != Provider::Anthropic {
                let mut tokens_area = TextArea::from(["8192"]);
                tokens_area.move_cursor(CursorMove::End);
                self.fields[4] = tokens_area;
            } else if next != Provider::Anthropic && previous == Provider::Anthropic {
                self.fields[4] = editor();
            }
        }
    }

    pub(crate) fn key(&mut self, key: KeyEvent) {
        if key.kind == KeyEventKind::Release {
            return;
        }
        let is_backtab = key.code == KeyCode::BackTab
            || (key.code == KeyCode::Tab && key.modifiers.contains(KeyModifiers::SHIFT));
        if is_backtab {
            self.selected = (self.selected + self.fields.len() - 1) % self.fields.len();
            return;
        }
        match key.code {
            KeyCode::Tab | KeyCode::Down => {
                self.selected = (self.selected + 1) % self.fields.len();
            }
            KeyCode::Up => {
                self.selected = (self.selected + self.fields.len() - 1) % self.fields.len();
            }
            KeyCode::Left if self.selected == 0 => self.cycle_provider(false),
            KeyCode::Right if self.selected == 0 => self.cycle_provider(true),
            KeyCode::Enter => {}
            _ if self.selected != 0
                && (!key.modifiers.contains(KeyModifiers::CONTROL)
                    || !matches!(key.code, KeyCode::Char('m' | 'j'))) =>
            {
                if self.selected == 4
                    && !matches!(
                        key.code,
                        KeyCode::Left | KeyCode::Right | KeyCode::Home | KeyCode::End
                    )
                {
                    self.max_tokens_touched = true;
                }
                edit(&mut self.fields[self.selected], key);
            }
            _ => {}
        }
    }

    pub(crate) fn paste(&mut self, text: &str) {
        if self.selected == 0 {
            return;
        }
        if self.selected == 4 {
            self.max_tokens_touched = true;
        }
        paste(&mut self.fields[self.selected], text, false);
    }

    pub(crate) fn validate_and_build(&self) -> Result<(OnboardingInput, Option<PathBuf>), String> {
        let provider = self.provider();
        let model = self.fields[1].lines().join("").trim().to_owned();
        let api_key_env = self.fields[2].lines().join("").trim().to_owned();
        let endpoint_raw = self.fields[3].lines().join("");
        let endpoint = (!endpoint_raw.trim().is_empty()).then(|| endpoint_raw.trim().to_owned());

        let max_tokens_raw = self.fields[4].lines().join("");
        let max_tokens_trimmed = max_tokens_raw.trim();
        let max_tokens = if max_tokens_trimmed.is_empty() {
            None
        } else {
            let parsed = max_tokens_trimmed
                .parse::<u32>()
                .map_err(|_| "Max tokens must be a positive integer".to_owned())?;
            Some(parsed)
        };

        let input = OnboardingInput {
            provider,
            model,
            api_key_env,
            endpoint,
            max_tokens,
            permission_mode: PermissionMode::Ask,
        };
        // Validate profile fields purely in memory before checking workspace or touching disk.
        render_initial_config(&input).map_err(|error| error.to_string())?;

        let workspace_raw = self.fields[5].lines().join("");
        let workspace_trimmed = workspace_raw.trim();
        let workspace_root = if workspace_trimmed.is_empty() {
            None
        } else {
            let candidate = PathBuf::from(workspace_trimmed);
            let abs = if candidate.is_absolute() {
                candidate
            } else {
                std::env::current_dir()
                    .map_err(|error| format!("Failed to resolve workspace directory: {error}"))?
                    .join(candidate)
            };
            let canonical = std::fs::canonicalize(&abs).map_err(|error| {
                format!("Workspace path is invalid ({}): {error}", abs.display())
            })?;
            let meta = std::fs::metadata(&canonical).map_err(|error| {
                format!(
                    "Workspace path is invalid ({}): {error}",
                    canonical.display()
                )
            })?;
            if !meta.is_dir() {
                return Err(format!(
                    "Workspace path is not a directory: {}",
                    canonical.display()
                ));
            }
            Some(canonical)
        };

        Ok((input, workspace_root))
    }
}
