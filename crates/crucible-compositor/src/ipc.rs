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
//! rule encodes the argument of `crucible pick --rename <current>`.
//!
//! | Request (`Request`)        | Response                                                  |
//! |----------------------------|-----------------------------------------------------------|
//! | `candidates`               | `ok ` + TAB-separated [`Candidate`]s (`kind:name[:path]`) |
//! | `open <name>`              | `ok` / `err <reason>`                                     |
//! | `open-folder <path>`       | `ok` / `err <reason>`                                     |
//! | `new <name>`               | `ok` / `err <reason>`                                     |
//! | `rename <name>`            | `ok` / `err <reason>`                                     |
//! | `save`                     | `ok`                                                      |
//! | `run <command>`            | `ok` (spawned) / `err <reason>`                           |
//!
//! An empty candidate list is answered with a bare `ok`.
//!
//! The pure items ([`encode_field`], [`decode_field`], [`Request`], [`Response`], [`Candidate`])
//! are what the `crucible pick`/`msg` client side imports; the socket server ([`bind_socket`],
//! [`CrucibleState::start_ipc`]) is compositor-only.

use std::io::{ErrorKind, Read, Write};
use std::os::unix::fs::DirBuilderExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::time::Duration;

use smithay::reexports::calloop::{Interest, Mode, PostAction, generic::Generic};
use tracing::{debug, info, warn};

use crate::state::{Backend, CrucibleState};

/// Environment variable carrying the IPC socket path to every child CDE spawns.
pub const SOCKET_ENV: &str = "CDE_SOCKET";

/// Environment variable carrying the terminal program to every child CDE spawns.
pub const TERMINAL_ENV: &str = "CDE_TERMINAL";

/// Environment variable carrying the terminal's exec flag (`-e`) to every child CDE spawns.
pub const TERMINAL_EXEC_FLAG_ENV: &str = "CDE_TERMINAL_EXEC_FLAG";

/// Longest request line accepted before the connection is dropped.
const MAX_LINE: usize = 64 * 1024;

/// Percent-encodes `%`, `:`, TAB, LF and CR in `field`; every other character passes through.
#[must_use]
pub fn encode_field(field: &str) -> String {
    let mut out = String::with_capacity(field.len());
    for c in field.chars() {
        match c {
            '%' => out.push_str("%25"),
            ':' => out.push_str("%3A"),
            '\t' => out.push_str("%09"),
            '\n' => out.push_str("%0A"),
            '\r' => out.push_str("%0D"),
            _ => out.push(c),
        }
    }
    out
}

/// Inverse of [`encode_field`]. Returns `None` for a `%` not followed by one of the five escapes
/// (`25`, `3A`, `09`, `0A`, `0D`; hex digits case-insensitive).
#[must_use]
pub fn decode_field(field: &str) -> Option<String> {
    let mut out = String::with_capacity(field.len());
    let mut rest = field;
    while let Some(i) = rest.find('%') {
        out.push_str(&rest[..i]);
        let esc = rest.get(i + 1..i + 3)?;
        out.push(match esc.to_ascii_uppercase().as_str() {
            "25" => '%',
            "3A" => ':',
            "09" => '\t',
            "0A" => '\n',
            "0D" => '\r',
            _ => return None,
        });
        rest = &rest[i + 3..];
    }
    out.push_str(rest);
    Some(out)
}

/// One IPC request (decoded arguments).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Request {
    /// `candidates` — list live, saved and folder candidates.
    Candidates,
    /// `open <name>` — switch to a live workspace or restore a saved one.
    Open(String),
    /// `open-folder <path>` — create (or reopen) a workspace rooted at a folder.
    OpenFolder(PathBuf),
    /// `new <name>` — create and persist an empty named workspace.
    New(String),
    /// `rename <name>` — rename the active workspace (and its saved file).
    Rename(String),
    /// `save` — flush every dirty workspace now.
    Save,
    /// `run <command>` — run a shell command line (`/bin/sh -c 'exec <command>'`) into the focused pane.
    Run(String),
}

