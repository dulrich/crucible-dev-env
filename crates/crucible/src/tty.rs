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

extern "C" fn noop_handler(_: libc::c_int) {}

/// Installs a do-nothing handler for `sig`. With `restart` a blocking read resumes after the
/// signal (`SA_RESTART`); without it the read returns `Interrupted`, which is how a loop learns of
/// the signal. A handler (not `SIG_IGN`) resets to the default across `exec`, so spawned commands
/// get normal signal semantics. Reports why it failed.
pub fn install_noop_handler(sig: libc::c_int, restart: bool) -> std::io::Result<()> {
    // SAFETY: `sigaction` is called with a zero-initialised (valid) `sigaction` struct whose
    // handler is an `extern "C"` function that does nothing, which is trivially
    // async-signal-safe; the mask is emptied via `sigemptyset` on a valid pointer and the old
    // action pointer is null, which `sigaction` permits.
    #[allow(unsafe_code)]
    unsafe {
        let mut act: libc::sigaction = std::mem::zeroed();
        act.sa_sigaction = noop_handler as extern "C" fn(libc::c_int) as libc::sighandler_t;
        act.sa_flags = if restart { libc::SA_RESTART } else { 0 };
        libc::sigemptyset(&raw mut act.sa_mask);
        if libc::sigaction(sig, &raw const act, std::ptr::null_mut()) != 0 {
            return Err(std::io::Error::last_os_error());
        }
    }
    Ok(())
}

/// Makes a terminal resize interrupt a blocking stdin read, so an overlay redraws at the new size
/// at once. An X11 terminal (urxvt) maps at its own default size and is resized afterwards, and on
/// a height shrink it drops the top lines; without a redraw the overlay stays clipped until a key.
pub fn redraw_on_resize() -> std::io::Result<()> {
    install_noop_handler(libc::SIGWINCH, false)
}

#[cfg(test)]
mod signal_tests {
    use super::install_noop_handler;

    #[test]
    fn an_interrupting_handler_makes_a_blocking_read_return_interrupted() {
        use std::io::Read;
        use std::os::unix::net::UnixStream;
        // SIGUSR2 is unused by the test harness; the handler is installed without SA_RESTART.
        install_noop_handler(libc::SIGUSR2, false).unwrap();
        let (mut reader, _writer) = UnixStream::pair().unwrap();
        // SAFETY: `pthread_self` has no preconditions; the id is only used by `pthread_kill`
        // below, while this thread is still alive (it is blocked in `read`).
        #[allow(unsafe_code)]
        let me = unsafe { libc::pthread_self() } as usize;
        let killer = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(50));
            // SAFETY: `me` is the live test thread blocked in `read`; SIGUSR2 has a no-op handler.
            #[allow(unsafe_code)]
            unsafe {
                libc::pthread_kill(me as libc::pthread_t, libc::SIGUSR2);
            }
        });
        let mut buf = [0u8; 8];
        let err = reader.read(&mut buf).unwrap_err();
        killer.join().unwrap();
        assert_eq!(err.kind(), std::io::ErrorKind::Interrupted);
    }
}
