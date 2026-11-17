//! The daemon core: the greetd protocol, the greeter and user-session lifecycle and the VT
//! switches, as an event-driven state machine over the [`Backend`] seam and the [`Vt`] trait.
//! The poll loop ([`crate::server`]) turns fds, signals and timers into calls here; the tests
//! call it directly with fakes.

use std::io;
use std::os::fd::{AsRawFd, OwnedFd, RawFd};
use std::time::{Duration, Instant};

use crucible_greetd::{ErrorType, Request, Response, Secret};

use crate::seam::{
    AuthStep, Backend, Child, Class, ExitStatus, OpenSpec, SeamError, SessionHandle, SpawnSpec,
};
use crate::vt::Vt;

/// The fd number the ready pipe's write end gets in the session process.
pub const READY_FD: RawFd = 3;
/// The env var naming [`READY_FD`] in the session process.
pub const READY_ENV: &str = "CDE_LOGIN_READY_FD";
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

struct Running<H: SessionHandle> {
    handle: H,
    child: H::Child,
    started: Instant,
}

enum Auth<H> {
    Pending(H),
    Authenticated(H, String),
}

enum UserSession<H: SessionHandle> {
    None,
    Starting {
        run: Running<H>,
        ready: OwnedFd,
        deadline: Instant,
    },
    Running(Running<H>),
}

/// The daemon state machine.
pub struct Daemon<B: Backend, V: Vt> {
    backend: B,
    vt: V,
    cfg: Config,
    greeter: Option<Running<B::Handle>>,
    greeter_due: Option<Instant>,
    backoff: Backoff,
    auth: Option<Auth<B::Handle>>,
    session: UserSession<B::Handle>,
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

fn close_handle<H: SessionHandle>(handle: H, what: &str) {
    if let Err(e) = handle.close() {
        tracing::warn!("closing {what}: {e}");
    }
}

fn make_pipe() -> io::Result<(OwnedFd, OwnedFd)> {
    let mut fds = [0 as libc::c_int; 2];
    #[allow(unsafe_code)]
    // SAFETY: pipe2 writes two fds into the array; on success we take ownership of both.
    let rc = unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) };
    if rc < 0 {
        return Err(io::Error::last_os_error());
    }
    #[allow(unsafe_code)]
    // SAFETY: both fds were just created and are owned by nobody else.
    Ok(unsafe {
        use std::os::fd::FromRawFd;
        (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1]))
    })
}

impl<B: Backend, V: Vt> Daemon<B, V> {
    /// A daemon with nothing running yet; call [`Daemon::start`].
    pub fn new(backend: B, vt: V, cfg: Config) -> Self {
        Self {
            backend,
            vt,
            cfg,
            greeter: None,
            greeter_due: None,
            backoff: Backoff::default(),
            auth: None,
            session: UserSession::None,
        }
    }

    /// Starts the greeter on its VT and switches to it.
    pub fn start(&mut self, now: Instant) {
        self.start_greeter(now);
    }

    fn activate(&mut self, vt: u32) -> io::Result<()> {
        let r = self.vt.activate(vt);
        if let Err(e) = &r {
            tracing::error!("activating VT {vt}: {e}");
        }
        r
    }

    fn try_start_greeter(&mut self) -> Result<Running<B::Handle>, SeamError> {
        let mut handle = self.backend.begin(Class::Greeter, &self.cfg.greeter_user)?;
        let mut account = None;
        for _ in 0..GREETER_MAX_STEPS {
            match handle.auth_step(None) {
                Ok(AuthStep::Authenticated { account: a }) => {
                    account = Some(a);
                    break;
                }
                Ok(AuthStep::Prompt(..)) => {}
                Err(e) => {
                    close_handle(handle, "greeter handle");
                    return Err(e);
                }
            }
        }
        let Some(account) = account else {
            close_handle(handle, "greeter handle");
            return Err(SeamError::Other(
                "greeter conversation did not finish".into(),
            ));
        };
        let sock = format!("GREETD_SOCK={}", self.cfg.sock_path);
        let res = handle
            .open(&OpenSpec {
                account: account.clone(),
                vt: self.cfg.greeter_vt,
                class: Class::Greeter,
                env: vec![sock.clone()],
            })
            .and_then(|()| {
                handle.spawn(SpawnSpec {
                    account,
                    cmd: self.cfg.greeter_cmd.clone(),
                    env: vec![sock],
                    vt: self.cfg.greeter_vt,
                    extra_fd: None,
                })
            });
        match res {
            Ok(child) => Ok(Running {
                handle,
                child,
                started: Instant::now(),
            }),
            Err(e) => {
                close_handle(handle, "greeter session");
                Err(e)
            }
        }
    }

    fn start_greeter(&mut self, now: Instant) {
        self.greeter_due = None;
        match self.try_start_greeter() {
            Ok(run) => {
                tracing::info!("greeter started (pid {})", run.child.pid());
                self.greeter = Some(run);
                let _ = self.activate(self.cfg.greeter_vt);
            }
            Err(e) => {
                let delay = self.backoff.on_failure(Duration::ZERO);
                tracing::error!("greeter start failed: {e}; retrying in {delay:?}");
                self.greeter_due = Some(now + delay);
            }
        }
    }

