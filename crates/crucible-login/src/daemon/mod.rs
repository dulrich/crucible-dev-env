//! The daemon core: the greetd protocol, the greeter and user-session lifecycle and the VT
//! switches, as an event-driven state machine over the [`Backend`] seam and the [`Vt`] trait.
//! The poll loop ([`crate::server`]) turns fds, signals and timers into calls here; the tests
//! call it directly with fakes.
//!
//! Event flow: a client request ([`Daemon::handle`], tagged with the server's client id) sends
//! a worker request and returns; the worker's socket ([`Daemon::worker_fds`]) becomes readable,
//! [`Daemon::on_worker_input`] collects the reply and advances (a greeter start, an
//! authentication step, a session open/spawn); the answer for the client lands in the one reply
//! slot, which the server drains with [`Daemon::take_reply`]. One client request is in flight at
//! a time; SIGCHLD, the session and greeter pipes and the timers are served meanwhile.

use std::io;
use std::os::fd::{AsRawFd, OwnedFd, RawFd};
use std::time::{Duration, Instant};

use crucible_greetd::{ErrorType, Request, Response, Secret};

use crate::seam::{
    AuthStep, Backend, Child, Class, ExitStatus, OpenSpec, Reply, SeamError, SessionHandle,
    SpawnSpec,
};
use crate::vt::Vt;

mod greeter;
mod lifecycle;
mod requests;
mod session;

use lifecycle::reap;

/// The fd number the ready pipe's write end gets in the session process.
pub const READY_FD: RawFd = 3;
/// The env var naming [`READY_FD`] in the session process.
pub const READY_ENV: &str = "CDE_LOGIN_READY_FD";
/// How long a logout waits for the new greeter's `ready` before switching anyway, and how long
/// a `start_session` waits for the previous (handed-off) session to close. Fixed rather than
/// `--ready-timeout` so it always fires before CDE's own 10 s hard exit.
pub const LOGOUT_WAIT: Duration = Duration::from_secs(4);
/// How many prompt-less steps a greeter conversation may take before it is refused.
const GREETER_MAX_STEPS: usize = 8;

/// Daemon settings (from the command line).
#[derive(Debug, Clone)]
pub struct Config {
    /// The account the greeter runs as.
    pub greeter_user: String,
    /// The greeter command words.
    pub greeter_cmd: Vec<String>,
    /// The greeter's VT.
    pub greeter_vt: u32,
    /// The user session's VT.
    pub session_vt: u32,
    /// How long to wait for the session's `ready` before switching anyway.
    pub ready_timeout: Duration,
    /// The greeter socket path (exported as `GREETD_SOCK`).
    pub sock_path: String,
}

/// Greeter restart delays: doubling from [`Backoff::BASE`] to [`Backoff::CAP`], reset once a
/// greeter has lived [`Backoff::STABLE`] or a session ended normally.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Backoff {
    next: Duration,
}

impl Default for Backoff {
    fn default() -> Self {
        Self { next: Self::BASE }
    }
}

impl Backoff {
    /// The first delay.
    pub const BASE: Duration = Duration::from_secs(1);
    /// The longest delay.
    pub const CAP: Duration = Duration::from_secs(30);
    /// A greeter that lived this long is not crash-looping.
    pub const STABLE: Duration = Duration::from_secs(60);

    /// The delay before restarting a greeter that failed after living `lived`.
    pub fn on_failure(&mut self, lived: Duration) -> Duration {
        if lived >= Self::STABLE {
            self.next = Self::BASE;
        }
        let d = self.next;
        self.next = (self.next * 2).min(Self::CAP);
        d
    }

    /// Forgets past failures.
    pub fn reset(&mut self) {
        self.next = Self::BASE;
    }
}

/// Whether a greeter-socket peer may talk to the daemon: the greeter account or root.
pub fn peer_allowed(peer_uid: u32, greeter_uid: u32) -> bool {
    peer_uid == greeter_uid || peer_uid == 0
}

/// Whether bytes read from the ready pipe signal readiness (CDE writes `ready\n`; EOF is
/// handled separately by the caller).
pub fn ready_seen(buf: &[u8]) -> bool {
    buf.windows(5).any(|w| w == b"ready")
}

