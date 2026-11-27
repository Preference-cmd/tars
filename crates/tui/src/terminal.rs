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

pub(crate) fn suspend_for_external_editor() -> io::Result<()> {
    use std::io::Write;
    execute!(
        io::stdout(),
        DisableBracketedPaste,
        crossterm::terminal::LeaveAlternateScreen,
        Show
    )?;
    crossterm::terminal::disable_raw_mode()?;
    io::stdout().flush()
}

pub(crate) fn resume_after_external_editor(terminal: &mut DefaultTerminal) -> io::Result<()> {
    use std::io::Write;
    let raw = crossterm::terminal::enable_raw_mode();
    let screen = execute!(
        io::stdout(),
        crossterm::terminal::EnterAlternateScreen,
        EnableBracketedPaste,
        crossterm::cursor::Hide
    );
    let flush = io::stdout().flush();
    let clear = terminal.clear();
    raw.and(screen).and(flush).and(clear)
}

struct RestoreTerminalOnDrop<'a> {
    terminal: Option<&'a mut DefaultTerminal>,
}

impl RestoreTerminalOnDrop<'_> {
    fn restore_now(&mut self) -> io::Result<()> {
        if let Some(terminal) = self.terminal.take() {
            resume_after_external_editor(terminal)
        } else {
            Ok(())
        }
    }
}

impl Drop for RestoreTerminalOnDrop<'_> {
    fn drop(&mut self) {
        if let Some(terminal) = self.terminal.take() {
            let _ = resume_after_external_editor(terminal);
        }
    }
}

pub(crate) fn run_with_suspended_terminal<T>(
    terminal: &mut DefaultTerminal,
    f: impl FnOnce() -> T,
) -> io::Result<T> {
    let mut guard = RestoreTerminalOnDrop {
        terminal: Some(terminal),
    };
    suspend_for_external_editor()?;
    let result = f();
    guard.restore_now()?;
    Ok(result)
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

/// Maximum raw clipboard payload accepted by OSC 52 (`1 MiB`).
pub(crate) const MAX_OSC52_RAW_BYTES: usize = 1024 * 1024;

/// Encode bytes with RFC 4648 standard Base64 (`+`, `/`, `=` padding).
pub(crate) fn encode_base64(input: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    let mut chunks = input.chunks_exact(3);
    for chunk in &mut chunks {
        let n = (u32::from(chunk[0]) << 16) | (u32::from(chunk[1]) << 8) | u32::from(chunk[2]);
        out.push(TABLE[((n >> 18) & 0x3f) as usize] as char);
        out.push(TABLE[((n >> 12) & 0x3f) as usize] as char);
        out.push(TABLE[((n >> 6) & 0x3f) as usize] as char);
        out.push(TABLE[(n & 0x3f) as usize] as char);
    }
    match chunks.remainder() {
        [a] => {
            let n = u32::from(*a) << 16;
            out.push(TABLE[((n >> 18) & 0x3f) as usize] as char);
            out.push(TABLE[((n >> 12) & 0x3f) as usize] as char);
            out.push('=');
            out.push('=');
        }
        [a, b] => {
            let n = (u32::from(*a) << 16) | (u32::from(*b) << 8);
            out.push(TABLE[((n >> 18) & 0x3f) as usize] as char);
            out.push(TABLE[((n >> 12) & 0x3f) as usize] as char);
            out.push(TABLE[((n >> 6) & 0x3f) as usize] as char);
            out.push('=');
        }
        _ => {}
    }
    out
}

/// Write an OSC 52 clipboard sequence (`c` selection) to `writer`, wrapping in
/// a tmux DCS passthrough envelope when `in_tmux` is true.
pub(crate) fn write_osc52_clipboard_with_tmux(
    writer: &mut impl io::Write,
    bytes: &[u8],
    in_tmux: bool,
) -> io::Result<()> {
    if bytes.len() > MAX_OSC52_RAW_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "clipboard payload exceeds 1 MiB limit",
        ));
    }
    let encoded = encode_base64(bytes);
    if in_tmux {
        write!(writer, "\x1bPtmux;\x1b\x1b]52;c;{encoded}\x07\x1b\\")?;
    } else {
        write!(writer, "\x1b]52;c;{encoded}\x07")?;
    }
    writer.flush()
}

