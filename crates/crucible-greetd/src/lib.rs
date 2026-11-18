//! greetd IPC client for the CDE greeter: a wire codec ([`codec`]), a pure login state machine
//! ([`machine`]) and a thin synchronous driver ([`Session`]). No compositor dependency.

pub mod codec;
pub mod fake;
pub mod machine;

use std::io::{Read, Write};

pub use codec::{AuthMessageType, CodecError, ErrorType, Request, Response, Secret};
pub use machine::{Event, LoginMachine, Output, State, View};

/// Called just before a `start_session` request is written; `false` aborts the exchange.
pub type BeforeStart = Box<dyn FnMut() -> bool + Send>;

/// Ties a [`LoginMachine`] to a greetd stream: user events go in, the view comes out, and every
/// request the machine emits is sent and its reply fed back until the machine awaits the user.
pub struct Session<S: Read + Write> {
    stream: S,
    machine: LoginMachine,
    before_start: Option<BeforeStart>,
}

impl<S: Read + Write> Session<S> {
    /// Wraps `stream` (the connection to `$GREETD_SOCK`) with a machine that starts `session_cmd`.
    pub fn new(stream: S, session_cmd: &str) -> Self {
        Self {
            stream,
            machine: LoginMachine::new(session_cmd),
            before_start: None,
        }
    }

    /// Sets the hook [`feed`](Self::feed) calls once, just before it writes `start_session` (never
    /// for another request). It may block; returning `false` makes `feed` fail with an I/O error
    /// without writing the request (a transport failure to the caller).
    pub fn set_before_start(&mut self, hook: BeforeStart) {
        self.before_start = Some(hook);
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
                    if matches!(req, Request::StartSession { .. })
                        && let Some(hook) = self.before_start.as_mut()
                        && !hook()
                    {
                        return Err(CodecError::Io(std::io::Error::other(
                            "start_session aborted before it was written",
                        )));
                    }
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
    use std::thread::JoinHandle;

    fn fake_greetd(sock: UnixStream, replies: Vec<Value>) -> JoinHandle<Vec<Value>> {
        crate::fake::serve(sock, replies)
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
        // The info line survives the next prompt and is only cleared by the next Submit.
        assert_eq!(v.message.as_deref(), Some("Password expires soon"));
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

    #[test]
    fn before_start_runs_once_just_before_start_session_and_is_waited_for() {
        use std::sync::{Arc, Mutex};
        let (client, server) = UnixStream::pair().unwrap();
        let srv = fake_greetd(
            server,
            vec![
                auth("secret", "Password:"),
                json!({"type":"success"}),
                json!({"type":"success"}),
            ],
        );
        let mut s = Session::new(client, "cde-session");
        let calls = Arc::new(Mutex::new(Vec::new()));
        let log = Arc::clone(&calls);
        s.set_before_start(Box::new(move || {
            // Blocks like the worker's ack wait; the request must not be written meanwhile.
            std::thread::sleep(std::time::Duration::from_millis(50));
            log.lock().unwrap().push(std::time::Instant::now());
            true
        }));
        submit(&mut s, "dulrich");
        assert!(calls.lock().unwrap().is_empty(), "not for create_session");
        submit(&mut s, "pw");
        assert!(s.is_done());
        assert_eq!(calls.lock().unwrap().len(), 1, "once, for start_session");
        let seen = srv.join().unwrap();
        assert_eq!(seen.len(), 3);
        assert_eq!(seen[2]["type"], "start_session");
    }

    #[test]
    fn before_start_abort_fails_without_writing_start_session() {
        let (client, server) = UnixStream::pair().unwrap();
        let srv = fake_greetd(
            server,
            vec![
                auth("secret", "Password:"),
                json!({"type":"success"}),
                json!({"type":"success"}),
            ],
        );
        let mut s = Session::new(client, "cde-session");
        // A dropped ack: the hook reports abort.
        let (tx, rx) = std::sync::mpsc::channel::<()>();
        drop(tx);
        s.set_before_start(Box::new(move || rx.recv().is_ok()));
        submit(&mut s, "dulrich");
        assert!(s.feed(Event::Submit(Secret::new("pw"))).is_err());
        drop(s);
        let seen = srv.join().unwrap();
        assert_eq!(seen.len(), 2, "start_session was never written: {seen:?}");
    }
}
