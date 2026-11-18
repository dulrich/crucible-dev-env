//! The compositor-side IPC socket server: bind, per-connection reply ordering and the calloop source.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::io::{ErrorKind, Read, Write};
use std::os::unix::fs::DirBuilderExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::Duration;

use smithay::reexports::calloop::channel::{Channel, Event};
use smithay::reexports::calloop::{Interest, Mode, PostAction, generic::Generic};
use tracing::{debug, info, warn};

use crate::state::{Backend, CrucibleState};

use super::{Request, Response};

/// Longest request line accepted before the connection is dropped.
const MAX_LINE: usize = 64 * 1024;

/// The IPC socket path for a compositor on Wayland socket `wayland_name`:
/// `<runtime_dir>/cde/<wayland_name>.sock`.
#[must_use]
pub fn socket_path(runtime_dir: &Path, wayland_name: &str) -> PathBuf {
    runtime_dir.join("cde").join(format!("{wayland_name}.sock"))
}

/// Binds the listening socket at `path`: creates the parent directory (mode 0700) and replaces a
/// stale socket file only when connecting to it fails (a live CDE keeps its socket).
pub fn bind_socket(path: &Path) -> std::io::Result<UnixListener> {
    if let Some(dir) = path.parent() {
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)?;
    }
    if path.exists() {
        if UnixStream::connect(path).is_ok() {
            return Err(std::io::Error::new(
                ErrorKind::AddrInUse,
                "another compositor is serving this socket",
            ));
        }
        std::fs::remove_file(path)?;
    }
    let listener = UnixListener::bind(path)?;
    listener.set_nonblocking(true)?;
    Ok(listener)
}

/// One IPC request's answer: immediate, or deferred until a worker sends it on the channel
/// (`capture` encodes and writes its PNG off the main thread).
pub enum Reply {
    /// Answered now.
    Now(Response),
    /// Answered later by the worker on this channel's single message.
    Later(Channel<Response>),
}

/// A pending slot in an [`Outbox`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ticket(u64);

/// Per-connection replies in request arrival order: a reply is released only once every earlier
/// one is ready, so a deferred `capture` holds later replies on its connection behind it.
#[derive(Debug, Default)]
pub struct Outbox {
    next: u64,
    slots: VecDeque<(u64, Option<Response>)>,
}

impl Outbox {
    /// Queues an answered reply.
    pub fn push_ready(&mut self, response: Response) {
        self.slots.push_back((self.next, Some(response)));
        self.next += 1;
    }

    /// Queues an empty slot to be filled by [`Outbox::fulfil`].
    pub fn push_pending(&mut self) -> Ticket {
        self.slots.push_back((self.next, None));
        self.next += 1;
        Ticket(self.next - 1)
    }

    /// Fills `ticket`'s slot (a stale ticket is ignored).
    pub fn fulfil(&mut self, ticket: Ticket, response: Response) {
        if let Some(slot) = self.slots.iter_mut().find(|(id, _)| *id == ticket.0) {
            slot.1 = Some(response);
        }
    }

    /// Removes and returns the ready prefix, in arrival order.
    pub fn drain_ready(&mut self) -> Vec<Response> {
        let mut out = Vec::new();
        while let Some((_, Some(_))) = self.slots.front() {
            if let Some((_, Some(r))) = self.slots.pop_front() {
                out.push(r);
            }
        }
        out
    }
}

/// The writing half of one IPC connection, shared by its read source and its pending replies.
struct ConnOut {
    io: UnixStream,
    outbox: Outbox,
    closed: bool,
}

impl ConnOut {
    /// Writes the outbox's ready prefix (main loop only); a failed write closes the connection.
    fn flush(&mut self) -> bool {
        if self.closed {
            return false;
        }
        let ready = self.outbox.drain_ready();
        if ready.is_empty() {
            return true;
        }
        // The socket is non-blocking for reads; replies are small, so a short blocking write
        // (bounded by the connection's write timeout) is acceptable.
        let _ = self.io.set_nonblocking(false);
        let ok = ready
            .iter()
            .all(|r| writeln!(self.io, "{}", r.to_line()).is_ok());
        let _ = self.io.set_nonblocking(true);
        if !ok {
            self.closed = true;
        }
        ok
    }
}