/// Write an OSC 52 clipboard sequence to `stdout`, detecting `$TMUX` automatically.
pub(crate) fn write_osc52_clipboard(bytes: &[u8]) -> io::Result<()> {
    let in_tmux = std::env::var_os("TMUX").is_some_and(|v| !v.is_empty());
    let mut stdout = io::stdout().lock();
    write_osc52_clipboard_with_tmux(&mut stdout, bytes, in_tmux)
}

struct RawModeGuard {
    restore: bool,
}

impl RawModeGuard {
    fn enter() -> io::Result<Self> {
        let was_raw = crossterm::terminal::is_raw_mode_enabled().unwrap_or(false);
        if !was_raw {
            crossterm::terminal::enable_raw_mode()?;
        }
        Ok(Self { restore: !was_raw })
    }
}

impl Drop for RawModeGuard {
    fn drop(&mut self) {
        if self.restore {
            let _ = crossterm::terminal::disable_raw_mode();
        }
    }
}

fn read_secret_loop(
    mut next_event: impl FnMut() -> io::Result<crossterm::event::Event>,
) -> io::Result<String> {
    use crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers};

    let mut secret = String::new();
    loop {
        let event = match next_event() {
            Ok(event) => event,
            Err(err) => {
                secret.clear();
                return Err(err);
            }
        };
        match event {
            Event::Key(key) if key.kind != KeyEventKind::Release => match key.code {
                KeyCode::Enter => return Ok(secret),
                KeyCode::Esc => {
                    secret.clear();
                    return Err(io::Error::new(
                        io::ErrorKind::Interrupted,
                        "secret entry cancelled",
                    ));
                }
                KeyCode::Char('c' | 'C') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    secret.clear();
                    return Err(io::Error::new(
                        io::ErrorKind::Interrupted,
                        "secret entry cancelled",
                    ));
                }
                KeyCode::Char('\u{0003}') => {
                    secret.clear();
                    return Err(io::Error::new(
                        io::ErrorKind::Interrupted,
                        "secret entry cancelled",
                    ));
                }
                KeyCode::Backspace => {
                    secret.pop();
                }
                KeyCode::Char(ch)
                    if !key
                        .modifiers
                        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
                        && !ch.is_control() =>
                {
                    secret.push(ch);
                }
                _ => {}
            },
            Event::Paste(text) => {
                secret.extend(text.chars().filter(|ch| !matches!(ch, '\r' | '\n')));
            }
            _ => {}
        }
    }
}

pub(crate) fn read_secret_from_tty(prompt: &str) -> io::Result<String> {
    use std::io::{IsTerminal, Write};
    if !io::stdin().is_terminal() {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "interactive terminal is required to read a secret",
        ));
    }
    let result = {
        let _raw_guard = RawModeGuard::enter()?;
        {
            let mut stderr = io::stderr().lock();
            stderr.write_all(prompt.as_bytes())?;
            stderr.flush()?;
        }
        read_secret_loop(crossterm::event::read)
    };
    let mut stderr = io::stderr().lock();
    let _ = stderr.write_all(b"\n");
    let _ = stderr.flush();
    result
}

#[cfg(test)]
mod tests {
    use super::{
        MAX_OSC52_RAW_BYTES, Screen, SuppressTerminalRestore, encode_base64,
        terminal_restore_allowed, write_osc52_clipboard_with_tmux,
    };
    use std::io;

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

    #[test]
    fn encode_base64_matches_rfc4648_test_vectors_and_utf8() {
        assert_eq!(encode_base64(b""), "");
        assert_eq!(encode_base64(b"f"), "Zg==");
        assert_eq!(encode_base64(b"fo"), "Zm8=");
        assert_eq!(encode_base64(b"foo"), "Zm9v");
        assert_eq!(encode_base64(b"foob"), "Zm9vYg==");
        assert_eq!(encode_base64(b"fooba"), "Zm9vYmE=");
        assert_eq!(encode_base64(b"foobar"), "Zm9vYmFy");
        assert_eq!(encode_base64(&[0x00, 0xff, 0x80, 0x7f]), "AP+Afw==");
        // UTF-8 CJK + emoji ("你好")
        assert_eq!(encode_base64("你好".as_bytes()), "5L2g5aW9");
    }

