//! The `CDE_SOCKET` IPC protocol: a pure codec shared with the `crucible` binary, plus the
//! compositor's non-blocking Unix-socket server driven by calloop.
//!
//! ## Wire format (plan `03-persistence`, Public Interfaces)
//! UTF-8, one request line → one response line (`\n`-terminated). A request is a verb, optionally
//! followed by **one space** and an argument (the rest of the line). Responses are `ok`,
//! `ok <payload>` or `err <reason>`.
//!
//! **Encoding:** every name, path and reason — in requests and responses alike — goes through
//! [`encode_field`], which percent-encodes exactly `%`, `:`, TAB, LF and CR (`%25`, `%3A`, `%09`,
//! `%0A`, `%0D`); every other byte passes through. [`decode_field`] is its exact inverse. The same
//! rule encodes the argument of `crucible pick --rename <current>`. The codec itself lives in
//! `crucible_persist::field` and is re-exported here.
//!
//! | Request (`Request`)        | Response                                                  |
//! |----------------------------|-----------------------------------------------------------|
//! | `candidates`               | `ok ` + TAB-separated [`Candidate`]s (`kind:name[:path]`) |
//! | `open <name>`              | `ok` / `err <reason>`                                     |
//! | `open-folder <path>`       | `ok` / `err <reason>`                                     |
//! | `new <name>`               | `ok` / `err <reason>`                                     |
//! | `rename <name>`            | `ok` / `err <reason>`                                     |
//! | `root <path>` / `root -`   | `ok` / `err <reason>` (set / clear the active workspace's root) |
//! | `bindings`                 | `ok ` + TAB-separated `chord:action` pairs (fields encoded) |
//! | `save`                     | `ok`                                                      |
//! | `run <command>`            | `ok` (spawned) / `err <reason>`                           |
//! | `relaunch`                 | `ok` (placeholder swapped for its app) / `err <reason>`   |
//! | `close-pid <n>`            | `ok <count>` / `err no window for pid <n>`                |
//! | `capture <path>`           | `ok <w>x<h>` (PNG of the focused output written) / `err <reason>` |
//!
//! `relaunch` is the only request that depends on the caller: the compositor reads the peer PID
//! (`SO_PEERCRED`) and accepts it only from an app relaunch placeholder (`crucible relaunch`).
//!
//! An empty candidate list is answered with a bare `ok`.
//!
//! The pure items ([`encode_field`], [`decode_field`], [`Request`], [`Response`], [`Candidate`])
//! are what the `crucible pick`/`msg` client side imports; the socket server ([`bind_socket`],
//! [`CrucibleState::start_ipc`]) is compositor-only.

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

/// Environment variable carrying the IPC socket path to every child CDE spawns.
pub const SOCKET_ENV: &str = "CDE_SOCKET";

/// Environment variable carrying the terminal program to every child CDE spawns.
pub const TERMINAL_ENV: &str = "CDE_TERMINAL";

/// Environment variable carrying the terminal's exec flag (`-e`) to every child CDE spawns.
pub const TERMINAL_EXEC_FLAG_ENV: &str = "CDE_TERMINAL_EXEC_FLAG";

/// Environment variable carrying the configured overlay style, set on overlay spawns only.
pub const OVERLAY_STYLE_ENV: &str = "CDE_OVERLAY_STYLE";

/// Environment variable carrying the `launcher { tui … }` entries (program names / `*` globs)
/// joined by `:`, set on overlay spawns only. Unset or empty means no configured entries (the
/// launcher's built-in list still applies). Entries never contain `/` or `:`.
pub const LAUNCHER_TUI_ENV: &str = "CDE_LAUNCHER_TUI";

/// Environment variable carrying the wall-clock spawn time (ms since the UNIX epoch), set on
/// overlay spawns only, so the overlay can log its spawn-to-first-usable-frame latency.
pub const SPAWN_EPOCH_ENV: &str = "CDE_SPAWN_EPOCH_MS";

