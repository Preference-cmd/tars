use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui_textarea::{TextArea, WrapMode};

mod complete;
pub(crate) mod editor;
mod graphemes;
pub(crate) mod image;
mod onboarding;
pub(crate) use complete::{
    CompleteResult, ModelArgCompletion, char_col_to_byte, complete_copy_arg, complete_editor,
    complete_model_arg, replace_range_in_editor, set_editor_text, slash_command_takes_args,
    strip_copy_command_prefix, strip_model_command_prefix,
};
pub(crate) use graphemes::edit;
pub(crate) use onboarding::FolderTrustWizard;

pub(crate) fn editor() -> TextArea<'static> {
    let mut editor = TextArea::default();
    editor.set_wrap_mode(WrapMode::Word);
    editor.set_cursor_line_style(ratatui::style::Style::default());
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
