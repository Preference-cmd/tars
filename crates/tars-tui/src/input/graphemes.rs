use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui_textarea::{CursorMove, TextArea};
use unicode_segmentation::UnicodeSegmentation;

// The candidate widget edits Unicode scalar values. Adapt ordinary horizontal
// movement and deletion to extended grapheme clusters without forking the widget.
pub(crate) fn edit(editor: &mut TextArea<'static>, key: KeyEvent) {
    if key.modifiers.contains(KeyModifiers::CONTROL) && !key.modifiers.contains(KeyModifiers::ALT) {
        match key.code {
            KeyCode::Char('z') => {
                editor.undo();
                return;
            }
            KeyCode::Char('y') => {
                editor.redo();
                return;
            }
            _ => {}
        }
    }
    let plain = !key
        .modifiers
        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT);
    let deleting = matches!(key.code, KeyCode::Backspace | KeyCode::Delete);
    let moving = matches!(key.code, KeyCode::Left | KeyCode::Right);
    if !plain || (!deleting && !moving) || (deleting && editor.is_selecting()) {
        editor.input(key);
        return;
    }
    let backward = matches!(key.code, KeyCode::Backspace | KeyCode::Left);
    let cursor = editor.cursor();
    let line = &editor.lines()[cursor.0];
    let mut columns = 0;
    let mut boundary = None;
    for grapheme in line.graphemes(true) {
        let end = columns + grapheme.chars().count();
        if (backward && columns < cursor.1 && cursor.1 <= end)
            || (!backward && columns <= cursor.1 && cursor.1 < end)
        {
            boundary = Some((columns, end));
            break;
        }
        columns = end;
    }
    let Some((start, end)) = boundary else {
        editor.input(key); // Newline crossing or an edge of the document.
        return;
    };
    if deleting {
        // Select the whole cluster; deletion is a single undoable edit. Finish
        // a partial-cluster cursor position before starting the selection.
        move_to(editor, end);
        editor.start_selection();
        move_to(editor, start);
        editor.delete_char();
    } else {
        if key.modifiers.contains(KeyModifiers::SHIFT) {
            if !editor.is_selecting() {
                editor.start_selection();
            }
        } else {
            editor.cancel_selection();
        }
        move_to(editor, if backward { start } else { end });
    }
}

fn move_to(editor: &mut TextArea<'static>, column: usize) {
    // CursorMove::Jump uses u16; stepping avoids truncating long pasted lines.
    while editor.cursor().1 > column {
        editor.move_cursor(CursorMove::Back);
    }
    while editor.cursor().1 < column {
        editor.move_cursor(CursorMove::Forward);
    }
}