/// Longest request line accepted before the connection is dropped.
const MAX_LINE: usize = 64 * 1024;

/// The field codec lives in `crucible_persist::field` (it also encodes `crucible pick`'s
/// argument); re-exported here under the same names for the IPC protocol and its clients.
pub use crucible_persist::field::{decode_field, encode_field};

/// One IPC request (decoded arguments).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Request {
    /// `candidates` — list live, saved and folder candidates.
    Candidates,
    /// `bindings` — list the effective keybindings as `(chord, action)` display pairs.
    Bindings,
    /// `open <name>` — switch to a live workspace or restore a saved one.
    Open(String),
    /// `open-folder <path>` — create (or reopen) a workspace rooted at a folder.
    OpenFolder(PathBuf),
    /// `new <name>` — create and persist an empty named workspace.
    New(String),
    /// `rename <name>` — rename the active workspace (and its saved file).
    Rename(String),
    /// `delete <name>` — delete a *saved, not live* workspace (its record moves to `deleted/` and
    /// its slot pins clear); a live name is refused (use `delete-workspace`), an unknown one errs.
    Delete(String),
    /// `root <path>` / `root -` — set (`Some`) or clear (`None`) the active workspace's root, the
    /// folder its new terminals and apps start in. Not a picker root (`picker { root … }`).
    SetRoot(Option<PathBuf>),
    /// `save` — flush every dirty workspace now.
    Save,
    /// `exec <action>` — run an action text (the `crucible_keys::Action` grammar, e.g.
    /// `run <command>` or `run-in-terminal <command>`) as if its chord was pressed; a failed action
    /// answers `err <reason>`.
    Exec(String),
    /// `run-tui <command>` — like `exec run-in-terminal`, but the terminal closes when the command
    /// exits (`crucible resume --now --exit`), unless the command exits abnormally: `resume` then shows the crash banner first.
    RunTui(String),
    /// `run-term <program>` — spawn the bare emulator `program` (no arguments) as a CDE terminal in
    /// the focused pane; the launcher's TERM rows.
    RunTerm(String),
    /// `relaunch` — swap the calling placeholder window (found by peer PID) for its app.
    Relaunch,
    /// `close-pid <n>` — politely ask every managed window of process `n` to close (xdg `close` /
    /// X11 `WM_DELETE_WINDOW`; an X11 window without it is destroyed); answers `ok <count>`.
    ClosePid(u32),
    /// `capture <path>` — write a PNG of the focused output (no cursor) to `path`; answers
    /// `ok <w>x<h>` once the file is written. Refused while locked. A debug request (plan 34).
    Capture(PathBuf),
}

/// Parses the pid argument of `close-pid`: plain decimal digits, a non-zero `u32`.
fn parse_pid(verb: &str, arg: Option<&str>) -> Result<u32, String> {
    let arg = arg.ok_or_else(|| format!("{verb} needs an argument"))?;
    if arg.is_empty() || !arg.bytes().all(|b| b.is_ascii_digit()) {
        return Err(format!("{verb} needs a decimal pid, got {arg:?}"));
    }
    match arg.parse::<u32>() {
        Ok(0) => Err(format!("{verb} needs a non-zero pid")),
        Ok(n) => Ok(n),
        Err(_) => Err(format!("{verb} pid {arg:?} is out of range")),
    }
}

impl Request {
    /// `exec run <cmd>`: run a shell command line into the focused pane.
    #[must_use]
    pub fn run(cmd: impl Into<String>) -> Request {
        Request::Exec(crucible_keys::Action::Run(cmd.into()).to_string())
    }

    /// `exec run-in-terminal <cmd>`: run a shell command line in a new terminal.
    #[must_use]
    pub fn run_in_terminal(cmd: impl Into<String>) -> Request {
        Request::Exec(crucible_keys::Action::RunInTerminal(cmd.into()).to_string())
    }

