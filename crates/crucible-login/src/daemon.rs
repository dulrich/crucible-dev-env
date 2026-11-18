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

    // ---- greeter -----------------------------------------------------------------------------

    /// Starts a greeter: `begin` and the first step request; the replies drive the rest
    /// ([`Daemon::drive_greeter`]). A logout start passes the ready pipe (read end, write end):
    /// the write end goes to [`READY_FD`] with [`READY_ENV`] set. Greeters started otherwise
    /// get none.
    fn start_greeter(&mut self, now: Instant, hand_off: Option<(OwnedFd, OwnedFd)>) {
        let logout = hand_off.is_some();
        let (hand_off, ready_w) = match hand_off {
            Some((r, w)) => (
                Some(HandOff {
                    ready: r,
                    deadline: now + LOGOUT_WAIT,
                }),
                Some(w),
            ),
            None => (None, None),
        };
        let mut handle = match self.backend.begin(Class::Greeter, &self.cfg.greeter_user) {
            Ok(h) => h,
            Err(e) => return self.greeter_failed(now, &e, logout),
        };
        if let Err(e) = handle.send_step(None) {
            self.close_handle(handle, "greeter handle");
            return self.greeter_failed(now, &e, logout);
        }
        self.greeter = Greeter::Starting(GreeterStart {
            handle,
            phase: GreeterPhase::Auth(1),
            logout,
            ready_w,
            hand_off,
        });
    }

    fn greeter_failed(&mut self, now: Instant, e: &SeamError, logout: bool) {
        let delay = self.backoff.on_failure(Duration::ZERO);
        if logout {
            tracing::error!("logout: greeter start failed: {e}; retrying in {delay:?}");
        } else {
            tracing::error!("greeter start failed: {e}; retrying in {delay:?}");
        }
        self.greeter = Greeter::Retry(now + delay);
        if logout {
            self.switch_to_greeter();
        }
    }

    fn drive_greeter(&mut self, now: Instant) -> bool {
        let mut g = match std::mem::replace(&mut self.greeter, Greeter::Idle) {
            Greeter::Starting(g) => g,
            other => {
                self.greeter = other;
                return false;
            }
        };
        let r = match g.handle.take_reply() {
            Ok(None) => {
                self.greeter = Greeter::Starting(g);
                return false;
            }
            Ok(Some(r)) => r,
            Err(e) => {
                self.close_handle(g.handle, "greeter handle");
                self.greeter_failed(now, &e, g.logout);
                return true;
            }
        };
        let next = match (g.phase, r) {
            (GreeterPhase::Auth(_), Reply::Step(AuthStep::Authenticated { account })) => {
                tracing::debug!("greeter authenticated as {account:?}");
                let env = vec![format!("GREETD_SOCK={}", self.cfg.sock_path)];
                g.handle
                    .send_open(&OpenSpec {
                        vt: self.cfg.greeter_vt,
                        env,
                    })
                    .map(|()| GreeterPhase::Open)
            }
            (GreeterPhase::Auth(n), Reply::Step(AuthStep::Prompt(..))) if n < GREETER_MAX_STEPS => {
                g.handle.send_step(None).map(|()| GreeterPhase::Auth(n + 1))
            }
            (GreeterPhase::Auth(_), Reply::Step(AuthStep::Prompt(..))) => Err(SeamError::Other(
                "greeter conversation did not finish".into(),
            )),
            (GreeterPhase::Open, Reply::Opened) => {
                let mut env = vec![format!("GREETD_SOCK={}", self.cfg.sock_path)];
                if g.ready_w.is_some() {
                    env.push(format!("{READY_ENV}={READY_FD}"));
                }
                g.handle
                    .send_spawn(SpawnSpec {
                        cmd: self.cfg.greeter_cmd.clone(),
                        env,
                        extra_fd: g.ready_w.take().map(|w| (w, READY_FD)),
                    })
                    .map(|()| GreeterPhase::Spawn)
            }
            (GreeterPhase::Spawn, Reply::Spawned) => {
                let run: Running<B::Handle> = Running {
                    child: g.handle.into_child(),
                    started: now,
                };
                if g.logout {
                    tracing::info!("logout: greeter started (pid {})", run.child.pid());
                } else {
                    tracing::info!("greeter started (pid {})", run.child.pid());
                }
                self.greeter = Greeter::Up {
                    run,
                    hand_off: g.hand_off,
                };
                if !g.logout {
                    let _ = self.activate(self.cfg.greeter_vt);
                }
                return true;
            }
            (_, r) => Err(unexpected(&r)),
        };
        match next {
            Ok(phase) => {
                g.phase = phase;
                self.greeter = Greeter::Starting(g);
            }
            Err(e) => {
                self.close_handle(g.handle, "greeter handle");
                self.greeter_failed(now, &e, g.logout);
            }
        }
        true
    }

    /// Bytes (or EOF) from the logout greeter's ready pipe: `ready` or EOF completes the
    /// hand-off.
    pub fn on_greeter_input(&mut self, buf: &[u8], eof: bool) {
        if eof || ready_seen(buf) {
            self.on_greeter_ready();
        }
    }

    /// The logout greeter signalled `ready` (or closed its pipe).
    pub fn on_greeter_ready(&mut self) {
        if let Greeter::Up { hand_off, .. } = &mut self.greeter
            && hand_off.take().is_some()
        {
            tracing::info!("logout: greeter ready");
            self.switch_to_greeter();
        }
    }

    fn switch_to_greeter(&mut self) {
        let vt = self.cfg.greeter_vt;
        if self.activate(vt).is_ok() {
            tracing::info!("logout: switched to VT {vt}");
        }
    }

    // ---- session channel ---------------------------------------------------------------------

    /// Bytes (or EOF) from the session channel. Before ready, `ready` or EOF means ready (as
    /// does a `quit` that arrives first); after ready, `quit` starts the logout hand-off and EOF
    /// retires the channel. Becoming ready puts the deferred `start_session` reply in the slot.
    pub fn on_session_input(&mut self, buf: &[u8], eof: bool, now: Instant) {
        let quit = quit_seen(buf);
        if matches!(self.session, UserSession::Starting { .. }) && (eof || quit || ready_seen(buf))
        {
            tracing::info!(
                "session ready ({})",
                if eof && !ready_seen(buf) {
                    "pipe closed"
                } else {
                    "ready"
                }
            );
            self.on_ready();
        }
        if quit {
            self.on_quit(now);
        }
        if eof
            && let UserSession::Running { chan, .. } | UserSession::Closing { chan, .. } =
                &mut self.session
        {
            *chan = None;
        }
        self.drive(now);
    }

    /// The session's `quit` notice: bring the greeter up in the background with its own ready
    /// pipe; [`Daemon::on_greeter_ready`], the [`LOGOUT_WAIT`] fallback or the greeter's exit
    /// then switch to it.
    pub fn on_quit(&mut self, now: Instant) {
        let (run, chan) = match std::mem::replace(&mut self.session, UserSession::None) {
            UserSession::Running { run, chan } => (run, chan),
            other => {
                self.session = other;
                return;
            }
        };
        self.session = UserSession::Closing {
            run,
            chan,
            queued: None,
        };
        tracing::info!("logout: quit notice");
        match self.greeter {
            // Up (already drawn) or starting without a ready pipe: switch at once.
            Greeter::Up { .. } | Greeter::Starting(_) => self.switch_to_greeter(),
            Greeter::Idle | Greeter::Retry(_) => match crate::sys::pipe_cloexec(0) {
                Ok(pipe) => self.start_greeter(now, Some(pipe)),
                Err(e) => self.greeter_failed(now, &SeamError::Other(format!("pipe: {e}")), true),
            },
        }
        self.drive(now);
    }

    // ---- greeter requests --------------------------------------------------------------------

    /// Cancels the user's conversation: an idle handle is closed now; one with a step request
    /// outstanding is closed when its late reply has been consumed.
    fn cancel_auth(&mut self) {
        match self.auth.take() {
            Some(Auth::Waiting(h) | Auth::Cancelled(h)) => self.auth = Some(Auth::Cancelled(h)),
            Some(Auth::Pending(h) | Auth::Authenticated(h, _)) => {
                self.close_handle(h, "authentication");
            }
            None => {}
        }
    }

    /// Drops a queued start (its client gets an error).
    fn drop_queued(&mut self, why: &str) {
        if let UserSession::Closing { queued, .. } = &mut self.session
            && let Some(q) = queued.take()
        {
            self.close_handle(q.handle, "queued session");
            self.answer(error(why));
        }
    }

    /// Serves one greeter request from `client`. Every reply, immediate or after worker
    /// replies, a ready wait or a queued start, goes to the reply slot for `client`
    /// ([`Daemon::take_reply`]). While a request is in flight ([`Daemon::busy`]) another one is
    /// refused at once with the returned error, which the server writes to that client.
    pub fn handle(&mut self, client: ClientId, req: Request, now: Instant) -> Result<(), Response> {
        if self.busy() {
            return Err(error("another request is in progress"));
        }
        self.asker = Some(Asker::Client(client));
        match req {
            Request::CreateSession { username } => self.create_session(&username),
            Request::PostAuthMessageResponse { response } => self.post_response(response),
            Request::StartSession { cmd, env } => self.start_session(cmd, env, now),
            Request::CancelSession => {
                self.cancel_auth();
                self.answer(Response::Success);
            }
        }
        self.drive(now);
        Ok(())
    }

    fn create_session(&mut self, username: &str) {
        if self.session_blocks() {
            return self.answer(error("a session is already active"));
        }
        self.cancel_auth();
        tracing::info!("create_session for {username:?}");
        match self.backend.begin(Class::User, username) {
            Ok(h) => self.send_step(h, None),
            Err(e) => self.answer(seam_reply(&e)),
        }
    }

    fn send_step(&mut self, mut h: B::Handle, answer: Option<Secret>) {
        match h.send_step(answer) {
            Ok(()) => self.auth = Some(Auth::Waiting(h)),
            Err(e) => {
                self.close_handle(h, "authentication");
                self.answer(seam_reply(&e));
            }
        }
    }

    fn post_response(&mut self, answer: Option<Secret>) {
        match self.auth.take() {
            Some(Auth::Pending(h)) => self.send_step(h, answer),
            other => {
                let msg = match other {
                    None => "no session under configuration",
                    Some(Auth::Authenticated(..)) => "session has no pending questions",
                    Some(Auth::Waiting(_) | Auth::Cancelled(_) | Auth::Pending(_)) => {
                        "a step is already in progress"
                    }
                };
                self.auth = other;
                self.answer(error(msg));
            }
        }
    }

    fn start_session(&mut self, cmd: Vec<String>, env: Vec<String>, now: Instant) {
        if self.session_blocks() {
            return self.answer(error("a session is already active"));
        }
        let (handle, account) = match self.auth.take() {
            Some(Auth::Authenticated(h, a)) => (h, a),
            None => return self.answer(error("no session under configuration")),
            pending => {
                self.auth = pending;
                return self.answer(error("session is not ready"));
            }
        };
        if let UserSession::Closing { queued, .. } = &mut self.session {
            // Only one user session at a time: wait (bounded) for the old one to close.
            tracing::info!("start_session: waiting for the previous session to close");
            *queued = Some(Queued {
                handle,
                account,
                cmd,
                env,
                deadline: now + LOGOUT_WAIT,
            });
            return;
        }
        self.open_session(handle, account, cmd, env);
    }

    /// The first step of a user session start: the open request.
    fn open_session(
        &mut self,
        mut handle: B::Handle,
        account: String,
        cmd: Vec<String>,
        env: Vec<String>,
    ) {
        let spec = OpenSpec {
            vt: self.cfg.session_vt,
            env: Vec::new(),
        };
        match handle.send_open(&spec) {
            Ok(()) => {
                self.session = UserSession::Opening {
                    handle,
                    account,
                    cmd,
                    env,
                }
            }
            Err(e) => self.start_failed(handle, &e),
        }
    }

    /// After the open: KD_GRAPHICS, the ready pipe and the spawn request.
    fn spawn_session(
        &mut self,
        mut handle: B::Handle,
        account: String,
        cmd: Vec<String>,
        env: Vec<String>,
    ) {
        let vt = self.cfg.session_vt;
        let prepared = self
            .vt
            .set_graphics(vt)
            .map_err(|e| SeamError::Other(format!("KD_GRAPHICS on VT {vt}: {e}")))
            .and_then(|()| {
                crate::sys::pipe_cloexec(0).map_err(|e| SeamError::Other(format!("pipe: {e}")))
            });
        let (r, w) = match prepared {
            Ok(p) => p,
            Err(e) => return self.start_failed(handle, &e),
        };
        let mut env = env;
        env.push(format!("{READY_ENV}={READY_FD}"));
        // The write end is moved into the spec and dropped once the request is sent, so EOF on
        // `r` means the session closed it.
        match handle.send_spawn(SpawnSpec {
            cmd,
            env,
            extra_fd: Some((w, READY_FD)),
        }) {
            Ok(()) => {
                self.session = UserSession::Spawning {
                    handle,
                    account,
                    ready: r,
                }
            }
            Err(e) => self.start_failed(handle, &e),
        }
    }

    fn start_failed(&mut self, handle: B::Handle, e: &SeamError) {
        tracing::error!("session start failed: {e}");
        self.close_handle(handle, "user session");
        self.answer(seam_reply(e));
    }

    fn drive_session(&mut self, now: Instant) -> bool {
        match std::mem::replace(&mut self.session, UserSession::None) {
            UserSession::Opening {
                mut handle,
                account,
                cmd,
                env,
            } => match handle.take_reply() {
                Ok(None) => {
                    self.session = UserSession::Opening {
                        handle,
                        account,
                        cmd,
                        env,
                    };
                    false
                }
                Ok(Some(Reply::Opened)) => {
                    self.spawn_session(handle, account, cmd, env);
                    true
                }
                Ok(Some(r)) => {
                    self.start_failed(handle, &unexpected(&r));
                    true
                }
                Err(e) => {
                    self.start_failed(handle, &e);
                    true
                }
            },
            UserSession::Spawning {
                mut handle,
                account,
                ready,
            } => match handle.take_reply() {
                Ok(None) => {
                    self.session = UserSession::Spawning {
                        handle,
                        account,
                        ready,
                    };
                    false
                }
                Ok(Some(Reply::Spawned)) => {
                    let child = handle.into_child();
                    tracing::info!("session for {account:?} started (pid {})", child.pid());
                    self.session = UserSession::Starting {
                        run: Running {
                            child,
                            started: now,
                        },
                        ready,
                        deadline: now + self.cfg.ready_timeout,
                    };
                    true
                }
                // A failed spawn, or the worker's exit racing its reply: nothing was spawned.
                Ok(Some(r)) => {
                    self.start_failed(handle, &unexpected(&r));
                    true
                }
                Err(e) => {
                    self.start_failed(handle, &e);
                    true
                }
            },
            other => {
                self.session = other;
                false
            }
        }
    }

    /// The session signalled `ready` (or closed the pipe, or timed out): switch to its VT and
    /// only then put `success` in the reply slot.
    pub fn on_ready(&mut self) {
        let (run, ready) = match std::mem::replace(&mut self.session, UserSession::None) {
            UserSession::Starting { run, ready, .. } => (run, ready),
            other => {
                self.session = other;
                return;
            }
        };
        self.session = UserSession::Running {
            run,
            chan: Some(ready),
        };
        let resp = match self.activate(self.cfg.session_vt) {
            Ok(()) => Response::Success,
            Err(e) => error(format!("could not switch to the session VT: {e}")),
        };
        self.answer(resp);
    }

    /// Fires due timers: the ready timeout, the logout fallback, the queued-start limit and a
    /// scheduled greeter restart.
    pub fn on_tick(&mut self, now: Instant) {
        if let UserSession::Starting { deadline, .. } = &self.session
            && now >= *deadline
        {
            tracing::warn!(
                "session not ready after {:?}; switching anyway",
                self.cfg.ready_timeout
            );
            self.on_ready();
        }
        let hand_off = match &mut self.greeter {
            Greeter::Up { hand_off, .. } => Some(hand_off),
            Greeter::Starting(g) => Some(&mut g.hand_off),
            _ => None,
        };
        if let Some(h) = hand_off
            && h.as_ref().is_some_and(|h| now >= h.deadline)
        {
            *h = None;
            tracing::warn!("logout: fallback (greeter not ready after {LOGOUT_WAIT:?})");
            self.switch_to_greeter();
        }
        if let UserSession::Closing {
            queued: Some(q), ..
        } = &self.session
            && now >= q.deadline
        {
            tracing::error!("start_session: the previous session did not close in time");
            self.drop_queued("the previous session did not close in time");
        }
        if let Greeter::Retry(d) = self.greeter
            && now >= d
        {
            self.start_greeter(now, None);
        }
        self.drive(now);
    }

    /// Reaps exited children (on `SIGCHLD`): the session, the greeter and closed workers. A
    /// starting session that died before it was ready answers its `start_session` with an
    /// error.
    pub fn poll_children(&mut self, now: Instant) {
        let session_exit = match &mut self.session {
            UserSession::Starting { run, .. }
            | UserSession::Running { run, .. }
            | UserSession::Closing { run, .. } => reap(&mut run.child, "session"),
            _ => None,
        };
        if session_exit.is_some() {
            match std::mem::replace(&mut self.session, UserSession::None) {
                UserSession::Starting { .. } => {
                    self.answer(error("the session exited before it was ready"));
                }
                UserSession::Closing { queued, .. } => {
                    // The greeter was brought up at `quit`; it (or its fallback) owns the switch.
                    tracing::info!("logout: session closed");
                    self.backoff.reset();
                    if let Some(q) = queued {
                        self.open_session(q.handle, q.account, q.cmd, q.env);
                    }
                }
                UserSession::Running { .. } => {
                    tracing::info!("session ended; restarting the greeter");
                    self.backoff.reset();
                    match self.greeter {
                        Greeter::Up { .. } => {
                            let _ = self.activate(self.cfg.greeter_vt);
                        }
                        // Its start switches to it when it completes.
                        Greeter::Starting(_) => {}
                        Greeter::Idle | Greeter::Retry(_) => self.start_greeter(now, None),
                    }
                }
                other => self.session = other,
            }
        }
        let greeter_exit = match &mut self.greeter {
            Greeter::Up { run, .. } => reap(&mut run.child, "greeter").map(|_| run.started),
            _ => None,
        };
        if let Some(started) = greeter_exit {
            let lived = now.saturating_duration_since(started);
            let hand_off = match std::mem::replace(&mut self.greeter, Greeter::Idle) {
                Greeter::Up { hand_off, .. } => hand_off,
                other => {
                    self.greeter = other;
                    None
                }
            };
            self.cancel_auth();
            self.drop_queued("the greeter exited");
            if hand_off.is_some() {
                tracing::warn!("logout: greeter exited before ready");
                self.switch_to_greeter();
            }
            if !self.session_blocks() {
                let delay = self.backoff.on_failure(lived);
                tracing::warn!("greeter exited; restarting in {delay:?}");
                self.greeter = Greeter::Retry(now + delay);
            }
        }
        self.closing
            .retain_mut(|w| reap(w, "closed session worker").is_none());
        self.drive(now);
    }

    /// Orderly stop (the loop has ended, so this may block): close every unspawned handle
    /// (outstanding requests included), SIGTERM every child, wait once for all of them within
    /// `grace`, SIGKILL the survivors and wait once more.
    pub fn shutdown(&mut self, grace: Duration) {
        let mut runs: Vec<Running<B::Handle>> = Vec::new();
        if let Some(a) = self.auth.take() {
            let (Auth::Waiting(h)
            | Auth::Cancelled(h)
            | Auth::Pending(h)
            | Auth::Authenticated(h, _)) = a;
            self.close_handle(h, "authentication");
        }
        match std::mem::replace(&mut self.session, UserSession::None) {
            UserSession::Opening { handle, .. } | UserSession::Spawning { handle, .. } => {
                self.close_handle(handle, "user session");
            }
            UserSession::Starting { run, .. } | UserSession::Running { run, .. } => runs.push(run),
            UserSession::Closing { run, queued, .. } => {
                runs.push(run);
                if let Some(q) = queued {
                    self.close_handle(q.handle, "queued session");
                }
            }
            UserSession::None => {}
        }
        match std::mem::replace(&mut self.greeter, Greeter::Idle) {
            Greeter::Starting(g) => self.close_handle(g.handle, "greeter handle"),
            Greeter::Up { run, .. } => runs.push(run),
            Greeter::Idle | Greeter::Retry(_) => {}
        }
        self.asker = None;
        self.reply = None;
        let mut closing = std::mem::take(&mut self.closing);
        for r in &mut runs {
            let _ = r.child.signal(libc::SIGTERM);
        }
        let mut alive = vec![true; runs.len()];
        wait_all(&mut runs, &mut alive, &mut closing, Instant::now() + grace);
        // Dropping each run drops its child: the worker closes PAM once it has reaped the user
        // process.
        for (r, a) in runs.iter_mut().zip(&alive) {
            if *a {
                let _ = r.child.signal(libc::SIGKILL);
            }
        }
        wait_all(
            &mut runs,
            &mut alive,
            &mut closing,
            Instant::now() + Duration::from_secs(1),
        );
    }
}

