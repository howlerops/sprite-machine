//! The real terminal: raw mode on the alternate screen, put back when the guard drops, when the
//! process panics, and on every other way out; and the key reader.

use std::io::{self, Stdout};
use std::sync::Once;
use std::time::Duration;

use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::crossterm::{cursor, execute};
use tokio::sync::mpsc;

use super::app::Key;

/// How often the key reader looks at whether the UI is still there.
const POLL: Duration = Duration::from_millis(200);

/// The terminal in TUI mode. Dropping it restores the terminal.
pub struct TerminalGuard {
    terminal: Terminal<CrosstermBackend<Stdout>>,
}

impl TerminalGuard {
    /// Raw mode and the alternate screen. A panic from here on restores the terminal first, so
    /// its message is readable.
    pub fn enter() -> io::Result<Self> {
        install_panic_hook();
        enable_raw_mode()?;
        let mut out = io::stdout();
        if let Err(error) = execute!(out, EnterAlternateScreen) {
            restore();
            return Err(error);
        }
        match Terminal::new(CrosstermBackend::new(out)) {
            Ok(terminal) => Ok(Self { terminal }),
            Err(error) => {
                restore();
                Err(error)
            }
        }
    }

    pub fn terminal(&mut self) -> &mut Terminal<CrosstermBackend<Stdout>> {
        &mut self.terminal
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        restore();
    }
}

/// Leaves the alternate screen, shows the cursor and turns raw mode off. Safe to call twice and
/// without having entered; errors are ignored because there is nothing left to do about them.
pub fn restore() {
    let _ = disable_raw_mode();
    let _ = execute!(io::stdout(), LeaveAlternateScreen, cursor::Show);
}

/// Makes a panic restore the terminal before the default hook prints its message (once per
/// process, however often the terminal is entered).
fn install_panic_hook() {
    static HOOK: Once = Once::new();
    HOOK.call_once(|| panic_hook_with(restore));
}

/// Puts a panic hook in front of the current one: `restore` runs first, then the hook that was
/// there, so the message is printed on a terminal that works.
pub fn panic_hook_with(restore: impl Fn() + Send + Sync + 'static) {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        restore();
        previous(info);
    }));
}

/// A thread that reads the terminal's key events and sends them on. It ends when the receiver is
/// dropped (within [`POLL`]).
pub fn read_keys() -> mpsc::Receiver<Key> {
    let (tx, rx) = mpsc::channel(64);
    std::thread::spawn(move || {
        while !tx.is_closed() {
            match event::poll(POLL) {
                Ok(true) => {}
                Ok(false) => continue,
                Err(_) => return,
            }
            match event::read() {
                Ok(Event::Key(key)) => {
                    if let Some(key) = map_key(key)
                        && tx.blocking_send(key).is_err()
                    {
                        return;
                    }
                }
                Ok(_) => {}
                Err(_) => return,
            }
        }
    });
    rx
}

/// A crossterm key event as a TUI key. Releases and repeats of other keys are not keys here.
pub fn map_key(event: KeyEvent) -> Option<Key> {
    if event.kind == KeyEventKind::Release {
        return None;
    }
    let ctrl = event.modifiers.contains(KeyModifiers::CONTROL);
    Some(match event.code {
        KeyCode::Char(c) if ctrl => Key::Ctrl(c.to_ascii_lowercase()),
        KeyCode::Char(c) => Key::Char(c),
        KeyCode::Enter => Key::Enter,
        KeyCode::Backspace => Key::Backspace,
        KeyCode::Esc => Key::Esc,
        KeyCode::Up => Key::Up,
        KeyCode::Down => Key::Down,
        KeyCode::PageUp => Key::PageUp,
        KeyCode::PageDown => Key::PageDown,
        _ => return None,
    })
}