    #[test]
    fn write_osc52_clipboard_formats_standard_and_tmux_sequences() {
        let mut std_out = Vec::new();
        write_osc52_clipboard_with_tmux(&mut std_out, b"hello", false).unwrap();
        assert_eq!(std_out, b"\x1b]52;c;aGVsbG8=\x07");

        let mut tmux_out = Vec::new();
        write_osc52_clipboard_with_tmux(&mut tmux_out, b"hello", true).unwrap();
        assert_eq!(tmux_out, b"\x1bPtmux;\x1b\x1b]52;c;aGVsbG8=\x07\x1b\\");
    }

    #[test]
    fn write_osc52_clipboard_rejects_payload_exceeding_one_mib() {
        let exact = vec![b'a'; MAX_OSC52_RAW_BYTES];
        let mut ok_buf = Vec::new();
        write_osc52_clipboard_with_tmux(&mut ok_buf, &exact, false).unwrap();
        assert!(ok_buf.starts_with(b"\x1b]52;c;"));
        assert!(ok_buf.ends_with(b"\x07"));

        let oversized = vec![b'a'; MAX_OSC52_RAW_BYTES + 1];
        let mut err_buf = Vec::new();
        let err = write_osc52_clipboard_with_tmux(&mut err_buf, &oversized, false).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
        assert!(err.to_string().contains("1 MiB"));
        assert!(err_buf.is_empty());
    }

    #[test]
    fn read_secret_loop_collects_keystrokes_paste_and_backspace_until_enter() {
        use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
        use std::collections::VecDeque;

        let mut events = VecDeque::from([
            Ok(Event::Key(KeyEvent::new(
                KeyCode::Char('s'),
                KeyModifiers::NONE,
            ))),
            Ok(Event::Key(KeyEvent::new_with_kind(
                KeyCode::Char('x'),
                KeyModifiers::NONE,
                KeyEventKind::Release,
            ))),
            Ok(Event::Key(KeyEvent::new(
                KeyCode::Char('k'),
                KeyModifiers::NONE,
            ))),
            Ok(Event::Key(KeyEvent::new(
                KeyCode::Char('z'),
                KeyModifiers::NONE,
            ))),
            Ok(Event::Key(KeyEvent::new(
                KeyCode::Backspace,
                KeyModifiers::NONE,
            ))),
            Ok(Event::Key(KeyEvent::new(
                KeyCode::Char('-'),
                KeyModifiers::NONE,
            ))),
            Ok(Event::Paste("abc\r\ndef\n".to_owned())),
            Ok(Event::Key(KeyEvent::new(
                KeyCode::Char('A'),
                KeyModifiers::SHIFT,
            ))),
            Ok(Event::Key(KeyEvent::new(
                KeyCode::Char('l'),
                KeyModifiers::CONTROL,
            ))),
            Ok(Event::Key(KeyEvent::new(
                KeyCode::Enter,
                KeyModifiers::NONE,
            ))),
        ]);

        let secret = super::read_secret_loop(|| {
            events
                .pop_front()
                .unwrap_or_else(|| Err(io::Error::other("eof")))
        })
        .unwrap();
        assert_eq!(secret, "sk-abcdefA");
    }

    #[test]
    fn read_secret_loop_returns_interrupted_on_ctrl_c_and_esc() {
        use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
        use std::collections::VecDeque;

        let mut ctrl_c_events = VecDeque::from([
            Ok(Event::Key(KeyEvent::new(
                KeyCode::Char('s'),
                KeyModifiers::NONE,
            ))),
            Ok(Event::Key(KeyEvent::new(
                KeyCode::Char('c'),
                KeyModifiers::CONTROL,
            ))),
        ]);
        let err = super::read_secret_loop(|| ctrl_c_events.pop_front().unwrap()).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::Interrupted);

        let mut esc_events = VecDeque::from([
            Ok(Event::Key(KeyEvent::new(
                KeyCode::Char('s'),
                KeyModifiers::NONE,
            ))),
            Ok(Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))),
        ]);
        let err = super::read_secret_loop(|| esc_events.pop_front().unwrap()).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::Interrupted);
    }

    #[test]
    fn read_secret_from_tty_rejects_non_terminal_without_hanging() {
        if !std::io::IsTerminal::is_terminal(&io::stdin()) {
            let err = super::read_secret_from_tty("Key: ").unwrap_err();
            assert_eq!(err.kind(), io::ErrorKind::Unsupported);
        }
    }
}
