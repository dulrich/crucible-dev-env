//! The authenticator/session seam. The daemon core drives it; WP-3 implements it with PAM (one
//! PAM handle per [`SessionHandle`], living in a root process for the session's lifetime), the
//! tests drive a fake, and [`StubBackend`] stands in until WP-3 lands.
//!
//! Lifecycle of one handle: [`Backend::begin`] → [`SessionHandle::auth_step`] until
//! [`AuthStep::Authenticated`] → [`SessionHandle::open`] → [`SessionHandle::spawn`] →
//! [`SessionHandle::close`]. `close` consumes the handle, so a handle is closed at most once; the
//! daemon guarantees it is closed exactly once (also when authentication is abandoned).

use std::os::fd::{OwnedFd, RawFd};

use crucible_greetd::{AuthMessageType, Secret};

/// A seam failure.
#[derive(Debug, thiserror::Error)]
pub enum SeamError {
    /// Authentication failed (wrong credentials, account refused); shown to the user.
    #[error("{0}")]
    Auth(String),
    /// Any other failure.
    #[error("{0}")]
    Other(String),
}

/// Which PAM service (and session class) a handle is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Class {
    /// A user session (PAM service `cde-login`, `XDG_SESSION_CLASS=user`).
    User,
    /// The greeter (PAM service `cde-login-greeter`, `XDG_SESSION_CLASS=greeter`, no prompts
    /// expected).
    Greeter,
}

impl Class {
    /// The PAM service name.
    pub fn service(self) -> &'static str {
        match self {
            Class::User => "cde-login",
            Class::Greeter => "cde-login-greeter",
        }
    }

    /// The `XDG_SESSION_CLASS` value.
    pub fn xdg_class(self) -> &'static str {
        match self {
            Class::User => "user",
            Class::Greeter => "greeter",
        }
    }
}

/// One step of an authentication conversation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthStep {
    /// The backend needs an answer (`Visible`/`Secret`) or an acknowledgement (`Info`/`Error`,
    /// answered with `None`).
    Prompt(AuthMessageType, String),
    /// Authentication and the account check (`pam_acct_mgmt`) passed. `account` is the account
    /// PAM resolved (`PAM_USER` after auth), which may differ from the submitted name; every later
    /// step runs as this account (audit item E1).
    Authenticated {
        /// The resolved account name.
        account: String,
    },
}

/// What [`SessionHandle::open`] registers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenSpec {
    /// The resolved account (from [`AuthStep::Authenticated`]).
    pub account: String,
    /// The VT number the session lives on (`XDG_VTNR`, `PAM_TTY=/dev/tty<vt>`).
    pub vt: u32,
    /// The session class.
    pub class: Class,
    /// Extra `KEY=VALUE` entries for the PAM environment before `pam_open_session`.
    pub env: Vec<String>,
}

/// What [`SessionHandle::spawn`] runs.
#[derive(Debug)]
pub struct SpawnSpec {
    /// The resolved account the command runs as (never root unless the account is root).
    pub account: String,
    /// The command words; the backend runs them as `<shell> -l -c 'exec "$@"'` (the account's
    /// shell when `/etc/shells` lists it, else `/bin/sh`), so no word is re-split.
    pub cmd: Vec<String>,
    /// Extra `KEY=VALUE` entries added to the from-scratch environment.
    pub env: Vec<String>,
    /// The VT that becomes the child's controlling tty and stdio (nothing is written to it).
    pub vt: u32,
    /// An extra fd that crosses the exec: the ready pipe's write end, to be placed at the given
    /// fd number. Every other daemon fd stays close-on-exec.
    pub extra_fd: Option<(OwnedFd, RawFd)>,
}

/// How a child ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExitStatus {
    /// Exited with this code.
    Code(i32),
    /// Killed by this signal.
    Signal(i32),
}

/// A spawned session process.
pub trait Child {
    /// Its pid (for logs).
    fn pid(&self) -> i32;
    /// Reaps without blocking: `Some` once it has exited.
    fn try_wait(&mut self) -> Result<Option<ExitStatus>, SeamError>;
    /// Sends a signal (`libc::SIGTERM`, `libc::SIGKILL`); a no-op once it has exited.
    fn signal(&mut self, sig: i32) -> Result<(), SeamError>;
}

/// One PAM-like handle: authentication, then the open session, then close.
pub trait SessionHandle {
    /// The spawned process type.
    type Child: Child;
    /// Advances the conversation. The first call passes `None`; later calls pass the answer to
    /// the previous prompt (`None` acknowledges `Info`/`Error`). Errors end the conversation;
    /// the daemon then closes the handle.
    fn auth_step(&mut self, answer: Option<Secret>) -> Result<AuthStep, SeamError>;
    /// Opens the session (`pam_setcred` + `pam_open_session`) after authentication.
    fn open(&mut self, spec: &OpenSpec) -> Result<(), SeamError>;
    /// Spawns the command as the account inside the open session.
    fn spawn(&mut self, spec: SpawnSpec) -> Result<Self::Child, SeamError>;
    /// Closes the session if it was opened, then ends the handle (`pam_close_session`,
    /// `pam_end`); also how an abandoned authentication is cancelled.
    fn close(self) -> Result<(), SeamError>;
}

/// Creates handles.
pub trait Backend {
    /// The handle type.
    type Handle: SessionHandle;
    /// Starts a conversation for `user` with the service of `class`.
    fn begin(&mut self, class: Class, user: &str) -> Result<Self::Handle, SeamError>;
}

/// The placeholder backend until WP-3: every `begin` fails.
#[derive(Debug, Default)]
pub struct StubBackend;

/// Uninhabited: the stub never produces a handle or child.
#[derive(Debug)]
pub enum Never {}

impl Backend for StubBackend {
    type Handle = Never;
    fn begin(&mut self, _class: Class, _user: &str) -> Result<Never, SeamError> {
        Err(SeamError::Other(
            "PAM backend not implemented (WP-3)".to_owned(),
        ))
    }
}

impl SessionHandle for Never {
    type Child = Never;
    fn auth_step(&mut self, _: Option<Secret>) -> Result<AuthStep, SeamError> {
        match *self {}
    }
    fn open(&mut self, _: &OpenSpec) -> Result<(), SeamError> {
        match *self {}
    }
    fn spawn(&mut self, _: SpawnSpec) -> Result<Never, SeamError> {
        match *self {}
    }
    fn close(self) -> Result<(), SeamError> {
        match self {}
    }
}

impl Child for Never {
    fn pid(&self) -> i32 {
        match *self {}
    }
    fn try_wait(&mut self) -> Result<Option<ExitStatus>, SeamError> {
        match *self {}
    }
    fn signal(&mut self, _: i32) -> Result<(), SeamError> {
        match *self {}
    }
}
