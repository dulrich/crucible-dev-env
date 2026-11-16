//! greetd IPC client for the CDE greeter: a wire codec ([`codec`]), a pure login state machine
//! ([`machine`]) and a thin synchronous driver ([`Session`]). No compositor dependency.

pub mod codec;
pub mod machine;

use std::io::{Read, Write};

pub use codec::{AuthMessageType, CodecError, ErrorType, Request, Response, Secret};
pub use machine::{Event, LoginMachine, Output, State, View};

/// Ties a [`LoginMachine`] to a greetd stream: user events go in, the view comes out, and every
/// request the machine emits is sent and its reply fed back until the machine awaits the user.
pub struct Session<S: Read + Write> {
    stream: S,
    machine: LoginMachine,
}

impl<S: Read + Write> Session<S> {
    /// Wraps `stream` (the connection to `$GREETD_SOCK`) with a machine that starts `session_cmd`.
    pub fn new(stream: S, session_cmd: &str) -> Self {
        Self {
            stream,
            machine: LoginMachine::new(session_cmd),
        }
    }

    /// The current view without feeding an event.
    pub fn view(&self) -> View {
        self.machine.view()
    }

    /// Whether the session has started (the caller should now exit).
    pub fn is_done(&self) -> bool {
        self.machine.state() == &State::Done
    }

    /// Feeds a user event and drives the exchange with greetd until the user is needed again.
    pub fn feed(&mut self, event: Event) -> Result<View, CodecError> {
        let mut event = event;
        loop {
            let out = self.machine.handle(event);
            match out.request {
                Some(req) => {
                    codec::write_message(&mut self.stream, &req)?;
                    let resp: Response = codec::read_message(&mut self.stream)?;
                    event = Event::Response(resp);
                }
                None => return Ok(out.view),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};
    use std::os::unix::net::UnixStream;
    use std::thread::{self, JoinHandle};

    /// Plays greetd: for each scripted reply, reads one request and sends the reply; returns the
    /// requests it saw.
    fn fake_greetd(mut sock: UnixStream, replies: Vec<Value>) -> JoinHandle<Vec<Value>> {
        thread::spawn(move || {
            let mut seen = Vec::new();
            for reply in replies {
                seen.push(codec::read_message::<_, Value>(&mut sock).unwrap());
                codec::write_message(&mut sock, &reply).unwrap();
            }
            seen
        })
    }

    fn submit(s: &mut Session<UnixStream>, text: &str) -> View {
        s.feed(Event::Submit(Secret::new(text))).unwrap()
    }

    fn auth(kind: &str, text: &str) -> Value {
        json!({"type":"auth_message","auth_message_type":kind,"auth_message":text})
    }

    #[test]
    fn multi_prompt_flow_starts_session() {
        let (client, server) = UnixStream::pair().unwrap();
        let srv = fake_greetd(
            server,
            vec![
                auth("secret", "Password:"),
                auth("secret", "Token:"),
                json!({"type":"success"}),
                json!({"type":"success"}),
            ],
        );
        let mut s = Session::new(client, "cde-session --flag");
        let v = submit(&mut s, "dulrich");
        assert_eq!((v.prompt.as_str(), v.secret), ("Password:", true));
        let v = submit(&mut s, "pw");
        assert_eq!(v.prompt, "Token:");
        submit(&mut s, "123456");
        assert!(s.is_done());
        let seen = srv.join().unwrap();
        assert_eq!(
            seen[0],
            json!({"type":"create_session","username":"dulrich"})
        );
        assert_eq!(
            seen[1],
            json!({"type":"post_auth_message_response","response":"pw"})
        );
        assert_eq!(
            seen[3],
            json!({"type":"start_session","cmd":["cde-session","--flag"],"env":[]})
        );
    }

    #[test]
    fn wrong_password_cancels_and_returns_to_username() {
        let (client, server) = UnixStream::pair().unwrap();
        let srv = fake_greetd(
            server,
            vec![
                auth("secret", "Password:"),
                json!({"type":"error","error_type":"auth_error","description":"Login incorrect"}),
                json!({"type":"success"}),
            ],
        );
        let mut s = Session::new(client, "cde-session");
        submit(&mut s, "dulrich");
        let v = submit(&mut s, "wrong");
        assert_eq!(v.prompt, "Username");
        assert_eq!(v.message.as_deref(), Some("Login incorrect"));
        assert_eq!(*s.machine.state(), State::Username);
        let seen = srv.join().unwrap();
        assert_eq!(seen[2], json!({"type":"cancel_session"}));
    }

    #[test]
    fn info_message_is_shown_and_acknowledged() {
        let (client, server) = UnixStream::pair().unwrap();
        let srv = fake_greetd(
            server,
            vec![
                auth("info", "Password expires soon"),
                auth("secret", "Password:"),
            ],
        );
        let mut s = Session::new(client, "cde-session");
        let v = submit(&mut s, "dulrich");
        assert_eq!(v.prompt, "Password:");
        let seen = srv.join().unwrap();
        assert_eq!(
            seen[1],
            json!({"type":"post_auth_message_response","response":null})
        );
        // The info text was visible while the ack was in flight.
        let mut m = LoginMachine::new("x");
        m.handle(Event::Submit(Secret::new("u")));
        let out = m.handle(Event::Response(Response::AuthMessage {
            auth_message_type: AuthMessageType::Info,
            auth_message: "hello".into(),
        }));
        assert_eq!(out.view.message.as_deref(), Some("hello"));
    }

    #[test]
    fn escape_cancels_back_to_username() {
        let (client, server) = UnixStream::pair().unwrap();
        let srv = fake_greetd(
            server,
            vec![auth("secret", "Password:"), json!({"type":"success"})],
        );
        let mut s = Session::new(client, "cde-session");
        submit(&mut s, "dulrich");
        let v = s.feed(Event::Escape).unwrap();
        assert_eq!(v.prompt, "Username");
        assert_eq!(v.message, None);
        let seen = srv.join().unwrap();
        assert_eq!(seen[1], json!({"type":"cancel_session"}));
    }
}
