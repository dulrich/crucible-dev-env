//! greetd IPC wire format (greetd-ipc(7)): a native-endian `u32` byte length followed by that
//! many bytes of UTF-8 JSON.

use std::fmt;
use std::io::{self, Read, Write};

use serde::{Deserialize, Serialize};

/// Largest message body accepted from the peer.
pub const MAX_MESSAGE_LEN: usize = 64 * 1024;

/// A codec failure; malformed input is always an error, never a panic.
#[derive(Debug, thiserror::Error)]
pub enum CodecError {
    /// The stream failed or ended early.
    #[error("greetd io: {0}")]
    Io(#[from] io::Error),
    /// The announced length exceeds [`MAX_MESSAGE_LEN`].
    #[error("greetd message of {0} bytes exceeds the {MAX_MESSAGE_LEN} byte cap")]
    TooLarge(usize),
    /// The body is not a known JSON message.
    #[error("greetd message is malformed: {0}")]
    Malformed(#[from] serde_json::Error),
}

/// A string that never appears in `Debug` output (passwords and other secret answers), and
/// whose bytes are overwritten with zeros when it is dropped (each clone wipes its own copy).
/// Only the final buffer is wiped: copies left behind by earlier reallocations, by `serde_json`
/// while decoding, or in socket buffers are not.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Secret(String);

impl Secret {
    /// Wraps a secret value.
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// The underlying value.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl Drop for Secret {
    fn drop(&mut self) {
        let mut bytes = std::mem::take(&mut self.0).into_bytes();
        wipe(&mut bytes);
    }
}

/// Overwrites `buf` with zeros through volatile writes, so the optimiser cannot drop the wipe
/// of a buffer that is about to be freed.
#[allow(unsafe_code)]
pub fn wipe(buf: &mut [u8]) {
    for b in buf.iter_mut() {
        // SAFETY: `b` is a valid, aligned, exclusive reference to one initialised byte.
        unsafe { std::ptr::write_volatile(b, 0) };
    }
    std::sync::atomic::compiler_fence(std::sync::atomic::Ordering::SeqCst);
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("<redacted>")
    }
}

/// A request to greetd.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Request {
    /// Begin authenticating `username`.
    CreateSession {
        /// The account to log in.
        username: String,
    },
    /// Answer an auth message (`None` acknowledges info/error messages).
    PostAuthMessageResponse {
        /// The answer; redacted in `Debug`.
        response: Option<Secret>,
    },
    /// Start the authenticated session.
    StartSession {
        /// The command and its arguments.
        cmd: Vec<String>,
        /// Extra `KEY=VALUE` environment entries.
        env: Vec<String>,
    },
    /// Abort the session being created.
    CancelSession,
}

/// Why greetd refused a request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorType {
    /// Authentication failed (wrong credentials).
    AuthError,
    /// Any other failure.
    Error,
}

/// The kind of an auth message.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthMessageType {
    /// A prompt whose answer may be echoed.
    Visible,
    /// A prompt whose answer must not be echoed.
    Secret,
    /// An informational message.
    Info,
    /// An error message.
    Error,
}

/// A reply from greetd.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Response {
    /// The request succeeded.
    Success,
    /// The request failed.
    Error {
        /// The failure class.
        error_type: ErrorType,
        /// Human-readable detail.
        description: String,
    },
    /// PAM wants something from the user.
    AuthMessage {
        /// The message kind.
        auth_message_type: AuthMessageType,
        /// The prompt or message text.
        auth_message: String,
    },
}

/// Writes one length-prefixed JSON message.
pub fn write_message<W: Write, T: Serialize>(w: &mut W, msg: &T) -> Result<(), CodecError> {
    let body = serde_json::to_vec(msg)?;
    let len = u32::try_from(body.len()).map_err(|_| CodecError::TooLarge(body.len()))?;
    w.write_all(&len.to_ne_bytes())?;
    w.write_all(&body)?;
    w.flush()?;
    Ok(())
}

/// Reads one length-prefixed JSON message, rejecting lengths above [`MAX_MESSAGE_LEN`].
pub fn read_message<R: Read, T: for<'de> Deserialize<'de>>(r: &mut R) -> Result<T, CodecError> {
    let mut head = [0u8; 4];
    r.read_exact(&mut head)?;
    let len = u32::from_ne_bytes(head) as usize;
    if len > MAX_MESSAGE_LEN {
        return Err(CodecError::TooLarge(len));
    }
    let mut body = vec![0u8; len];
    r.read_exact(&mut body)?;
    Ok(serde_json::from_slice(&body)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixStream;

    #[test]
    fn wipe_zeroes_the_buffer_and_secret_stays_transparent() {
        let mut b = b"hunter 2".to_vec();
        wipe(&mut b);
        assert_eq!(b, [0u8; 8]);
        let s = Secret::new("pw");
        let c = s.clone();
        drop(s);
        assert_eq!(c.expose(), "pw", "a clone owns its own copy");
        assert_eq!(serde_json::to_string(&Some(c)).unwrap(), r#""pw""#);
    }

    #[test]
    fn rejects_oversize_length() {
        let (mut a, mut b) = UnixStream::pair().unwrap();
        a.write_all(&(MAX_MESSAGE_LEN as u32 + 1).to_ne_bytes())
            .unwrap();
        let err = read_message::<_, Response>(&mut b).unwrap_err();
        assert!(matches!(err, CodecError::TooLarge(_)));
    }

    #[test]
    fn rejects_garbage_and_unknown_json() {
        for body in [&b"not json"[..], &br#"{"type":"bogus"}"#[..]] {
            let (mut a, mut b) = UnixStream::pair().unwrap();
            a.write_all(&(body.len() as u32).to_ne_bytes()).unwrap();
            a.write_all(body).unwrap();
            let err = read_message::<_, Response>(&mut b).unwrap_err();
            assert!(matches!(err, CodecError::Malformed(_)));
        }
    }

    #[test]
    fn wire_bytes_round_trip_unchanged() {
        let reqs = [
            r#"{"type":"create_session","username":"alice"}"#,
            r#"{"type":"post_auth_message_response","response":"pw"}"#,
            r#"{"type":"post_auth_message_response","response":null}"#,
            r#"{"type":"start_session","cmd":["cde-session"],"env":["A=1"]}"#,
            r#"{"type":"cancel_session"}"#,
        ];
        for json in reqs {
            let req: Request = serde_json::from_str(json).unwrap();
            assert_eq!(serde_json::to_string(&req).unwrap(), json);
        }
        let resps = [
            r#"{"type":"success"}"#,
            r#"{"type":"error","error_type":"auth_error","description":"bad"}"#,
            r#"{"type":"error","error_type":"error","description":"x"}"#,
            r#"{"type":"auth_message","auth_message_type":"secret","auth_message":"Password: "}"#,
            r#"{"type":"auth_message","auth_message_type":"visible","auth_message":"v"}"#,
            r#"{"type":"auth_message","auth_message_type":"info","auth_message":"i"}"#,
            r#"{"type":"auth_message","auth_message_type":"error","auth_message":"e"}"#,
        ];
        for json in resps {
            let resp: Response = serde_json::from_str(json).unwrap();
            assert_eq!(serde_json::to_string(&resp).unwrap(), json);
        }
    }

    #[test]
    fn secret_is_redacted_in_debug() {
        let req = Request::PostAuthMessageResponse {
            response: Some(Secret::new("hunter2")),
        };
        assert!(!format!("{req:?}").contains("hunter2"));
    }
}
