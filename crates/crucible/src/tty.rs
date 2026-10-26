//! Terminal mode control shared by `pick` and `resume`: raw mode through `rustix::termios`,
//! restored on every exit path (Drop guard plus a panic hook), and the window size.

use std::io::Write;
use std::sync::Mutex;

use rustix::stdio::stdin;
use rustix::termios::{OptionalActions, Termios, tcgetattr, tcgetwinsize, tcsetattr};

/// The termios to restore if a panic unwinds past the guard (or aborts the main thread).
static SAVED: Mutex<Option<Termios>> = Mutex::new(None);
/// Escape sequences a panic restores in addition to termios (set by the picker).
static SCREEN_RESTORE: Mutex<String> = Mutex::new(String::new());

/// Alternate screen on, cursor hidden.
pub const ENTER_SCREEN: &str = "\x1b[?1049h\x1b[?25l";
/// Cursor shown, alternate screen off.
pub const LEAVE_SCREEN: &str = "\x1b[?25h\x1b[?1049l";

/// Puts stdin in raw mode for its lifetime; dropping restores the saved termios (and leaves the
/// alternate screen if it was entered).
pub struct RawGuard {
    saved: Termios,
    screen: bool,
    leave: String,
}

impl RawGuard {
    /// Enters raw mode on stdin, optionally switching to the alternate screen. `None` when stdin
    /// is not a terminal. Installs the panic hook that restores everything.
    pub fn enter(alt_screen: bool) -> Option<RawGuard> {
        let saved = tcgetattr(stdin()).ok()?;
        let mut raw = saved.clone();
        raw.make_raw();
        tcsetattr(stdin(), OptionalActions::Flush, &raw).ok()?;
        *SAVED.lock().unwrap_or_else(|e| e.into_inner()) = Some(saved.clone());
        let leave = format!("{}{LEAVE_SCREEN}", crate::style::Style::from_env().reset());
        if alt_screen {
            *SCREEN_RESTORE.lock().unwrap_or_else(|e| e.into_inner()) = leave.clone();
            let mut out = std::io::stdout();
            let _ = out.write_all(ENTER_SCREEN.as_bytes());
            let _ = out.flush();
        }
        install_panic_hook();
        Some(RawGuard {
            saved,
            screen: alt_screen,
            leave,
        })
    }
}

impl Drop for RawGuard {
    fn drop(&mut self) {
        if self.screen {
            let mut out = std::io::stdout();
            let _ = out.write_all(self.leave.as_bytes());
            let _ = out.flush();
        }
        let _ = tcsetattr(stdin(), OptionalActions::Flush, &self.saved);
        *SAVED.lock().unwrap_or_else(|e| e.into_inner()) = None;
        *SCREEN_RESTORE.lock().unwrap_or_else(|e| e.into_inner()) = String::new();
    }
}

/// Wraps the current panic hook so the saved termios and screen are restored first.
fn install_panic_hook() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let default = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            let screen = SCREEN_RESTORE
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone();
            if !screen.is_empty() {
                let mut out = std::io::stdout();
                let _ = out.write_all(screen.as_bytes());
                let _ = out.flush();
            }
            if let Some(t) = SAVED.lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
                let _ = tcsetattr(stdin(), OptionalActions::Flush, t);
            }
            default(info);
        }));
    });
}

/// Terminal size as `(cols, rows)`, falling back to 80x24.
#[must_use]
pub fn window_size() -> (u16, u16) {
    match tcgetwinsize(stdin()) {
        Ok(w) if w.ws_col > 0 && w.ws_row > 0 => (w.ws_col, w.ws_row),
        _ => (80, 24),
    }
}