/// Splits every complete line off the front of `buf`, leaving a partial tail buffered.
#[must_use]
pub fn take_lines(buf: &mut Vec<u8>) -> Vec<String> {
    let mut lines = Vec::new();
    while let Some(i) = buf.iter().position(|b| *b == b'\n') {
        let line: Vec<u8> = buf.drain(..=i).collect();
        lines.push(String::from_utf8_lossy(&line[..i]).into_owned());
    }
    lines
}

impl<B: Backend + 'static> CrucibleState<B> {
    /// Binds `$XDG_RUNTIME_DIR/cde/<wayland-socket>.sock` and drives it from calloop. Records the
    /// path in `persist.ipc_path` (exported to children as [`super::SOCKET_ENV`]). Failures are logged and
    /// leave IPC off.
    pub fn start_ipc(&mut self) {
        let (Some(runtime), Some(name)) = (
            std::env::var_os("XDG_RUNTIME_DIR"),
            self.socket_name.clone(),
        ) else {
            warn!("no XDG_RUNTIME_DIR or wayland socket; IPC disabled");
            return;
        };
        let path = socket_path(Path::new(&runtime), &name);
        let listener = match bind_socket(&path) {
            Ok(l) => l,
            Err(err) => {
                warn!(path = %path.display(), %err, "cannot bind the IPC socket; IPC disabled");
                return;
            }
        };
        let Ok(accept) = listener.try_clone() else {
            warn!("cannot clone the IPC listener; IPC disabled");
            return;
        };
        let inserted = self.handle.insert_source(
            Generic::new(listener, Interest::READ, Mode::Level),
            move |_, _, data| {
                loop {
                    match accept.accept() {
                        Ok((stream, _)) => data.add_ipc_connection(stream),
                        Err(e) if e.kind() == ErrorKind::WouldBlock => break,
                        Err(err) => {
                            warn!(%err, "IPC accept failed");
                            break;
                        }
                    }
                }
                Ok(PostAction::Continue)
            },
        );
        if let Err(err) = inserted {
            warn!(%err, "cannot register the IPC socket source; IPC disabled");
            return;
        }
        info!(path = %path.display(), "IPC socket listening");
        self.persist.ipc_path = Some(path);
    }

    fn add_ipc_connection(&mut self, stream: UnixStream) {
        let (Ok(mut io), Ok(out)) = (stream.try_clone(), stream.try_clone()) else {
            return;
        };
        if stream.set_nonblocking(true).is_err() {
            return;
        }
        let _ = out.set_write_timeout(Some(Duration::from_millis(200)));
        let peer = rustix::net::sockopt::socket_peercred(&stream)
            .ok()
            .and_then(|c| u32::try_from(c.pid.as_raw_nonzero().get()).ok());
        let conn = Rc::new(RefCell::new(ConnOut {
            io: out,
            outbox: Outbox::default(),
            closed: false,
        }));
        let mut buf = Vec::new();
        let res = self.handle.insert_source(
            Generic::new(stream, Interest::READ, Mode::Level),
            move |_, _, data| {
                let close = |conn: &RefCell<ConnOut>| {
                    conn.borrow_mut().closed = true;
                    Ok(PostAction::Remove)
                };
                let mut chunk = [0u8; 4096];
                loop {
                    match io.read(&mut chunk) {
                        Ok(0) => return close(&conn),
                        Ok(n) => buf.extend_from_slice(&chunk[..n]),
                        Err(e) if e.kind() == ErrorKind::WouldBlock => break,
                        Err(e) if e.kind() == ErrorKind::Interrupted => {}
                        Err(_) => return close(&conn),
                    }
                }
                for line in take_lines(&mut buf) {
                    let reply = match Request::parse(&line) {
                        Ok(req) => {
                            debug!(?req, "IPC request");
                            data.handle_ipc(req, peer)
                        }
                        Err(reason) => Reply::Now(Response::Err(reason)),
                    };
                    match reply {
                        Reply::Now(response) => conn.borrow_mut().outbox.push_ready(response),
                        Reply::Later(rx) => {
                            let ticket = conn.borrow_mut().outbox.push_pending();
                            let pending = Rc::clone(&conn);
                            let inserted = data.handle.insert_source(rx, move |event, (), _| {
                                if let Event::Msg(response) = event {
                                    let mut conn = pending.borrow_mut();
                                    // A closed connection drops the worker's reply quietly.
                                    if !conn.closed {
                                        conn.outbox.fulfil(ticket, response);
                                        conn.flush();
                                    }
                                }
                            });
                            if let Err(err) = inserted {
                                warn!(%err, "cannot register an IPC reply channel");
                                conn.borrow_mut().outbox.fulfil(
                                    ticket,
                                    Response::Err("cannot register the reply channel".into()),
                                );
                            }
                        }
                    }
                }
                if !conn.borrow_mut().flush() {
                    return close(&conn);
                }
                if buf.len() > MAX_LINE {
                    warn!("IPC request line too long; dropping the connection");
                    return close(&conn);
                }
                Ok(PostAction::Continue)
            },
        );
        if let Err(err) = res {
            warn!(%err, "cannot register an IPC connection");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ok(s: &str) -> Response {
        Response::Ok(s.into())
    }

    #[test]
    fn outbox_holds_replies_behind_a_pending_capture() {
        let mut outbox = Outbox::default();
        let capture = outbox.push_pending();
        outbox.push_ready(ok("pong"));
        assert!(outbox.drain_ready().is_empty());
        outbox.fulfil(capture, ok("4x3"));
        assert_eq!(outbox.drain_ready(), vec![ok("4x3"), ok("pong")]);
        assert!(outbox.drain_ready().is_empty());
    }

    #[test]
    fn outbox_drains_out_of_order_fulfilments_in_arrival_order() {
        let mut outbox = Outbox::default();
        let first = outbox.push_pending();
        let second = outbox.push_pending();
        outbox.fulfil(second, ok("2"));
        assert!(outbox.drain_ready().is_empty());
        outbox.fulfil(first, ok("1"));
        assert_eq!(outbox.drain_ready(), vec![ok("1"), ok("2")]);
    }

    #[test]
    fn outbox_releases_ready_replies_up_to_the_first_pending() {
        let mut outbox = Outbox::default();
        outbox.push_ready(ok("a"));
        let pending = outbox.push_pending();
        outbox.push_ready(ok("b"));
        assert_eq!(outbox.drain_ready(), vec![ok("a")]);
        outbox.fulfil(pending, ok("p"));
        assert_eq!(outbox.drain_ready(), vec![ok("p"), ok("b")]);
    }
    #[test]
    fn take_lines_keeps_the_partial_tail() {
        let mut buf = b"save\ncandi".to_vec();
        assert_eq!(take_lines(&mut buf), vec!["save".to_string()]);
        assert_eq!(buf, b"candi");
        buf.extend_from_slice(b"dates\n");
        assert_eq!(take_lines(&mut buf), vec!["candidates".to_string()]);
        assert!(buf.is_empty());
    }

    #[test]
    fn socket_path_and_stale_replacement() {
        let dir = std::env::temp_dir().join(format!("cde-ipc-test-{}", std::process::id()));
        let path = socket_path(&dir, "wayland-9");
        assert_eq!(path, dir.join("cde").join("wayland-9.sock"));
        // A stale (non-listening) file is replaced.
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, b"").unwrap();
        let live = bind_socket(&path).expect("stale file replaced");
        // A live socket is not.
        assert!(bind_socket(&path).is_err());
        drop(live);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