/// Shutdown's one wait for every child still alive (and every closed worker) until `end`.
fn wait_all<H: SessionHandle>(
    runs: &mut [Running<H>],
    alive: &mut [bool],
    closing: &mut Vec<H::Child>,
    end: Instant,
) {
    loop {
        for (r, a) in runs.iter_mut().zip(alive.iter_mut()) {
            if *a && exited(&mut r.child, "session") {
                *a = false;
            }
        }
        closing.retain_mut(|w| !exited(w, "closed session worker"));
        if (!alive.contains(&true) && closing.is_empty()) || Instant::now() >= end {
            return;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn reap<C: Child>(child: &mut C, what: &str) -> Option<ExitStatus> {
    match child.try_wait() {
        Ok(Some(st)) => {
            tracing::info!("{what} (pid {}) exited: {st:?}", child.pid());
            Some(st)
        }
        Ok(None) => None,
        // A wait error says nothing about the process: keep the state and look again later.
        Err(e) => {
            tracing::error!("waiting for {what}: {e}");
            None
        }
    }
}

/// Shutdown's wait: only `Ok(Some(_))` means exited; a wait error keeps the child alive.
fn exited<C: Child>(child: &mut C, what: &str) -> bool {
    reap(child, what).is_some()
}

#[cfg(test)]
#[path = "daemon_tests.rs"]
mod tests;
