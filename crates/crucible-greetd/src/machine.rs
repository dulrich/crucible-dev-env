//! The pure login state machine: events in, the request to send and the view to show out.
//! No I/O happens here.

use crate::codec::{AuthMessageType, Request, Response, Secret};

/// Where the login flow is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum State {
    /// Waiting for the user to type a username.
    Username,
    /// Waiting for greetd's reply to `create_session` or an auth answer.
    Authenticating,
    /// Waiting for the user to answer an auth prompt.
    Prompt {
        /// The prompt text.
        text: String,
        /// Whether the answer must not be echoed.
        secret: bool,
    },
    /// Waiting for greetd's reply to `start_session`.
    Starting,
    /// Waiting for greetd's reply to `cancel_session`; then back to the username.
    Cancelling,
    /// The session started; the caller should exit.
    Done,
}

/// An input to the machine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// The user submitted the field (Enter); redacted in `Debug` since it may be a password.
    Submit(Secret),
    /// The user pressed Escape.
    Escape,
    /// greetd replied.
    Response(Response),
}

/// What the UI should show.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct View {
    /// The field label ("Username" or the PAM prompt).
    pub prompt: String,
    /// Whether the input must not be echoed.
    pub secret: bool,
    /// A message line (greetd info/error, or the last failure).
    pub message: Option<String>,
}

/// The result of one transition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Output {
    /// The request to send to greetd, if any.
    pub request: Option<Request>,
    /// What to show now.
    pub view: View,
}

/// The login flow for one configured session command.
#[derive(Debug, Clone)]
pub struct LoginMachine {
    state: State,
    message: Option<String>,
    cmd: Vec<String>,
}

impl LoginMachine {
    /// Starts at the username prompt; `session_cmd` is split on whitespace for `start_session`.
    pub fn new(session_cmd: &str) -> Self {
        Self {
            state: State::Username,
            message: None,
            cmd: session_cmd.split_whitespace().map(str::to_owned).collect(),
        }
    }

    /// The current state.
    pub fn state(&self) -> &State {
        &self.state
    }

    /// The current view.
    pub fn view(&self) -> View {
        let (prompt, secret) = match &self.state {
            State::Prompt { text, secret } => (text.clone(), *secret),
            _ => ("Username".to_owned(), false),
        };
        View {
            prompt,
            secret,
            message: self.message.clone(),
        }
    }

    fn out(&self, request: Option<Request>) -> Output {
        Output {
            request,
            view: self.view(),
        }
    }

    fn cancel(&mut self, message: Option<String>) -> Output {
        self.state = State::Cancelling;
        self.message = message;
        self.out(Some(Request::CancelSession))
    }

    /// Applies one event.
    pub fn handle(&mut self, event: Event) -> Output {
        match (self.state.clone(), event) {
            (State::Username, Event::Submit(text)) => {
                self.state = State::Authenticating;
                self.message = None;
                self.out(Some(Request::CreateSession {
                    username: text.expose().to_owned(),
                }))
            }
            (State::Prompt { .. }, Event::Submit(answer)) => {
                self.state = State::Authenticating;
                self.message = None;
                self.out(Some(Request::PostAuthMessageResponse {
                    response: Some(answer),
                }))
            }
            (State::Prompt { .. }, Event::Escape) => self.cancel(None),
            (State::Authenticating, Event::Response(resp)) => self.on_auth_response(resp),
            (State::Starting, Event::Response(Response::Success)) => {
                self.state = State::Done;
                self.out(None)
            }
            (State::Starting, Event::Response(Response::Error { description, .. })) => {
                self.cancel(Some(description))
            }
            (State::Cancelling, Event::Response(_)) => {
                self.state = State::Username;
                self.out(None)
            }
            _ => self.out(None),
        }
    }

    fn on_auth_response(&mut self, resp: Response) -> Output {
        match resp {
            Response::Success => {
                self.state = State::Starting;
                self.out(Some(Request::StartSession {
                    cmd: self.cmd.clone(),
                    env: Vec::new(),
                }))
            }
            Response::Error { description, .. } => self.cancel(Some(description)),
            Response::AuthMessage {
                auth_message_type,
                auth_message,
            } => match auth_message_type {
                AuthMessageType::Visible | AuthMessageType::Secret => {
                    self.state = State::Prompt {
                        text: auth_message,
                        secret: auth_message_type == AuthMessageType::Secret,
                    };
                    self.message = None;
                    self.out(None)
                }
                AuthMessageType::Info | AuthMessageType::Error => {
                    self.message = Some(auth_message);
                    self.out(Some(Request::PostAuthMessageResponse { response: None }))
                }
            },
        }
    }
}