    /// Whether this request acts (changes the model, spawns, or touches overlays). Acting requests
    /// are refused while the session is locked; `candidates`, `bindings` and `save` are not acting.
    #[must_use]
    pub fn acts(&self) -> bool {
        match self {
            Request::Candidates | Request::Bindings | Request::Save => false,
            Request::Open(_)
            | Request::OpenFolder(_)
            | Request::New(_)
            | Request::Rename(_)
            | Request::Delete(_)
            | Request::SetRoot(_)
            | Request::Exec(_)
            | Request::RunTui(_)
            | Request::RunTerm(_)
            | Request::Relaunch
            | Request::ClosePid(_)
            | Request::Capture(_) => true,
        }
    }

    /// Parses one request line (without its trailing newline; a trailing `\r` is not stripped
    /// because CR is always encoded). Errors carry a human-readable reason.
    pub fn parse(line: &str) -> Result<Request, String> {
        let (verb, arg) = match line.split_once(' ') {
            Some((v, a)) => (v, Some(a)),
            None => (line, None),
        };
        let need = |arg: Option<&str>| -> Result<String, String> {
            let arg = arg.ok_or_else(|| format!("{verb} needs an argument"))?;
            let decoded =
                decode_field(arg).ok_or_else(|| "malformed percent-encoding".to_string())?;
            if decoded.is_empty() {
                return Err(format!("{verb} needs a non-empty argument"));
            }
            Ok(decoded)
        };
        let none = |arg: Option<&str>, req: Request| match arg {
            None => Ok(req),
            Some(_) => Err(format!("{verb} takes no argument")),
        };
        match verb {
            "candidates" => none(arg, Request::Candidates),
            "bindings" => none(arg, Request::Bindings),
            "save" => none(arg, Request::Save),
            "relaunch" => none(arg, Request::Relaunch),
            "open" => need(arg).map(Request::Open),
            "open-folder" => need(arg).map(|p| Request::OpenFolder(PathBuf::from(p))),
            "new" => need(arg).map(Request::New),
            "rename" => need(arg).map(Request::Rename),
            "delete" => need(arg).map(Request::Delete),
            "root" => need(arg).map(|p| Request::SetRoot((p != "-").then(|| PathBuf::from(p)))),
            "exec" => need(arg).map(Request::Exec),
            "run-tui" => need(arg).map(Request::RunTui),
            "run-term" => need(arg).map(Request::RunTerm),
            "close-pid" => parse_pid(verb, arg).map(Request::ClosePid),
            "capture" => need(arg).map(|p| Request::Capture(PathBuf::from(p))),
            other => Err(format!("unknown request {other:?}")),
        }
    }

    /// Formats the request as one wire line (no trailing newline). Paths must be UTF-8 (lossy
    /// otherwise).
    #[must_use]
    pub fn to_line(&self) -> String {
        match self {
            Request::Candidates => "candidates".into(),
            Request::Bindings => "bindings".into(),
            Request::Save => "save".into(),
            Request::Relaunch => "relaunch".into(),
            Request::Open(n) => format!("open {}", encode_field(n)),
            Request::OpenFolder(p) => format!("open-folder {}", encode_field(&p.to_string_lossy())),
            Request::New(n) => format!("new {}", encode_field(n)),
            Request::Rename(n) => format!("rename {}", encode_field(n)),
            Request::Delete(n) => format!("delete {}", encode_field(n)),
            Request::SetRoot(None) => "root -".into(),
            Request::SetRoot(Some(p)) => format!("root {}", encode_field(&p.to_string_lossy())),
            Request::Exec(a) => format!("exec {}", encode_field(a)),
            Request::RunTui(c) => format!("run-tui {}", encode_field(c)),
            Request::RunTerm(c) => format!("run-term {}", encode_field(c)),
            Request::ClosePid(n) => format!("close-pid {n}"),
            Request::Capture(p) => format!("capture {}", encode_field(&p.to_string_lossy())),
        }
    }
}