impl Request {
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
            "save" => none(arg, Request::Save),
            "open" => need(arg).map(Request::Open),
            "open-folder" => need(arg).map(|p| Request::OpenFolder(PathBuf::from(p))),
            "new" => need(arg).map(Request::New),
            "rename" => need(arg).map(Request::Rename),
            "run" => need(arg).map(Request::Run),
            other => Err(format!("unknown request {other:?}")),
        }
    }

    /// Formats the request as one wire line (no trailing newline). Paths must be UTF-8 (lossy
    /// otherwise).
    #[must_use]
    pub fn to_line(&self) -> String {
        match self {
            Request::Candidates => "candidates".into(),
            Request::Save => "save".into(),
            Request::Open(n) => format!("open {}", encode_field(n)),
            Request::OpenFolder(p) => format!("open-folder {}", encode_field(&p.to_string_lossy())),
            Request::New(n) => format!("new {}", encode_field(n)),
            Request::Rename(n) => format!("rename {}", encode_field(n)),
            Request::Run(c) => format!("run {}", encode_field(c)),
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
}

/// Formats candidates as the `ok` payload: TAB-separated `kind:name[:path]` items.
#[must_use]
pub fn format_candidates(items: &[Candidate]) -> String {
    items
        .iter()
        .map(|c| {
            let mut s = format!("{}:{}", c.kind.as_str(), encode_field(&c.name));
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
            let kind = match parts.next()? {
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
            Some(Candidate { kind, name, path })
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
        let Ok(mut io) = stream.try_clone() else {
            return;
        };
        if stream.set_nonblocking(true).is_err() {
            return;
        }
        let _ = io.set_write_timeout(Some(Duration::from_millis(200)));
        let mut buf = Vec::new();
        let res = self.handle.insert_source(
            Generic::new(stream, Interest::READ, Mode::Level),
            move |_, _, data| {
                let mut chunk = [0u8; 4096];
                loop {
                    match io.read(&mut chunk) {
                        Ok(0) => return Ok(PostAction::Remove),
                        Ok(n) => buf.extend_from_slice(&chunk[..n]),
                        Err(e) if e.kind() == ErrorKind::WouldBlock => break,
                        Err(e) if e.kind() == ErrorKind::Interrupted => {}
                        Err(_) => return Ok(PostAction::Remove),
                    }
                }
                for line in take_lines(&mut buf) {
                    let response = match Request::parse(&line) {
                        Ok(req) => {
                            debug!(?req, "IPC request");
                            data.handle_ipc(req)
                        }
                        Err(reason) => Response::Err(reason),
                    };
                    // The socket is non-blocking for reads; replies are small, so a short blocking
                    // write (bounded by the timeout above) is acceptable.
                    let _ = io.set_nonblocking(false);
                    let ok = writeln!(io, "{}", response.to_line()).is_ok();
                    let _ = io.set_nonblocking(true);
                    if !ok {
                        return Ok(PostAction::Remove);
                    }
                }
                if buf.len() > MAX_LINE {
                    warn!("IPC request line too long; dropping the connection");
                    return Ok(PostAction::Remove);
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

    #[test]
    fn codec_round_trips_every_special_character() {
        for s in [
            "a:b",
            "tab\there",
            "l\nf",
            "c\rr",
            "100%",
            "%3A literal",
            "plain ü",
            "",
        ] {
            let enc = encode_field(s);
            assert!(!enc.contains([':', '\t', '\n', '\r']), "{enc:?}");
            assert_eq!(decode_field(&enc).as_deref(), Some(s));
        }
        assert_eq!(encode_field("a%:\t\n\r"), "a%25%3A%09%0A%0D");
    }

    #[test]
    fn decode_rejects_unknown_or_truncated_escapes() {
        assert_eq!(decode_field("%41"), None);
        assert_eq!(decode_field("%2"), None);
        assert_eq!(decode_field("x%"), None);
        assert_eq!(decode_field("%3a").as_deref(), Some(":"));
    }

    #[test]
    fn requests_round_trip_through_the_wire() {
        let reqs = [
            Request::Candidates,
            Request::Save,
            Request::Open("a:b\tc".into()),
            Request::OpenFolder(PathBuf::from("/tmp/x:y/100%")),
            Request::New("n\nl".into()),
            Request::Rename("r\r".into()),
            Request::Run("sh -c 'echo 100%: a\tb'".into()),
        ];
        for r in reqs {
            assert_eq!(Request::parse(&r.to_line()), Ok(r.clone()));
        }
    }

    #[test]
    fn request_parse_rejects_bad_input() {
        assert!(Request::parse("open").is_err());
        assert!(Request::parse("open ").is_err());
        assert!(Request::parse("save now").is_err());
        assert!(Request::parse("bogus x").is_err());
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
    fn candidates_round_trip_with_encoded_fields() {
        let items = vec![
            Candidate {
                kind: CandidateKind::Live,
                name: "a:b".into(),
                path: Some(PathBuf::from("/r\toot")),
            },
            Candidate {
                kind: CandidateKind::Saved,
                name: "100%".into(),
                path: None,
            },
            Candidate {
                kind: CandidateKind::Folder,
                name: "x\ny".into(),
                path: Some(PathBuf::from("/p:q/x\ny")),
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