    /// Whether a user session is starting or running.
    pub fn session_active(&self) -> bool {
        !matches!(self.session, UserSession::None)
    }

    /// Whether the greeter is running.
    pub fn greeter_running(&self) -> bool {
        self.greeter.is_some()
    }

    /// When a scheduled greeter restart is due.
    pub fn greeter_due(&self) -> Option<Instant> {
        self.greeter_due
    }

    /// The ready pipe's read end while a session is starting.
    pub fn ready_fd(&self) -> Option<RawFd> {
        match &self.session {
            UserSession::Starting { ready, .. } => Some(ready.as_raw_fd()),
            _ => None,
        }
    }

    /// The earliest timer the poll loop must wake for.
    pub fn next_deadline(&self) -> Option<Instant> {
        let ready = match &self.session {
            UserSession::Starting { deadline, .. } => Some(*deadline),
            _ => None,
        };
        match (ready, self.greeter_due) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        }
    }

    fn cancel_auth(&mut self) {
        if let Some(auth) = self.auth.take() {
            let h = match auth {
                Auth::Pending(h) | Auth::Authenticated(h, _) => h,
            };
            close_handle(h, "authentication");
        }
    }

    /// Serves one greeter request. `None` means the reply is deferred (a starting session):
    /// it comes back from [`Daemon::on_ready`], [`Daemon::on_tick`] or
    /// [`Daemon::poll_children`].
    pub fn handle(&mut self, req: Request, now: Instant) -> Option<Response> {
        match req {
            Request::CreateSession { username } => Some(self.create_session(&username)),
            Request::PostAuthMessageResponse { response } => Some(self.post_response(response)),
            Request::StartSession { cmd, env } => self.start_session(cmd, env, now),
            Request::CancelSession => {
                self.cancel_auth();
                Some(Response::Success)
            }
        }
    }

    fn create_session(&mut self, username: &str) -> Response {
        if self.session_active() {
            return error("a session is already active");
        }
        self.cancel_auth();
        tracing::info!("create_session for {username:?}");
        match self.backend.begin(Class::User, username) {
            Ok(h) => {
                self.auth = Some(Auth::Pending(h));
                self.step(None)
            }
            Err(e) => seam_reply(&e),
        }
    }

    fn post_response(&mut self, answer: Option<Secret>) -> Response {
        match &self.auth {
            None => error("no session under configuration"),
            Some(Auth::Authenticated(..)) => error("session has no pending questions"),
            Some(Auth::Pending(_)) => self.step(answer),
        }
    }

    fn step(&mut self, answer: Option<Secret>) -> Response {
        let Some(Auth::Pending(mut h)) = self.auth.take() else {
            return error("no session under configuration");
        };
        match h.auth_step(answer) {
            Ok(AuthStep::Prompt(kind, text)) => {
                self.auth = Some(Auth::Pending(h));
                Response::AuthMessage {
                    auth_message_type: kind,
                    auth_message: text,
                }
            }
            Ok(AuthStep::Authenticated { account }) => {
                tracing::info!("authenticated as {account:?}");
                self.auth = Some(Auth::Authenticated(h, account));
                Response::Success
            }
            Err(e) => {
                tracing::info!("authentication failed: {e}");
                close_handle(h, "authentication");
                seam_reply(&e)
            }
        }
    }

    fn start_session(
        &mut self,
        cmd: Vec<String>,
        env: Vec<String>,
        now: Instant,
    ) -> Option<Response> {
        if self.session_active() {
            return Some(error("a session is already active"));
        }
        let (mut handle, account) = match self.auth.take() {
            Some(Auth::Authenticated(h, a)) => (h, a),
            Some(pending @ Auth::Pending(_)) => {
                self.auth = Some(pending);
                return Some(error("session is not ready"));
            }
            None => return Some(error("no session under configuration")),
        };
        let vt = self.cfg.session_vt;
        let res = (|| {
            handle.open(&OpenSpec {
                account: account.clone(),
                vt,
                class: Class::User,
                env: Vec::new(),
            })?;
            self.vt
                .set_graphics(vt)
                .map_err(|e| SeamError::Other(format!("KD_GRAPHICS on VT {vt}: {e}")))?;
            let (r, w) = make_pipe().map_err(|e| SeamError::Other(format!("pipe: {e}")))?;
            let mut env = env;
            env.push(format!("{READY_ENV}={READY_FD}"));
            let child = handle.spawn(SpawnSpec {
                account: account.clone(),
                cmd,
                env,
                vt,
                extra_fd: Some((w, READY_FD)),
            })?;
            // The write end was moved into the spec and dropped by the backend after spawn,
            // so EOF on `r` means the session closed it.
            Ok((child, r))
        })();
        match res {
            Ok((child, ready)) => {
                tracing::info!("session for {account:?} started (pid {})", child.pid());
                self.session = UserSession::Starting {
                    run: Running {
                        handle,
                        child,
                        started: now,
                    },
                    ready,
                    deadline: now + self.cfg.ready_timeout,
                };
                None
            }
            Err(e) => {
                tracing::error!("session start failed: {e}");
                close_handle(handle, "user session");
                Some(seam_reply(&e))
            }
        }
    }

    /// The session signalled `ready` (or closed the pipe, or timed out): switch to its VT and
    /// only then reply `success`.
    pub fn on_ready(&mut self) -> Option<Response> {
        let UserSession::Starting { run, .. } =
            std::mem::replace(&mut self.session, UserSession::None)
        else {
            return None;
        };
        self.session = UserSession::Running(run);
        match self.activate(self.cfg.session_vt) {
            Ok(()) => Some(Response::Success),
            Err(e) => Some(error(format!("could not switch to the session VT: {e}"))),
        }
    }

    /// Fires due timers: the ready timeout and a scheduled greeter restart.
    pub fn on_tick(&mut self, now: Instant) -> Option<Response> {
        let mut reply = None;
        if let UserSession::Starting { deadline, .. } = &self.session
            && now >= *deadline
        {
            tracing::warn!(
                "session not ready after {:?}; switching anyway",
                self.cfg.ready_timeout
            );
            reply = self.on_ready();
        }
        if self.greeter_due.is_some_and(|d| now >= d) && self.greeter.is_none() {
            self.start_greeter(now);
        }
        reply
    }

    /// Reaps exited children (on `SIGCHLD`). Returns the deferred reply when a starting
    /// session died before it was ready.
    pub fn poll_children(&mut self, now: Instant) -> Option<Response> {
        let mut reply = None;
        let session_exit = match &mut self.session {
            UserSession::Starting { run, .. } | UserSession::Running(run) => {
                reap(&mut run.child, "session")
            }
            UserSession::None => None,
        };
        if session_exit.is_some() {
            match std::mem::replace(&mut self.session, UserSession::None) {
                UserSession::Starting { run, .. } => {
                    close_handle(run.handle, "user session");
                    reply = Some(error("the session exited before it was ready"));
                }
                UserSession::Running(run) => {
                    close_handle(run.handle, "user session");
                    tracing::info!("session ended; restarting the greeter");
                    self.backoff.reset();
                    if self.greeter.is_none() {
                        self.start_greeter(now);
                    } else {
                        let _ = self.activate(self.cfg.greeter_vt);
                    }
                }
                UserSession::None => {}
            }
        }
        let greeter_exit = self
            .greeter
            .as_mut()
            .and_then(|g| reap(&mut g.child, "greeter"));
        if greeter_exit.is_some()
            && let Some(g) = self.greeter.take()
        {
            let lived = now.saturating_duration_since(g.started);
            close_handle(g.handle, "greeter session");
            self.cancel_auth();
            if !self.session_active() {
                let delay = self.backoff.on_failure(lived);
                tracing::warn!("greeter exited; restarting in {delay:?}");
                self.greeter_due = Some(now + delay);
            }
        }
        reply
    }

    /// Orderly stop: cancel authentication, SIGTERM the children, SIGKILL after `grace`, close
    /// every open session.
    pub fn shutdown(&mut self, grace: Duration) {
        self.cancel_auth();
        let mut runs: Vec<Running<B::Handle>> = Vec::new();
        match std::mem::replace(&mut self.session, UserSession::None) {
            UserSession::Starting { run, .. } | UserSession::Running(run) => runs.push(run),
            UserSession::None => {}
        }
        runs.extend(self.greeter.take());
        self.greeter_due = None;
        for r in &mut runs {
            let _ = r.child.signal(libc::SIGTERM);
        }
        let end = Instant::now() + grace;
        let mut alive: Vec<bool> = vec![true; runs.len()];
        loop {
            for (r, a) in runs.iter_mut().zip(alive.iter_mut()) {
                if *a && !matches!(r.child.try_wait(), Ok(None)) {
                    *a = false;
                }
            }
            if !alive.contains(&true) || Instant::now() >= end {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        for (r, a) in runs.iter_mut().zip(&alive) {
            if *a {
                let _ = r.child.signal(libc::SIGKILL);
                for _ in 0..50 {
                    if !matches!(r.child.try_wait(), Ok(None)) {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(20));
                }
            }
        }
        for r in runs {
            close_handle(r.handle, "session");
        }
    }
}

fn reap<C: Child>(child: &mut C, what: &str) -> Option<ExitStatus> {
    match child.try_wait() {
        Ok(Some(st)) => {
            tracing::info!("{what} (pid {}) exited: {st:?}", child.pid());
            Some(st)
        }
        Ok(None) => None,
        Err(e) => {
            tracing::error!("waiting for {what}: {e}");
            Some(ExitStatus::Code(-1))
        }
    }
}

#[cfg(test)]
#[path = "daemon_tests.rs"]
mod tests;