/// One IPC response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Response {
    /// Success; the payload is already wire-formatted (e.g. [`format_candidates`]) and may be
    /// empty (`ok`).
    Ok(String),
    /// Failure with a decoded, human-readable reason.
    Err(String),
}

impl Response {
    /// Formats the response as one wire line (no trailing newline).
    #[must_use]
    pub fn to_line(&self) -> String {
        match self {
            Response::Ok(p) if p.is_empty() => "ok".into(),
            Response::Ok(p) => format!("ok {p}"),
            Response::Err(r) => format!("err {}", encode_field(r)),
        }
    }

    /// Parses one response line; `None` if it is neither `ok…` nor `err …`.
    #[must_use]
    pub fn parse(line: &str) -> Option<Response> {
        if line == "ok" {
            return Some(Response::Ok(String::new()));
        }
        if let Some(p) = line.strip_prefix("ok ") {
            return Some(Response::Ok(p.to_string()));
        }
        line.strip_prefix("err ")
            .and_then(decode_field)
            .map(Response::Err)
    }
}

/// The kind of a picker candidate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CandidateKind {
    /// A workspace live in this compositor.
    Live,
    /// A saved workspace that is not live.
    Saved,
    /// A folder under a configured picker root.
    Folder,
}

impl CandidateKind {
    fn as_str(self) -> &'static str {
        match self {
            CandidateKind::Live => "live",
            CandidateKind::Saved => "saved",
            CandidateKind::Folder => "folder",
        }
    }
}

/// One `candidates` item: `kind:name[:path]` with `name`/`path` encoded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    /// Live, saved or folder.
    pub kind: CandidateKind,
    /// Workspace name (folders: the folder's basename).
    pub name: String,
    /// Folder path, or a live workspace's root; `None` otherwise.
    pub path: Option<PathBuf>,
    /// The `Super+<n>` slot the workspace is pinned to (0-9), when the compositor chose to send
    /// slots (`picker { slots #true }`); always `None` for folders and when the setting is off.
    pub slot: Option<u8>,
}

/// Formats candidates as the `ok` payload: TAB-separated `kind[#slot]:name[:path]` items. The
/// optional `#<digit>` rides on the kind token, which never contains encoded content, so it
/// cannot clash with `:` (escaped by [`encode_field`]) in names or paths.
#[must_use]
pub fn format_candidates(items: &[Candidate]) -> String {
    items
        .iter()
        .map(|c| {
            let mut s = c.kind.as_str().to_string();
            if let Some(n) = c.slot {
                s.push_str(&format!("#{n}"));
            }
            s.push(':');
            s.push_str(&encode_field(&c.name));
            if let Some(p) = &c.path {
                s.push(':');
                s.push_str(&encode_field(&p.to_string_lossy()));
            }
            s
        })
        .collect::<Vec<_>>()
        .join("\t")
}

/// Parses a `candidates` payload back into items; `None` on any malformed item.
#[must_use]
pub fn parse_candidates(payload: &str) -> Option<Vec<Candidate>> {
    if payload.is_empty() {
        return Some(Vec::new());
    }
    payload
        .split('\t')
        .map(|item| {
            let mut parts = item.split(':');
            let head = parts.next()?;
            let (head, slot) = match head.split_once('#') {
                Some((k, n)) => {
                    let mut digits = n.chars();
                    let d = digits.next()?.to_digit(10)?;
                    if digits.next().is_some() {
                        return None;
                    }
                    (k, Some(u8::try_from(d).ok()?))
                }
                None => (head, None),
            };
            let kind = match head {
                "live" => CandidateKind::Live,
                "saved" => CandidateKind::Saved,
                "folder" => CandidateKind::Folder,
                _ => return None,
            };
            let name = decode_field(parts.next()?)?;
            let path = match parts.next() {
                Some(p) => Some(PathBuf::from(decode_field(p)?)),
                None => None,
            };
            if parts.next().is_some() {
                return None;
            }
            Some(Candidate {
                kind,
                name,
                path,
                slot,
            })
        })
        .collect()
}

