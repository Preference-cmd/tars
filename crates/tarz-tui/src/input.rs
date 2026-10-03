use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui_textarea::{TextArea, WrapMode};
use tarz_core::definition::{CredentialRef, Provider, SessionDefinition};

mod graphemes;
pub(crate) use graphemes::edit;

pub(crate) fn editor() -> TextArea<'static> {
    let mut editor = TextArea::default();
    editor.set_wrap_mode(WrapMode::Word);
    editor
}

pub(crate) fn control(key: KeyEvent, letter: char) -> bool {
    key.kind == KeyEventKind::Press
        && key.modifiers.contains(KeyModifiers::CONTROL)
        && key.code == KeyCode::Char(letter)
}

// Strip control characters, including escape, from pasted content. Newlines and
// tabs are data; the paste event never passes through command dispatch.
pub(crate) fn paste(editor: &mut TextArea<'static>, text: &str, multiline: bool) {
    let normalized = text.replace("\r\n", "\n").replace('\r', "\n");
    let text: String = normalized
        .chars()
        .filter(|c| !c.is_control() || (multiline && matches!(c, '\n' | '\t')))
        .collect();
    editor.insert_str(text);
}

pub(crate) struct Form {
    pub fields: Vec<TextArea<'static>>,
    pub selected: usize,
    pub error: String,
    pub limits: (usize, u32, u32),
}
pub(crate) const FIELD_NAMES: [&str; 5] = [
    "Provider",
    "Model",
    "Endpoint (optional, full URL)",
    "Credential environment variable",
    "Timeout seconds (1..86400)",
];

impl Default for Form {
    fn default() -> Self {
        Self {
            fields: ["openai-chat", "", "", "OPENAI_API_KEY", "120"]
                .into_iter()
                .map(|s| TextArea::from([s]))
                .collect(),
            selected: 1,
            error: String::new(),
            limits: (256, 10, 64),
        }
    }
}
impl Form {
    pub fn new(preset: Option<&SessionDefinition>) -> Self {
        let Some(preset) = preset else {
            return Self::default();
        };
        let provider = match preset.provider {
            Provider::OpenAiChat => "openai-chat",
            Provider::OpenAiResponses => "openai-responses",
            Provider::Anthropic => "anthropic",
        };
        let CredentialRef::Environment(name) = &preset.credential;
        let values = [
            provider.to_owned(),
            preset.model.clone(),
            preset.endpoint.clone().unwrap_or_default(),
            name.clone(),
            preset.work_timeout_secs.unwrap_or(120).to_string(),
        ];
        Self {
            fields: values
                .into_iter()
                .map(|value| TextArea::from([value]))
                .collect(),
            selected: 1,
            error: String::new(),
            limits: (
                preset.retained_work_capacity,
                preset.max_model_rounds,
                preset.max_tool_calls,
            ),
        }
    }
    pub fn definition(&self) -> Result<SessionDefinition, String> {
        let values: Vec<_> = self.fields.iter().map(|f| f.lines().join("")).collect();
        let provider = match values[0].as_str() {
            "openai-chat" => Provider::OpenAiChat,
            "openai-responses" => Provider::OpenAiResponses,
            "anthropic" => Provider::Anthropic,
            _ => return Err("Provider: openai-chat | openai-responses | anthropic".into()),
        };
        let timeout = values[4]
            .parse::<u64>()
            .ok()
            .filter(|n| (1..=86400).contains(n))
            .ok_or("Timeout must be 1..86400 seconds")?;
        let definition = SessionDefinition {
            provider,
            model: values[1].clone(),
            endpoint: (!values[2].is_empty()).then(|| values[2].clone()),
            credential: CredentialRef::Environment(values[3].clone()),
            work_timeout_secs: Some(timeout),
            retained_work_capacity: self.limits.0,
            max_model_rounds: self.limits.1,
            max_tool_calls: self.limits.2,
        };
        definition.validate().map_err(|e| e.to_string())?;
        Ok(definition)
    }
    pub fn key(&mut self, key: KeyEvent) {
        if key.kind == KeyEventKind::Release {
            return;
        }
        match key.code {
            KeyCode::Tab => self.selected = (self.selected + 1) % self.fields.len(),
            KeyCode::BackTab => {
                self.selected = (self.selected + self.fields.len() - 1) % self.fields.len()
            }
            KeyCode::Enter => {}
            _ if !key.modifiers.contains(KeyModifiers::CONTROL)
                || !matches!(key.code, KeyCode::Char('m' | 'j')) =>
            {
                edit(&mut self.fields[self.selected], key);
            }
            _ => {}
        }
    }
}
