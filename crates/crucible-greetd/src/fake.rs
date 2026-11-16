//! A scripted stand-in for greetd, used by tests and `crucible greeter --fake-greetd <file>`.

use std::io;
use std::os::unix::net::UnixStream;
use std::thread::{self, JoinHandle};

use serde_json::Value;

use crate::codec;

/// Parses a reply script: one JSON reply object per non-blank line (`#` lines are comments).
pub fn parse_replies(text: &str) -> Result<Vec<Value>, serde_json::Error> {
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(serde_json::from_str)
        .collect()
}

/// Plays greetd on `sock`: for each scripted reply, reads one request and sends the reply, then
/// closes the socket. Returns the requests it saw (a read failure ends the script early).
pub fn serve(mut sock: UnixStream, replies: Vec<Value>) -> JoinHandle<Vec<Value>> {
    thread::spawn(move || {
        let mut seen = Vec::new();
        for reply in replies {
            match codec::read_message::<_, Value>(&mut sock) {
                Ok(req) => seen.push(req),
                Err(_) => break,
            }
            if codec::write_message(&mut sock, &reply).is_err() {
                break;
            }
        }
        seen
    })
}

/// Connects a client stream to an in-process fake serving `replies`.
pub fn connect(replies: Vec<Value>) -> io::Result<(UnixStream, JoinHandle<Vec<Value>>)> {
    let (client, server) = UnixStream::pair()?;
    Ok((client, serve(server, replies)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_one_reply_per_line_skipping_blanks_and_comments() {
        let text = "# login\n{\"type\":\"success\"}\n\n  {\"type\":\"auth_message\",\"auth_message_type\":\"secret\",\"auth_message\":\"Password:\"}\n";
        let r = parse_replies(text).unwrap();
        assert_eq!(r.len(), 2);
        assert_eq!(r[0], json!({"type":"success"}));
        assert!(parse_replies("{not json").is_err());
    }
}