/// Formats `(chord, action)` pairs as the `bindings` payload: TAB-separated
/// `encode_field(chord):encode_field(action)` items.
#[must_use]
pub fn format_bindings(entries: &[(String, String)]) -> String {
    entries
        .iter()
        .map(|(c, a)| format!("{}:{}", encode_field(c), encode_field(a)))
        .collect::<Vec<_>>()
        .join("\t")
}

/// Parses a `bindings` payload back into pairs; `None` on any malformed item.
#[must_use]
pub fn parse_bindings(payload: &str) -> Option<Vec<(String, String)>> {
    if payload.is_empty() {
        return Some(Vec::new());
    }
    payload
        .split('\t')
        .map(|item| {
            let (chord, action) = item.split_once(':')?;
            Some((decode_field(chord)?, decode_field(action)?))
        })
        .collect()
}

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
    /// path in `persist.ipc_path` (exported to children as [`SOCKET_ENV`]). Failures are logged and
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
    fn requests_round_trip_through_the_wire() {
        let reqs = [
            Request::Candidates,
            Request::Bindings,
            Request::Save,
            Request::Relaunch,
            Request::Open("a:b\tc".into()),
            Request::OpenFolder(PathBuf::from("/tmp/x:y/100%")),
            Request::Capture(PathBuf::from("/tmp/shot:1 100%.png")),
            Request::New("n\nl".into()),
            Request::Rename("r\r".into()),
            Request::Delete("d:\t100%".into()),
            Request::SetRoot(Some(PathBuf::from("/tmp/x:y/100%"))),
            Request::SetRoot(None),
            Request::Exec("run sh -c 'echo 100%: a\tb'".into()),
            Request::Exec("run-in-terminal htop -d 5: 100%".into()),
            Request::RunTui("'/usr/bin/vim' a:b 100%".into()),
            Request::RunTerm("/opt/my term/ghostty".into()),
            Request::Exec("split-right".into()),
            Request::ClosePid(4242),
        ];
        for r in reqs {
            assert_eq!(Request::parse(&r.to_line()), Ok(r.clone()));
        }
    }

    #[test]
    fn close_pid_parse_rejects_bad_input() {
        assert_eq!(Request::parse("close-pid 42"), Ok(Request::ClosePid(42)));
        for bad in [
            "close-pid",
            "close-pid ",
            "close-pid x",
            "close-pid 0",
            "close-pid +5",
            "close-pid -5",
            "close-pid 5 6",
            "close-pid 5x",
            "close-pid 4294967296",
        ] {
            assert!(Request::parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn retired_run_requests_no_longer_parse() {
        // `run`/`run-in-terminal` travel as `exec run …`/`exec run-in-terminal …` now.
        assert!(Request::parse("run firefox").is_err());
        assert!(Request::parse("run-in-terminal htop").is_err());
    }

    #[test]
    fn only_read_only_requests_do_not_act() {
        for r in [Request::Candidates, Request::Bindings, Request::Save] {
            assert!(!r.acts(), "{r:?}");
        }
        for r in [
            Request::Exec("quit".into()),
            Request::Open("a".into()),
            Request::OpenFolder(PathBuf::from("/tmp")),
            Request::New("n".into()),
            Request::Rename("r".into()),
            Request::Delete("d".into()),
            Request::SetRoot(None),
            Request::RunTui("htop".into()),
            Request::RunTerm("ghostty".into()),
            Request::Relaunch,
            Request::ClosePid(7),
        ] {
            assert!(r.acts(), "{r:?}");
        }
    }

    #[test]
    fn request_parse_rejects_bad_input() {
        assert!(Request::parse("open").is_err());
        assert!(Request::parse("open ").is_err());
        assert!(Request::parse("save now").is_err());
        assert_eq!(
            Request::parse("bindings x"),
            Err("bindings takes no argument".to_string())
        );
        assert_eq!(
            Request::parse("relaunch x"),
            Err("relaunch takes no argument".to_string())
        );
        assert!(Request::parse("bogus x").is_err());
        // A bare `root` is an error; `root -` clears.
        assert!(Request::parse("root").is_err());
        assert_eq!(Request::parse("root -"), Ok(Request::SetRoot(None)));
        assert!(Request::parse("open %ZZ").is_err());
        // The argument is the rest of the line after ONE space: spaces inside are kept.
        assert_eq!(
            Request::parse("open my  ws"),
            Ok(Request::Open("my  ws".into()))
        );
    }

    #[test]
    fn responses_round_trip() {
        for r in [
            Response::Ok(String::new()),
            Response::Ok("live:1".into()),
            Response::Err("no such: workspace\n".into()),
        ] {
            assert_eq!(Response::parse(&r.to_line()), Some(r.clone()));
        }
        assert_eq!(Response::Ok(String::new()).to_line(), "ok");
        assert_eq!(Response::parse("nope"), None);
    }

    #[test]
    fn bindings_round_trip_with_encoded_fields() {
        let entries = vec![
            ("Super+comma".to_string(), "open-settings".to_string()),
            ("Super+F2".to_string(), "run echo 50%:a\tb".to_string()),
        ];
        let payload = format_bindings(&entries);
        assert_eq!(payload.split('\t').count(), 2);
        assert_eq!(parse_bindings(&payload), Some(entries));
        assert_eq!(parse_bindings(""), Some(vec![]));
        assert_eq!(format_bindings(&[]), "");
        assert_eq!(parse_bindings("nocolon"), None);
        assert_eq!(parse_bindings("a:%ZZ"), None);
    }

    #[test]
    fn candidates_round_trip_with_encoded_fields() {
        let items = vec![
            Candidate {
                kind: CandidateKind::Live,
                name: "a:b".into(),
                path: Some(PathBuf::from("/r\toot")),
                slot: None,
            },
            Candidate {
                kind: CandidateKind::Saved,
                name: "100%".into(),
                path: None,
                slot: None,
            },
            Candidate {
                kind: CandidateKind::Folder,
                name: "x\ny".into(),
                path: Some(PathBuf::from("/p:q/x\ny")),
                slot: None,
            },
        ];
        let payload = format_candidates(&items);
        assert_eq!(payload.split('\t').count(), 3);
        assert!(payload.starts_with("live:a%3Ab:/r%09oot\tsaved:100%25\tfolder:"));
        assert_eq!(parse_candidates(&payload), Some(items));
        assert_eq!(parse_candidates(""), Some(vec![]));
        assert_eq!(parse_candidates("weird:x"), None);
    }

    #[test]
    fn candidates_round_trip_slots_and_accept_legacy_payloads() {
        let items = vec![
            Candidate {
                kind: CandidateKind::Live,
                name: "a:b#3".into(),
                path: Some(PathBuf::from("/r:s")),
                slot: Some(3),
            },
            Candidate {
                kind: CandidateKind::Saved,
                name: "z".into(),
                path: None,
                slot: Some(0),
            },
        ];
        let payload = format_candidates(&items);
        assert_eq!(payload, "live#3:a%3Ab#3:/r%3As\tsaved#0:z");
        assert_eq!(parse_candidates(&payload), Some(items));
        let legacy = parse_candidates("live:a:/r\tsaved:b").unwrap();
        assert!(legacy.iter().all(|c| c.slot.is_none()));
        assert_eq!(legacy.len(), 2);
        assert_eq!(parse_candidates("live#:a"), None);
        assert_eq!(parse_candidates("live#12:a"), None);
        assert_eq!(parse_candidates("live#x:a"), None);
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
