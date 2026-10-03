use crossterm::{
    cursor::Show,
    event::{DisableBracketedPaste, EnableBracketedPaste},
    execute,
};
use ratatui::DefaultTerminal;
use std::io;

pub(crate) struct Screen {
    pub terminal: DefaultTerminal,
}

fn restore() {
    let _ = execute!(io::stdout(), DisableBracketedPaste, Show);
    let _ = ratatui::try_restore();
}

impl Screen {
    pub fn enter() -> io::Result<Self> {
        let terminal = match ratatui::try_init() {
            Ok(terminal) => terminal,
            Err(error) => {
                restore();
                return Err(error);
            }
        };
        let screen = Self { terminal };
        execute!(io::stdout(), EnableBracketedPaste)?;
        let original = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            restore();
            original(info);
        }));
        Ok(screen)
    }
}
impl Drop for Screen {
    fn drop(&mut self) {
        restore();
    }
}

#[cfg(test)]
mod tests {
    use super::Screen;
    #[test]
    #[ignore = "requires a real PTY; run by the local terminal probe"]
    fn restore_after_unwind_and_error() {
        let result = std::panic::catch_unwind(|| {
            let _screen = Screen::enter().unwrap();
            assert!(crossterm::terminal::is_raw_mode_enabled().unwrap());
            panic!("controlled terminal unwind");
        });
        assert!(result.is_err());
        assert!(!crossterm::terminal::is_raw_mode_enabled().unwrap());
        let result: std::io::Result<()> = (|| {
            let _screen = Screen::enter()?;
            Err(std::io::Error::other("controlled terminal error"))
        })();
        assert!(result.is_err());
        assert!(!crossterm::terminal::is_raw_mode_enabled().unwrap());
    }
}
