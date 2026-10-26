use crossterm::{
    cursor::Show,
    event::{DisableBracketedPaste, EnableBracketedPaste},
    execute,
};
use ratatui::DefaultTerminal;
use std::cell::Cell;
use std::io;

pub(crate) struct Screen {
    pub terminal: DefaultTerminal,
}

thread_local! {
    // Set only while a UI adapter future is being polled. A panic there is
    // caught and reported in the interface, so terminal restoration must wait
    // for the normal event-loop exit instead of tearing down raw mode.
    static SUPPRESS_TERMINAL_RESTORE: Cell<bool> = const { Cell::new(false) };
}

pub(crate) struct SuppressTerminalRestore {
    previous: bool,
}

impl SuppressTerminalRestore {
    pub(crate) fn enter() -> Self {
        let previous = SUPPRESS_TERMINAL_RESTORE.with(|flag| flag.replace(true));
        Self { previous }
    }
}

impl Drop for SuppressTerminalRestore {
    fn drop(&mut self) {
        SUPPRESS_TERMINAL_RESTORE.with(|flag| flag.set(self.previous));
    }
}

pub(crate) fn terminal_restore_allowed() -> bool {
    !SUPPRESS_TERMINAL_RESTORE.with(Cell::get)
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
            if terminal_restore_allowed() {
                restore();
                original(info);
            }
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
    use super::{Screen, SuppressTerminalRestore, terminal_restore_allowed};

    #[test]
    fn suppression_guard_toggles_only_for_its_scope() {
        assert!(terminal_restore_allowed());
        {
            let _guard = SuppressTerminalRestore::enter();
            assert!(!terminal_restore_allowed());
        }
        assert!(terminal_restore_allowed());
    }

    #[test]
    #[ignore = "requires a real PTY; run with --test-threads=1"]
    fn caught_job_panic_keeps_raw_mode_until_normal_exit() {
        let _screen = Screen::enter().unwrap();
        let result = std::panic::catch_unwind(|| {
            let _guard = SuppressTerminalRestore::enter();
            panic!("controlled job panic");
        });
        assert!(result.is_err());
        assert!(
            crossterm::terminal::is_raw_mode_enabled().unwrap(),
            "a caught job panic must not restore the terminal"
        );
    }

    #[test]
    #[ignore = "requires a real PTY; run with --test-threads=1"]
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
