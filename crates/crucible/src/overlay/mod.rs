//! The overlay runner shared by `pick`, `launch` and `keys`: [`run_overlay`] owns the socket,
//! the connection, the raw-mode guard, the render → read → decode → step → send loop and the
//! first-frame latency log. An overlay supplies only its domain hooks through [`Overlay`]; the
//! state and frame scaffolding every list overlay shares live in [`list`], the terminal-side
//! helpers in [`term`].

use std::io::{Read, Write};

use crucible_compositor::ipc::{Request, Response, SOCKET_ENV};

use crate::client::{Client, ClientError, socket_from_env};
use crate::style::Style;
use crate::tty::{RawGuard, window_size};

pub mod list;
pub mod term;

use list::ListState;
use term::{Key, decode_keys};

/// What the runner must do after a key or response.
#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    /// Redraw and keep reading.
    Continue,
    /// Leave the overlay with exit code 0.
    Exit,
    /// Send this request, then feed the response to [`Overlay::on_response`].
    Send(Request),
}

/// An overlay's domain hooks; the runner drives them.
pub trait Overlay {
    /// The whole screen for a `cols` x `rows` terminal (raw-mode line endings), painted with the
    /// overlay `style`.
    fn render_styled(&self, cols: u16, rows: u16, style: &Style) -> String;

    /// Applies one key.
    fn step(&mut self, key: Key) -> Outcome;

    /// The shared query / selection / status state.
    fn list_mut(&mut self) -> &mut ListState;

    /// Applies the compositor's answer to the last [`Outcome::Send`]: `ok` leaves, `err` stays
    /// open with the reason in the status line. Shared; overlays do not override it.
    fn on_response(&mut self, resp: &Response) -> Outcome {
        self.list_mut().on_response(resp)
    }
}

/// The reply fed to [`Overlay::on_response`] when the exchange itself failed.
#[must_use]
pub fn error_reply(err: &ClientError) -> Response {
    Response::Err(match err {
        ClientError::BadResponse(line) => format!("bad response {line:?}"),
        ClientError::Io(_) | ClientError::Closed => format!("compositor error: {err}"),
    })
}

/// Runs overlay `name` (the stderr prefix and latency-log label); returns the exit code. `setup`
/// receives the connected client and builds the overlay (it may fetch its rows); its error is
/// printed and exits 1. Exit codes: 2 when `$CDE_SOCKET` is missing, 1 on connect, setup or
/// terminal failure, 0 otherwise.
pub fn run_overlay<O: Overlay>(
    name: &str,
    setup: impl FnOnce(&mut Client) -> Result<O, String>,
) -> i32 {
    let path = match socket_from_env(std::env::var_os(SOCKET_ENV)) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("crucible {name}: {e}");
            return 2;
        }
    };
    let mut client = match Client::connect(&path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("crucible {name}: cannot connect to {}: {e}", path.display());
            return 1;
        }
    };
    let mut overlay = match setup(&mut client) {
        Ok(o) => o,
        Err(e) => {
            eprintln!("crucible {name}: {e}");
            return 1;
        }
    };
    let Some(guard) = RawGuard::enter(true) else {
        eprintln!("crucible {name}: stdin is not a terminal");
        return 1;
    };
    let code = event_loop(name, &mut overlay, &mut client);
    drop(guard);
    code
}

fn event_loop<O: Overlay>(name: &str, overlay: &mut O, client: &mut Client) -> i32 {
    let style = Style::from_env();
    let mut pending: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 256];
    let mut stdin = std::io::stdin();
    loop {
        let (cols, rows) = window_size();
        let mut out = std::io::stdout();
        let _ = out.write_all(overlay.render_styled(cols, rows, &style).as_bytes());
        let _ = out.flush();
        crate::latency::first_frame(name);
        let n = match stdin.read(&mut chunk) {
            Ok(0) => return 0,
            Ok(n) => n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => return 1,
        };
        pending.extend_from_slice(&chunk[..n]);
        for key in decode_keys(&mut pending) {
            let mut outcome = overlay.step(key);
            if let Outcome::Send(req) = &outcome {
                let resp = client.send(req).unwrap_or_else(|e| error_reply(&e));
                outcome = overlay.on_response(&resp);
            }
            if outcome == Outcome::Exit {
                return 0;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exchange_failures_become_err_replies_with_todays_wording() {
        assert_eq!(
            error_reply(&ClientError::BadResponse("???".into())),
            Response::Err("bad response \"???\"".into())
        );
        assert_eq!(
            error_reply(&ClientError::Closed),
            Response::Err("compositor error: compositor closed the connection".into())
        );
        let io = std::io::Error::new(std::io::ErrorKind::TimedOut, "timed out");
        assert_eq!(
            error_reply(&ClientError::Io(io)),
            Response::Err("compositor error: timed out".into())
        );
    }
}
