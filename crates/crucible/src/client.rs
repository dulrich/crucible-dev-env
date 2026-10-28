//! Thin blocking client for the compositor's IPC socket (`CDE_SOCKET`).

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crucible_compositor::ipc::{Request, Response, SOCKET_ENV};

/// Read timeout so a hung compositor cannot wedge a client.
const TIMEOUT: Duration = Duration::from_secs(5);

/// The socket path from the `CDE_SOCKET` value, or a user-facing message when unset/empty.
///
/// # Errors
/// Returns the message to print when the variable is missing or empty.
pub fn socket_from_env(value: Option<std::ffi::OsString>) -> Result<PathBuf, String> {
    match value {
        Some(v) if !v.is_empty() => Ok(PathBuf::from(v)),
        _ => Err(format!(
            "{SOCKET_ENV} is not set: run this inside a CDE session (the compositor exports it to every child)"
        )),
    }
}

/// A connected IPC client; one connection may carry several requests.
pub struct Client {
    reader: BufReader<UnixStream>,
}

impl Client {
    /// Connects to the socket at `path`.
    ///
    /// # Errors
    /// Any connect or socket-option error.
    pub fn connect(path: &Path) -> std::io::Result<Client> {
        let stream = UnixStream::connect(path)?;
        stream.set_read_timeout(Some(TIMEOUT))?;
        stream.set_write_timeout(Some(TIMEOUT))?;
        Ok(Client {
            reader: BufReader::new(stream),
        })
    }

    /// Sends one request line (no newline) and returns the response line (newline stripped).
    /// The raw escape hatch for `crucible msg`, the debug client; everything else uses
    /// [`Self::send`].
    ///
    /// # Errors
    /// I/O errors, a timeout, or the compositor closing the connection without answering.
    pub fn request_raw(&mut self, line: &str) -> std::io::Result<String> {
        self.exchange(line)?.ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "compositor closed the connection",
            )
        })
    }

    /// Sends a typed request and parses the typed response.
    ///
    /// # Errors
    /// [`ClientError`]: an I/O error or timeout, the compositor closing the connection without
    /// answering, or a reply line that is neither `ok` nor `err`.
    pub fn send(&mut self, request: &Request) -> Result<Response, ClientError> {
        let line = self
            .exchange(&request.to_line())
            .map_err(ClientError::Io)?
            .ok_or(ClientError::Closed)?;
        Response::parse(&line).ok_or(ClientError::BadResponse(line))
    }

    /// One request line out, one response line back (`None` when the connection closed).
    fn exchange(&mut self, line: &str) -> std::io::Result<Option<String>> {
        let stream = self.reader.get_mut();
        stream.write_all(line.as_bytes())?;
        stream.write_all(b"\n")?;
        stream.flush()?;
        let mut resp = String::new();
        if self.reader.read_line(&mut resp)? == 0 {
            return Ok(None);
        }
        while resp.ends_with('\n') {
            resp.pop();
        }
        Ok(Some(resp))
    }
}

/// Why [`Client::send`] produced no [`Response`].
#[derive(Debug)]
pub enum ClientError {
    /// A socket read/write error or timeout.
    Io(std::io::Error),
    /// The compositor closed the connection without answering.
    Closed,
    /// The reply line was neither `ok …` nor `err …` (the raw line).
    BadResponse(String),
}

impl std::fmt::Display for ClientError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ClientError::Io(e) => e.fmt(f),
            ClientError::Closed => f.write_str("compositor closed the connection"),
            ClientError::BadResponse(line) => write!(f, "unrecognised response {line:?}"),
        }
    }
}

impl std::error::Error for ClientError {}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;
    use std::os::unix::net::UnixListener;

    #[test]
    fn missing_or_empty_env_is_an_error() {
        assert!(socket_from_env(None).unwrap_err().contains("CDE_SOCKET"));
        assert!(socket_from_env(Some(OsString::new())).is_err());
        assert_eq!(
            socket_from_env(Some("/run/x.sock".into())).unwrap(),
            PathBuf::from("/run/x.sock")
        );
    }

    #[test]
    fn request_round_trips_several_lines_on_one_connection() {
        let dir = std::env::temp_dir().join(format!("cde-client-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("s.sock");
        let _ = std::fs::remove_file(&path);
        let listener = UnixListener::bind(&path).unwrap();
        let t = std::thread::spawn(move || {
            let (s, _) = listener.accept().unwrap();
            let mut r = BufReader::new(s.try_clone().unwrap());
            let mut w = s;
            for _ in 0..2 {
                let mut l = String::new();
                r.read_line(&mut l).unwrap();
                writeln!(w, "ok echo:{}", l.trim_end()).unwrap();
            }
        });
        let mut c = Client::connect(&path).unwrap();
        assert_eq!(c.request_raw("save").unwrap(), "ok echo:save");
        assert_eq!(c.request_raw("candidates").unwrap(), "ok echo:candidates");
        t.join().unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A one-connection server answering each line with the next of `replies`.
    fn serve(
        tag: &str,
        replies: &'static [&'static str],
    ) -> (PathBuf, std::thread::JoinHandle<()>) {
        let dir = std::env::temp_dir().join(format!("cde-client-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("s.sock");
        let _ = std::fs::remove_file(&path);
        let listener = UnixListener::bind(&path).unwrap();
        let t = std::thread::spawn(move || {
            let (s, _) = listener.accept().unwrap();
            let mut r = BufReader::new(s.try_clone().unwrap());
            let mut w = s;
            for reply in replies {
                let mut l = String::new();
                r.read_line(&mut l).unwrap();
                writeln!(w, "{reply}").unwrap();
            }
        });
        (path, t)
    }

    #[test]
    fn send_round_trips_typed_requests_on_one_connection() {
        let (path, t) = serve("typed", &["ok", "err unknown command bogus", "ok a:b"]);
        let mut c = Client::connect(&path).unwrap();
        assert_eq!(c.send(&Request::Save).unwrap(), Response::Ok(String::new()));
        assert_eq!(
            c.send(&Request::Candidates).unwrap(),
            Response::Err("unknown command bogus".into())
        );
        assert_eq!(
            c.send(&Request::Bindings).unwrap(),
            Response::Ok("a:b".into())
        );
        t.join().unwrap();
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn send_reports_an_unparseable_response() {
        let (path, t) = serve("bad", &["???"]);
        let mut c = Client::connect(&path).unwrap();
        match c.send(&Request::Save) {
            Err(ClientError::BadResponse(line)) => assert_eq!(line, "???"),
            other => panic!("expected BadResponse, got {other:?}"),
        }
        t.join().unwrap();
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn send_reports_a_closed_connection() {
        let (path, t) = serve("closed", &[]);
        let mut c = Client::connect(&path).unwrap();
        t.join().unwrap();
        assert!(matches!(
            c.send(&Request::Save),
            Err(ClientError::Closed | ClientError::Io(_))
        ));
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn client_error_messages() {
        assert_eq!(
            ClientError::Closed.to_string(),
            "compositor closed the connection"
        );
        assert_eq!(
            ClientError::BadResponse("x".into()).to_string(),
            "unrecognised response \"x\""
        );
    }

    #[test]
    fn closed_connection_is_an_error() {
        let dir = std::env::temp_dir().join(format!("cde-client-eof-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("s.sock");
        let _ = std::fs::remove_file(&path);
        let listener = UnixListener::bind(&path).unwrap();
        let t = std::thread::spawn(move || {
            let _ = listener.accept().unwrap();
        });
        let mut c = Client::connect(&path).unwrap();
        t.join().unwrap();
        assert!(c.request_raw("save").is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