/// Whether bytes read from the session channel carry CDE's logout notice (`quit\n`).
pub fn quit_seen(buf: &[u8]) -> bool {
    buf.windows(4).any(|w| w == b"quit")
}

/// A spawned session: the child is the session (its end closes PAM), so no handle is kept.
struct Running<H: SessionHandle> {
    child: H::Child,
    started: Instant,
}

/// The opaque id the server gives each greeter-socket connection; a reply goes to the id that
/// asked.
pub type ClientId = u64;

/// Who waits for the reply to the request in flight.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Asker {
    Client(ClientId),
    /// The asker disconnected: the reply is computed as usual and then discarded.
    Gone,
}

/// The user's authentication conversation.
enum Auth<H> {
    /// A step request is outstanding.
    Waiting(H),
    /// A step request is outstanding but the conversation was cancelled: the late reply is
    /// consumed, then the handle closed.
    Cancelled(H),
    /// Waiting for the user's answer to a prompt.
    Pending(H),
    Authenticated(H, String),
}

/// The logout greeter's ready pipe while the hand-off waits for it to draw.
struct HandOff {
    ready: OwnedFd,
    deadline: Instant,
}

/// Where a greeter start is in its reply-driven sequence.
#[derive(Debug, Clone, Copy)]
enum GreeterPhase {
    /// The n-th step request is outstanding.
    Auth(usize),
    Open,
    Spawn,
}

struct GreeterStart<H> {
    handle: H,
    phase: GreeterPhase,
    /// A logout start: the VT switch belongs to the hand-off, not to the start.
    logout: bool,
    /// The logout ready pipe's write end, until the spawn request carries it.
    ready_w: Option<OwnedFd>,
    hand_off: Option<HandOff>,
}

enum Greeter<H: SessionHandle> {
    Idle,
    /// A restart is scheduled.
    Retry(Instant),
    /// A start is waiting for a worker reply.
    Starting(GreeterStart<H>),
    Up {
        run: Running<H>,
        hand_off: Option<HandOff>,
    },
}

/// A `start_session` waiting for the handed-off session to close.
struct Queued<H> {
    handle: H,
    account: String,
    cmd: Vec<String>,
    env: Vec<String>,
    deadline: Instant,
}

enum UserSession<H: SessionHandle> {
    None,
    /// The open request is outstanding.
    Opening {
        handle: H,
        account: String,
        cmd: Vec<String>,
        env: Vec<String>,
    },
    /// The spawn request is outstanding; `ready` is the ready pipe's read end.
    Spawning {
        handle: H,
        account: String,
        ready: OwnedFd,
    },
    /// Spawned, waiting for `ready`.
    Starting {
        run: Running<H>,
        ready: OwnedFd,
        deadline: Instant,
    },
    Running {
        run: Running<H>,
        /// The session channel, kept open after `ready` for the `quit` notice; `None` after EOF.
        chan: Option<OwnedFd>,
    },
    /// The session sent `quit`: the greeter was brought up for it and the session is closing,
    /// optionally with the next `start_session` queued behind it.
    Closing {
        run: Running<H>,
        chan: Option<OwnedFd>,
        queued: Option<Queued<H>>,
    },
}

/// The daemon state machine.
pub struct Daemon<B: Backend, V: Vt> {
    backend: B,
    vt: V,
    cfg: Config,
    greeter: Greeter<B::Handle>,
    backoff: Backoff,
    auth: Option<Auth<B::Handle>>,
    session: UserSession<B::Handle>,
    /// Closed unspawned workers, reaped on `SIGCHLD`.
    closing: Vec<<B::Handle as SessionHandle>::Child>,
    /// The client request in flight (at most one).
    asker: Option<Asker>,
    /// The reply waiting for the server.
    reply: Option<(ClientId, Response)>,
}

fn error(description: impl Into<String>) -> Response {
    Response::Error {
        error_type: ErrorType::Error,
        description: description.into(),
    }
}

