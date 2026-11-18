//! The authenticator/session seam. The daemon core drives it; [`crate::worker::PamBackend`]
//! implements it with PAM (one PAM handle per [`SessionHandle`], living in a root process for
//! the session's lifetime), and the tests drive a fake.
//!
//! Lifecycle of one handle: [`Backend::begin`] (fixes the class) → steps until
//! [`AuthStep::Authenticated`] (fixes the account) → open (fixes the VT) → spawn, after which
//! [`SessionHandle::into_child`] turns the handle into the [`Child`] that is the session; the
//! backend closes PAM when it ends. [`SessionHandle::close`] is for a handle that never spawned
//! (failed or abandoned authentication, a failed open or spawn, a queued start dropped); it
//! consumes the handle, so the daemon closes each unspawned handle exactly once.
//!
//! Every step is an event, never a blocking round trip (review A6): `send_*` writes the request
//! and returns at once; the handle's [`SessionHandle::fd`] becomes readable when the reply is
//! there, and [`SessionHandle::take_reply`] collects it with one non-blocking receive. At most
//! one request is outstanding per handle. The daemon polls the fds of the handles it is waiting
//! on (request → worker fd readable → reply → the daemon's reply slot → the client).

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
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
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
    /// The VT number the session lives on: the one value behind `PAM_TTY=tty<vt>`, the user
    /// process's `/dev/tty<vt>` stdio and `XDG_VTNR`.
    pub vt: u32,
    /// Extra `KEY=VALUE` entries for the PAM environment before `pam_open_session`.
    pub env: Vec<String>,
}

/// What [`SessionHandle::spawn`] runs.
#[derive(Debug)]
pub struct SpawnSpec {
    /// The command words; the backend runs them as `<shell> -l -c 'exec "$@"'` (the account's
    /// shell when `/etc/shells` lists it, else `/bin/sh`), so no word is re-split.
    pub cmd: Vec<String>,
    /// Extra `KEY=VALUE` entries added to the from-scratch environment.
    pub env: Vec<String>,
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

/// Where [`Child::signal`] aims a signal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SignalTarget {
    /// The session's own process (the PAM worker, which forwards it to the user process).
    Session,
    /// The user process through its pidfd, never a bare pid (SIGKILL cannot be forwarded).
    UserPidfd,
}

/// The target every [`Child`] implementation uses for `sig`.
pub fn signal_target(sig: i32) -> SignalTarget {
    if sig == libc::SIGKILL {
        SignalTarget::UserPidfd
    } else {
        SignalTarget::Session
    }
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

/// A reply collected by [`SessionHandle::take_reply`]; which one depends on the request sent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reply {
    /// The answer to a step request.
    Step(AuthStep),
    /// The open request succeeded.
    Opened,
    /// The spawn request succeeded: call [`SessionHandle::into_child`].
    Spawned,
}

/// One PAM-like handle: authentication, then the open session, then close. Requests return at
/// once; the reply arrives on [`SessionHandle::fd`].
pub trait SessionHandle {
    /// The spawned process type.
    type Child: Child;
    /// The poll source: readable (or hung up) when a reply can be collected.
    fn fd(&self) -> RawFd;
    /// Requests the next conversation step. The first call passes `None`; later calls pass the
    /// answer to the previous prompt (`None` acknowledges `Info`/`Error`). An error reply ends
    /// the conversation; the daemon then closes the handle.
    fn send_step(&mut self, answer: Option<Secret>) -> Result<(), SeamError>;
    /// Requests the open (`pam_setcred` + `pam_open_session`) after authentication.
    fn send_open(&mut self, spec: &OpenSpec) -> Result<(), SeamError>;
    /// Requests the spawn of the command as the authenticated account inside the open session,
    /// on the VT given to the open (it becomes the child's controlling tty and stdio). The spec
    /// (and our copy of its extra fd) is dropped once the request is sent.
    fn send_spawn(&mut self, spec: SpawnSpec) -> Result<(), SeamError>;
    /// One non-blocking receive: `Ok(None)` while the reply is not there yet; an error reply,
    /// an undecodable one or the worker's EOF is `Err`. After an `Err` or a reply to a failed
    /// spawn the handle is unspawned and the daemon closes it.
    fn take_reply(&mut self) -> Result<Option<Reply>, SeamError>;
    /// The session, after [`Reply::Spawned`]: its end closes PAM.
    fn into_child(self) -> Self::Child;
    /// Ends a handle that never spawned: sends the close (`pam_close_session` if opened,
    /// `pam_setcred(DELETE_CRED)`, `pam_end`) without waiting, and returns the worker to reap
    /// without blocking (on `SIGCHLD`).
    fn close(self) -> Result<Self::Child, SeamError>;
}

/// Creates handles.
pub trait Backend {
    /// The handle type.
    type Handle: SessionHandle;
    /// Starts a conversation for `user` with the service of `class`.
    fn begin(&mut self, class: Class, user: &str) -> Result<Self::Handle, SeamError>;
}
