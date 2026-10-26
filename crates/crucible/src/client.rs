//! Thin blocking client for the compositor's IPC socket (`CDE_SOCKET`).

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crucible_compositor::ipc::SOCKET_ENV;

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
    ///
    /// # Errors
    /// I/O errors, a timeout, or the compositor closing the connection without answering.
    pub fn request(&mut self, line: &str) -> std::io::Result<String> {
        let stream = self.reader.get_mut();
        stream.write_all(line.as_bytes())?;
        stream.write_all(b"\n")?;
        stream.flush()?;
        let mut resp = String::new();
        if self.reader.read_line(&mut resp)? == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "compositor closed the connection",
            ));
        }
        while resp.ends_with('\n') {
            resp.pop();
        }
        Ok(resp)
    }
}

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
        assert_eq!(c.request("save").unwrap(), "ok echo:save");
        assert_eq!(c.request("candidates").unwrap(), "ok echo:candidates");
        t.join().unwrap();
        let _ = std::fs::remove_dir_all(&dir);
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
        assert!(c.request("save").is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