fn seam_reply(e: &SeamError) -> Response {
    match e {
        SeamError::Auth(m) => Response::Error {
            error_type: ErrorType::AuthError,
            description: m.clone(),
        },
        SeamError::Other(m) => error(m.clone()),
    }
}

fn unexpected(r: &Reply) -> SeamError {
    SeamError::Other(format!("unexpected reply {r:?}"))
}

impl<B: Backend, V: Vt> Daemon<B, V> {
    /// A daemon with nothing running yet; call [`Daemon::start`].
    pub fn new(backend: B, vt: V, cfg: Config) -> Self {
        Self {
            backend,
            vt,
            cfg,
            greeter: Greeter::Idle,
            backoff: Backoff::default(),
            auth: None,
            session: UserSession::None,
            closing: Vec::new(),
            asker: None,
            reply: None,
        }
    }

    /// Starts the greeter on its VT and switches to it (once its start sequence completes).
    pub fn start(&mut self, now: Instant) {
        self.start_greeter(now, None);
        self.drive(now);
    }

    fn activate(&mut self, vt: u32) -> io::Result<()> {
        let r = self.vt.activate(vt);
        if let Err(e) = &r {
            tracing::error!("activating VT {vt}: {e}");
        }
        r
    }

    /// Closes an unspawned handle; its worker is reaped now if it already exited, else on
    /// `SIGCHLD`.
    fn close_handle(&mut self, handle: B::Handle, what: &str) {
        match handle.close() {
            Ok(mut w) => {
                if reap(&mut w, what).is_none() {
                    self.closing.push(w);
                }
            }
            Err(e) => tracing::warn!("closing {what}: {e}"),
        }
    }

    // ---- the reply slot ----------------------------------------------------------------------

    /// Whether a client request is in flight or its reply waits in the slot. A request that
    /// arrives meanwhile is refused (see [`Daemon::handle`]).
    pub fn busy(&self) -> bool {
        self.asker.is_some() || self.reply.is_some()
    }

    /// The reply for the request in flight, with the client that asked; the server takes it
    /// once per loop iteration (and after each request).
    pub fn take_reply(&mut self) -> Option<(ClientId, Response)> {
        self.reply.take()
    }

    /// A client disconnected: a reply still owed to it is discarded when it comes.
    pub fn on_client_gone(&mut self, id: ClientId) {
        if self.asker == Some(Asker::Client(id)) {
            self.asker = Some(Asker::Gone);
        }
        if self.reply.as_ref().is_some_and(|(c, _)| *c == id) {
            self.reply = None;
        }
    }

    fn answer(&mut self, resp: Response) {
        match self.asker.take() {
            Some(Asker::Client(id)) => self.reply = Some((id, resp)),
            Some(Asker::Gone) => tracing::debug!("reply for a disconnected client discarded"),
            None => tracing::debug!("reply with no request in flight discarded"),
        }
    }

    // ---- probes ------------------------------------------------------------------------------

    /// Whether a user session is starting or running.
    pub fn session_active(&self) -> bool {
        !matches!(self.session, UserSession::None)
    }

    /// Whether the greeter is running.
    pub fn greeter_running(&self) -> bool {
        matches!(self.greeter, Greeter::Up { .. })
    }

    /// When a scheduled greeter restart is due.
    pub fn greeter_due(&self) -> Option<Instant> {
        match self.greeter {
            Greeter::Retry(d) => Some(d),
            _ => None,
        }
    }

    /// Whether the session sent `quit` and is closing behind a new greeter.
    pub fn handed_off(&self) -> bool {
        matches!(self.session, UserSession::Closing { .. })
    }

    /// Whether a session blocks a new login conversation: starting, running without a logout
    /// hand-off, or closing with a start already queued.
    fn session_blocks(&self) -> bool {
        match &self.session {
            UserSession::None => false,
            UserSession::Closing { queued, .. } => queued.is_some(),
            _ => true,
        }
    }

    /// The session channel's read end: the ready pipe while a session starts, then (until EOF)
    /// the same pipe carrying the `quit` notice.
    pub fn ready_fd(&self) -> Option<RawFd> {
        match &self.session {
            UserSession::Starting { ready, .. } => Some(ready.as_raw_fd()),
            UserSession::Running { chan, .. } | UserSession::Closing { chan, .. } => {
                chan.as_ref().map(AsRawFd::as_raw_fd)
            }
            _ => None,
        }
    }

    /// The logout greeter's ready pipe while a hand-off waits for it.
    pub fn greeter_ready_fd(&self) -> Option<RawFd> {
        match &self.greeter {
            Greeter::Up {
                hand_off: Some(h), ..
            } => Some(h.ready.as_raw_fd()),
            _ => None,
        }
    }

    /// The worker sockets whose replies the daemon waits for; when one is readable the server
    /// calls [`Daemon::on_worker_input`].
    pub fn worker_fds(&self) -> Vec<RawFd> {
        let auth = match &self.auth {
            Some(Auth::Waiting(h) | Auth::Cancelled(h)) => Some(h.fd()),
            _ => None,
        };
        let greeter = match &self.greeter {
            Greeter::Starting(g) => Some(g.handle.fd()),
            _ => None,
        };
        let session = match &self.session {
            UserSession::Opening { handle, .. } | UserSession::Spawning { handle, .. } => {
                Some(handle.fd())
            }
            _ => None,
        };
        [auth, greeter, session].into_iter().flatten().collect()
    }

    /// The earliest timer the poll loop must wake for.
    pub fn next_deadline(&self) -> Option<Instant> {
        let session = match &self.session {
            UserSession::Starting { deadline, .. } => Some(*deadline),
            UserSession::Closing {
                queued: Some(q), ..
            } => Some(q.deadline),
            _ => None,
        };
        let greeter = match &self.greeter {
            Greeter::Retry(d) => Some(*d),
            Greeter::Up {
                hand_off: Some(h), ..
            } => Some(h.deadline),
            Greeter::Starting(GreeterStart {
                hand_off: Some(h), ..
            }) => Some(h.deadline),
            _ => None,
        };
        session.into_iter().chain(greeter).min()
    }

    // ---- worker replies ----------------------------------------------------------------------

    /// A worker socket is readable: collect every reply that is there and advance.
    pub fn on_worker_input(&mut self, now: Instant) {
        self.drive(now);
    }

    /// Collects replies (one non-blocking receive per outstanding request) until none is
    /// there. Every entry point ends here, so a reply that is already available is consumed at
    /// once.
    fn drive(&mut self, now: Instant) {
        loop {
            let a = self.drive_auth();
            let g = self.drive_greeter(now);
            let s = self.drive_session(now);
            if !(a || g || s) {
                break;
            }
        }
    }

    fn drive_auth(&mut self) -> bool {
        let (mut h, cancelled) = match self.auth.take() {
            Some(Auth::Waiting(h)) => (h, false),
            Some(Auth::Cancelled(h)) => (h, true),
            other => {
                self.auth = other;
                return false;
            }
        };
        let got = match h.take_reply() {
            Ok(None) => {
                self.auth = Some(if cancelled {
                    Auth::Cancelled(h)
                } else {
                    Auth::Waiting(h)
                });
                return false;
            }
            Ok(Some(r)) => Ok(r),
            Err(e) => Err(e),
        };
        if cancelled {
            self.close_handle(h, "authentication");
            self.answer(error("authentication cancelled"));
            return true;
        }
        match got {
            Ok(Reply::Step(AuthStep::Prompt(kind, text))) => {
                self.auth = Some(Auth::Pending(h));
                self.answer(Response::AuthMessage {
                    auth_message_type: kind,
                    auth_message: text,
                });
            }
            Ok(Reply::Step(AuthStep::Authenticated { account })) => {
                tracing::info!("authenticated as {account:?}");
                self.auth = Some(Auth::Authenticated(h, account));
                self.answer(Response::Success);
            }
            Ok(r) => {
                self.close_handle(h, "authentication");
                self.answer(seam_reply(&unexpected(&r)));
            }
            Err(e) => {
                tracing::info!("authentication failed: {e}");
                self.close_handle(h, "authentication");
                self.answer(seam_reply(&e));
            }
        }
        true
    }
}

#[cfg(test)]
mod tests;
