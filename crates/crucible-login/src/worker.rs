//! The PAM backend: one root worker process per session handle.
//!
//! `pam_systemd` registers the logind session for the process that calls `pam_open_session`
//! (and moves it into the session scope), so PAM never runs in the daemon. [`PamBackend::begin`]
//! forks a worker that owns the PAM handle for the session's whole life and talks to the daemon
//! over a `SOCK_SEQPACKET` socketpair:
//!
//! - `Step(answer)` → `Prompt` | `Authenticated` | `AuthError` | `Error` (the first `Step` starts
//!   PAM; PAM's push-driven conversation blocks on the next `Step` for each message);
//! - `Open` → `Ok` | `Error`; `Spawn` (+ the extra fd over `SCM_RIGHTS`) → `Spawned(pid)` (+ a
//!   pidfd for the user process over `SCM_RIGHTS`, absent if `pidfd_open` failed) | `Error`; `Close` → the worker closes PAM and exits.
//!
//! After `Spawned`, the worker forwards `SIGTERM`/`SIGINT`/`SIGHUP` to the user process, reaps
//! it, runs `pam_close_session` + `pam_setcred(DELETE_CRED)` + `pam_end`, and exits with the
//! user process's status, so the daemon-side [`Child`] is the worker itself. SIGKILL cannot be
//! forwarded, so the daemon sends it through the user process's pidfd (never a bare pid).
#![allow(unsafe_code)]

use std::ffi::CString;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::sync::atomic::{AtomicI32, Ordering};

use crucible_greetd::{AuthMessageType, Secret};

use crate::pam::{PAM_DELETE_CRED, PAM_ESTABLISH_CRED, Pam};
use crate::seam::{
    AuthStep, Backend, Child, Class, ExitStatus, OpenSpec, SeamError, SessionHandle, SignalTarget,
    SpawnSpec, signal_target,
};

/// The `PATH` every session starts with.
pub const SESSION_PATH: &str = "/usr/local/bin:/usr/bin:/bin";
const MAX_MSG: usize = 64 * 1024;

// ---------------------------------------------------------------------------------------------
// Wire messages (pure codec).

/// Daemon → worker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Req {
    /// Advance the conversation (answer to the previous prompt; the first one starts PAM).
    Step(Option<String>),
    /// Open the session.
    Open {
        /// VT number.
        vt: u32,
        /// Session class.
        class: Class,
        /// Extra PAM env entries.
        env: Vec<String>,
    },
    /// Spawn the user process; `target_fd` is where the passed fd lands (if one is passed).
    Spawn {
        /// Resolved account.
        account: String,
        /// Command words.
        cmd: Vec<String>,
        /// Extra env entries.
        env: Vec<String>,
        /// VT number.
        vt: u32,
        /// Target fd number for the `SCM_RIGHTS` fd.
        target_fd: Option<i32>,
    },
    /// Close PAM and exit.
    Close,
}

/// Worker → daemon.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Rep {
    /// A conversation message.
    Prompt(AuthMessageType, String),
    /// Auth + account check passed; `PAM_USER`.
    Authenticated(String),
    /// Authentication failed.
    AuthError(String),
    /// Any other failure.
    Error(String),
    /// The request succeeded.
    Ok,
    /// The user process was started.
    Spawned(i32),
}

#[derive(Default)]
struct Enc(Vec<u8>);

impl Enc {
    fn u8(&mut self, v: u8) -> &mut Self {
        self.0.push(v);
        self
    }
    fn u32(&mut self, v: u32) -> &mut Self {
        self.0.extend_from_slice(&v.to_le_bytes());
        self
    }
    fn str(&mut self, s: &str) -> &mut Self {
        self.u32(s.len() as u32);
        self.0.extend_from_slice(s.as_bytes());
        self
    }
    fn opt(&mut self, s: Option<&str>) -> &mut Self {
        match s {
            Some(s) => self.u8(1).str(s),
            None => self.u8(0),
        }
    }
    fn list(&mut self, v: &[String]) -> &mut Self {
        self.u32(v.len() as u32);
        for s in v {
            self.str(s);
        }
        self
    }
}

struct Dec<'a>(&'a [u8]);

type DecResult<T> = Result<T, String>;

impl Dec<'_> {
    fn take(&mut self, n: usize) -> DecResult<&[u8]> {
        if self.0.len() < n {
            return Err("truncated message".into());
        }
        let (a, b) = self.0.split_at(n);
        self.0 = b;
        Ok(a)
    }
    fn u8(&mut self) -> DecResult<u8> {
        Ok(self.take(1)?[0])
    }
    fn u32(&mut self) -> DecResult<u32> {
        let b = self.take(4)?;
        Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }
    fn str(&mut self) -> DecResult<String> {
        let n = self.u32()? as usize;
        String::from_utf8(self.take(n)?.to_vec()).map_err(|_| "invalid utf-8".to_owned())
    }
    fn opt(&mut self) -> DecResult<Option<String>> {
        match self.u8()? {
            0 => Ok(None),
            _ => self.str().map(Some),
        }
    }
    fn list(&mut self) -> DecResult<Vec<String>> {
        let n = self.u32()? as usize;
        if n > MAX_MSG {
            return Err("list too long".into());
        }
        (0..n).map(|_| self.str()).collect()
    }
    fn end<T>(&self, v: T) -> DecResult<T> {
        if self.0.is_empty() {
            Ok(v)
        } else {
            Err("trailing bytes".into())
        }
    }
}

fn class_byte(c: Class) -> u8 {
    match c {
        Class::User => 0,
        Class::Greeter => 1,
    }
}

fn kind_byte(k: AuthMessageType) -> u8 {
    match k {
        AuthMessageType::Visible => 0,
        AuthMessageType::Secret => 1,
        AuthMessageType::Info => 2,
        AuthMessageType::Error => 3,
    }
}

impl Req {
    /// Encodes the request.
    pub fn encode(&self) -> Vec<u8> {
        let mut e = Enc::default();
        match self {
            Req::Step(a) => {
                e.u8(0).opt(a.as_deref());
            }
            Req::Open { vt, class, env } => {
                e.u8(1).u32(*vt).u8(class_byte(*class)).list(env);
            }
            Req::Spawn {
                account,
                cmd,
                env,
                vt,
                target_fd,
            } => {
                e.u8(2).str(account).list(cmd).list(env).u32(*vt);
                match target_fd {
                    Some(fd) => e.u8(1).u32(*fd as u32),
                    None => e.u8(0),
                };
            }
            Req::Close => {
                e.u8(3);
            }
        }
        e.0
    }

    /// Decodes a request.
    pub fn decode(b: &[u8]) -> Result<Self, String> {
        let mut d = Dec(b);
        let r = match d.u8()? {
            0 => Req::Step(d.opt()?),
            1 => Req::Open {
                vt: d.u32()?,
                class: if d.u8()? == 0 {
                    Class::User
                } else {
                    Class::Greeter
                },
                env: d.list()?,
            },
            2 => Req::Spawn {
                account: d.str()?,
                cmd: d.list()?,
                env: d.list()?,
                vt: d.u32()?,
                target_fd: match d.u8()? {
                    0 => None,
                    _ => Some(d.u32()? as i32),
                },
            },
            3 => Req::Close,
            t => return Err(format!("unknown request tag {t}")),
        };
        d.end(r)
    }
}

impl Rep {
    /// Encodes the reply.
    pub fn encode(&self) -> Vec<u8> {
        let mut e = Enc::default();
        match self {
            Rep::Prompt(k, t) => e.u8(0).u8(kind_byte(*k)).str(t),
            Rep::Authenticated(a) => e.u8(1).str(a),
            Rep::AuthError(m) => e.u8(2).str(m),
            Rep::Error(m) => e.u8(3).str(m),
            Rep::Ok => e.u8(4),
            Rep::Spawned(p) => e.u8(5).u32(*p as u32),
        };
        e.0
    }

    /// Decodes a reply.
    pub fn decode(b: &[u8]) -> Result<Self, String> {
        let mut d = Dec(b);
        let r = match d.u8()? {
            0 => {
                let k = match d.u8()? {
                    0 => AuthMessageType::Visible,
                    1 => AuthMessageType::Secret,
                    2 => AuthMessageType::Info,
                    3 => AuthMessageType::Error,
                    k => return Err(format!("unknown message type {k}")),
                };
                Rep::Prompt(k, d.str()?)
            }
            1 => Rep::Authenticated(d.str()?),
            2 => Rep::AuthError(d.str()?),
            3 => Rep::Error(d.str()?),
            4 => Rep::Ok,
            5 => Rep::Spawned(d.u32()? as i32),
            t => return Err(format!("unknown reply tag {t}")),
        };
        d.end(r)
    }
}

// ---------------------------------------------------------------------------------------------
// Pure builders: environment, argv, fd plan.

/// The account fields the spawn needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Account {
    /// Login name.
    pub name: String,
    /// uid.
    pub uid: u32,
    /// Primary gid.
    pub gid: u32,
    /// Home directory.
    pub home: String,
    /// Login shell (`/bin/sh` if empty).
    pub shell: String,
}

/// The user process environment, built from scratch: the PAM env, then the account and XDG
/// variables, then `extra`; a later entry replaces an earlier one with the same key. Nothing
/// from the daemon's own environment is included.
pub fn build_env(
    pam_env: &[String],
    acct: &Account,
    vt: u32,
    class: Class,
    extra: &[String],
) -> Vec<String> {
    let base = [
        format!("HOME={}", acct.home),
        format!("USER={}", acct.name),
        format!("LOGNAME={}", acct.name),
        format!("SHELL={}", acct.shell),
        format!("PATH={SESSION_PATH}"),
        "XDG_SESSION_TYPE=wayland".to_owned(),
        format!("XDG_SESSION_CLASS={}", class.xdg_class()),
        "XDG_SEAT=seat0".to_owned(),
        format!("XDG_VTNR={vt}"),
    ];
    let mut out: Vec<String> = Vec::new();
    for kv in pam_env.iter().chain(base.iter()).chain(extra.iter()) {
        let Some((k, _)) = kv.split_once('=') else {
            continue;
        };
        out.retain(|e| e.split_once('=').map(|(ek, _)| ek) != Some(k));
        out.push(kv.clone());
    }
    out
}

/// The PAM env entries set before `pam_open_session`.
pub fn session_pam_env(vt: u32, class: Class, extra: &[String]) -> Vec<String> {
    let mut v = vec![
        "XDG_SESSION_TYPE=wayland".to_owned(),
        format!("XDG_SESSION_CLASS={}", class.xdg_class()),
        "XDG_SEAT=seat0".to_owned(),
        format!("XDG_VTNR={vt}"),
    ];
    v.extend(extra.iter().cloned());
    v
}

/// The shell the command runs through: the account's shell when `/etc/shells` lists it, else
/// `/bin/sh`. A system account such as the greeter's has `/usr/sbin/nologin`, which would
/// refuse the command (greetd always runs `/bin/sh`).
pub fn exec_shell<'a>(shell: &'a str, etc_shells: &str) -> &'a str {
    let listed = etc_shells
        .lines()
        .map(str::trim)
        .any(|l| !l.starts_with('#') && l == shell);
    if listed { shell } else { "/bin/sh" }
}

/// The exec argv: the login shell as a login shell running the command words as `"$@"`, so
/// no word is ever re-split or re-quoted.
pub fn login_argv(shell: &str, cmd: &[String]) -> Vec<String> {
    let mut v = vec![
        shell.to_owned(),
        "-l".to_owned(),
        "-c".to_owned(),
        "exec \"$@\"".to_owned(),
        "cde-login-session".to_owned(),
    ];
    v.extend(cmd.iter().cloned());
    v
}

/// The fds that survive into the user process: stdio (the VT) and the extra fd's target.
/// Returns the inclusive ranges to close (`close_range`), everything else.
pub fn close_ranges(target: Option<RawFd>) -> Vec<(u32, u32)> {
    match target {
        Some(t) if t > 3 => vec![(3, t as u32 - 1), (t as u32 + 1, u32::MAX)],
        Some(3) => vec![(4, u32::MAX)],
        _ => vec![(3, u32::MAX)],
    }
}

// ---------------------------------------------------------------------------------------------
// Socket I/O with optional SCM_RIGHTS.

fn send(sock: RawFd, data: &[u8], fd: Option<RawFd>) -> io::Result<()> {
    let mut iov = libc::iovec {
        iov_base: data.as_ptr() as *mut _,
        iov_len: data.len(),
    };
    // SAFETY: CMSG_SPACE is a pure size computation.
    let space = unsafe { libc::CMSG_SPACE(std::mem::size_of::<RawFd>() as u32) } as usize;
    let mut cbuf = vec![0u8; space];
    // SAFETY: zeroed msghdr is valid; pointers reference live locals.
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    if let Some(fd) = fd {
        msg.msg_control = cbuf.as_mut_ptr().cast();
        msg.msg_controllen = space as _;
        // SAFETY: the control buffer is CMSG_SPACE(sizeof fd) bytes.
        unsafe {
            let c = libc::CMSG_FIRSTHDR(&msg);
            (*c).cmsg_level = libc::SOL_SOCKET;
            (*c).cmsg_type = libc::SCM_RIGHTS;
            (*c).cmsg_len = libc::CMSG_LEN(std::mem::size_of::<RawFd>() as u32) as _;
            std::ptr::write_unaligned(libc::CMSG_DATA(c).cast::<RawFd>(), fd);
        }
    }
    loop {
        // SAFETY: msg is fully initialised.
        let n = unsafe { libc::sendmsg(sock, &msg, libc::MSG_NOSIGNAL) };
        if n >= 0 {
            return Ok(());
        }
        let e = io::Error::last_os_error();
        if e.kind() != io::ErrorKind::Interrupted {
            return Err(e);
        }
    }
}

/// Receives one message (`None`: peer closed) and an optional passed fd (close-on-exec).
fn recv(sock: RawFd) -> io::Result<Option<(Vec<u8>, Option<OwnedFd>)>> {
    let mut buf = vec![0u8; MAX_MSG];
    let mut iov = libc::iovec {
        iov_base: buf.as_mut_ptr().cast(),
        iov_len: buf.len(),
    };
    // SAFETY: pure size computation.
    let space = unsafe { libc::CMSG_SPACE(std::mem::size_of::<RawFd>() as u32) } as usize;
    let mut cbuf = vec![0u8; space];
    // SAFETY: zeroed msghdr is valid.
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = cbuf.as_mut_ptr().cast();
    msg.msg_controllen = space as _;
    let n = loop {
        // SAFETY: msg points at live buffers.
        let n = unsafe { libc::recvmsg(sock, &mut msg, libc::MSG_CMSG_CLOEXEC) };
        if n >= 0 {
            break n as usize;
        }
        let e = io::Error::last_os_error();
        if e.kind() != io::ErrorKind::Interrupted {
            return Err(e);
        }
    };
    let mut fd = None;
    // SAFETY: walking the control buffer the kernel filled.
    unsafe {
        let c = libc::CMSG_FIRSTHDR(&msg);
        if !c.is_null() && (*c).cmsg_level == libc::SOL_SOCKET && (*c).cmsg_type == libc::SCM_RIGHTS
        {
            let raw = std::ptr::read_unaligned(libc::CMSG_DATA(c).cast::<RawFd>());
            fd = Some(OwnedFd::from_raw_fd(raw));
        }
    }
    if n == 0 && fd.is_none() {
        return Ok(None);
    }
    buf.truncate(n);
    Ok(Some((buf, fd)))
}

// ---------------------------------------------------------------------------------------------
// Daemon side.

/// The production backend.
#[derive(Debug, Default)]
pub struct PamBackend;

/// The daemon's end of one worker.
#[derive(Debug)]
pub struct PamHandle {
    sock: OwnedFd,
    pid: i32,
    spawned: bool,
}

/// The worker process, as the daemon sees the session.
#[derive(Debug)]
pub struct WorkerChild {
    worker: i32,
    user: i32,
    /// The user process's pidfd from the `Spawned` reply; `None` if the worker sent none.
    user_pidfd: Option<OwnedFd>,
    status: Option<ExitStatus>,
}

fn other(e: impl std::fmt::Display) -> SeamError {
    SeamError::Other(e.to_string())
}

impl Backend for PamBackend {
    type Handle = PamHandle;

    fn begin(&mut self, class: Class, user: &str) -> Result<PamHandle, SeamError> {
        let mut fds = [0 as libc::c_int; 2];
        // SAFETY: socketpair fills two fds.
        let rc = unsafe {
            libc::socketpair(
                libc::AF_UNIX,
                libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC,
                0,
                fds.as_mut_ptr(),
            )
        };
        if rc < 0 {
            return Err(other(io::Error::last_os_error()));
        }
        // SAFETY: both fds are fresh and ours.
        let (ours, theirs) =
            unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) };
        // Prepared before fork so the child allocates nothing it does not have to.
        let user = user.to_owned();
        // SAFETY: the daemon is single-threaded; the child never returns into daemon code.
        let pid = unsafe { libc::fork() };
        if pid < 0 {
            return Err(other(io::Error::last_os_error()));
        }
        if pid == 0 {
            let sock = theirs.as_raw_fd();
            std::mem::forget(ours);
            let code = std::panic::catch_unwind(|| worker_main(sock, class, &user)).unwrap_or(1);
            // SAFETY: leaving the forked worker without running daemon destructors.
            unsafe { libc::_exit(code) };
        }
        drop(theirs);
        Ok(PamHandle {
            sock: ours,
            pid,
            spawned: false,
        })
    }
}

impl PamHandle {
    fn call(&mut self, req: &Req, fd: Option<RawFd>) -> Result<Rep, SeamError> {
        self.call_fd(req, fd).map(|(r, _)| r)
    }

    /// [`Self::call`], also returning an fd passed with the reply.
    fn call_fd(
        &mut self,
        req: &Req,
        fd: Option<RawFd>,
    ) -> Result<(Rep, Option<OwnedFd>), SeamError> {
        send(self.sock.as_raw_fd(), &req.encode(), fd)
            .map_err(|e| other(format!("worker: {e}")))?;
        match recv(self.sock.as_raw_fd()).map_err(|e| other(format!("worker: {e}")))? {
            Some((b, rfd)) => Rep::decode(&b)
                .map(|r| (r, rfd))
                .map_err(|e| other(format!("worker reply: {e}"))),
            None => Err(other("session worker exited")),
        }
    }
}

fn unexpected(r: Rep) -> SeamError {
    match r {
        Rep::AuthError(m) => SeamError::Auth(m),
        Rep::Error(m) => SeamError::Other(m),
        r => SeamError::Other(format!("unexpected worker reply {r:?}")),
    }
}

impl SessionHandle for PamHandle {
    type Child = WorkerChild;

    fn auth_step(&mut self, answer: Option<Secret>) -> Result<AuthStep, SeamError> {
        let req = Req::Step(answer.map(|s| s.expose().to_owned()));
        match self.call(&req, None)? {
            Rep::Prompt(k, t) => Ok(AuthStep::Prompt(k, t)),
            Rep::Authenticated(account) => Ok(AuthStep::Authenticated { account }),
            r => Err(unexpected(r)),
        }
    }

    fn open(&mut self, spec: &OpenSpec) -> Result<(), SeamError> {
        let req = Req::Open {
            vt: spec.vt,
            class: spec.class,
            env: spec.env.clone(),
        };
        match self.call(&req, None)? {
            Rep::Ok => Ok(()),
            r => Err(unexpected(r)),
        }
    }

    fn spawn(&mut self, spec: SpawnSpec) -> Result<WorkerChild, SeamError> {
        let (fd, target) = match &spec.extra_fd {
            Some((fd, t)) => (Some(fd.as_raw_fd()), Some(*t)),
            None => (None, None),
        };
        let req = Req::Spawn {
            account: spec.account.clone(),
            cmd: spec.cmd.clone(),
            env: spec.env.clone(),
            vt: spec.vt,
            target_fd: target,
        };
        let rep = self.call_fd(&req, fd);
        // Our copy of the extra fd is dropped here; the worker dropped its own after the fork.
        drop(spec);
        match rep? {
            (Rep::Spawned(user), user_pidfd) => {
                self.spawned = true;
                if user_pidfd.is_none() {
                    tracing::warn!("no pidfd for user process {user}; SIGKILL will not reach it");
                }
                Ok(WorkerChild {
                    worker: self.pid,
                    user,
                    user_pidfd,
                    status: None,
                })
            }
            (r, _) => Err(unexpected(r)),
        }
    }

    fn close(self) -> Result<(), SeamError> {
        // A spawned worker closes PAM itself when the user process exits and is reaped through
        // its WorkerChild; the Close it gets here is unread.
        let _ = send(self.sock.as_raw_fd(), &Req::Close.encode(), None);
        if self.spawned {
            return Ok(());
        }
        drop(self.sock);
        let mut st = 0;
        loop {
            // SAFETY: waiting on our own child.
            let r = unsafe { libc::waitpid(self.pid, &mut st, 0) };
            if r >= 0 {
                return Ok(());
            }
            let e = io::Error::last_os_error();
            if e.kind() != io::ErrorKind::Interrupted {
                return Err(other(format!("waiting for the session worker: {e}")));
            }
        }
    }
}

fn decode_status(st: libc::c_int) -> ExitStatus {
    if libc::WIFSIGNALED(st) {
        ExitStatus::Signal(libc::WTERMSIG(st))
    } else {
        ExitStatus::Code(libc::WEXITSTATUS(st))
    }
}

impl Child for WorkerChild {
    fn pid(&self) -> i32 {
        self.worker
    }

    fn try_wait(&mut self) -> Result<Option<ExitStatus>, SeamError> {
        if self.status.is_some() {
            return Ok(self.status);
        }
        let mut st = 0;
        // SAFETY: non-blocking wait on our own child.
        let r = unsafe { libc::waitpid(self.worker, &mut st, libc::WNOHANG) };
        if r < 0 {
            return Err(other(io::Error::last_os_error()));
        }
        if r == 0 {
            return Ok(None);
        }
        self.status = Some(decode_status(st));
        Ok(self.status)
    }

    fn signal(&mut self, sig: i32) -> Result<(), SeamError> {
        if self.status.is_some() {
            return Ok(());
        }
        // SIGKILL cannot be forwarded: it goes to the user process through its pidfd, so it
        // can only hit that process (ESRCH once reaped), and the worker still reaps it and closes
        // PAM. Without a pidfd there is no safe target: the daemon never signals a bare user pid
        // and never SIGKILLs the worker (PAM would stay open), so the SIGKILL is dropped with an
        // error log and the worker's own SIGTERM forwarding is all that stops the session.
        // Every other signal goes to the worker, which forwards it.
        let rc = match signal_target(sig) {
            SignalTarget::UserPidfd => {
                let Some(fd) = &self.user_pidfd else {
                    tracing::error!("no pidfd for user process {}; SIGKILL not sent", self.user);
                    return Ok(());
                };
                // SAFETY: pidfd_send_signal(2) on our own pidfd with a NULL siginfo.
                unsafe {
                    libc::syscall(
                        libc::SYS_pidfd_send_signal,
                        fd.as_raw_fd(),
                        sig,
                        std::ptr::null::<libc::siginfo_t>(),
                        0u32,
                    )
                }
            }
            // SAFETY: plain kill(2) on the worker, our own child.
            SignalTarget::Session => i64::from(unsafe { libc::kill(self.worker, sig) }),
        };
        if rc < 0 {
            let e = io::Error::last_os_error();
            if e.raw_os_error() != Some(libc::ESRCH) {
                return Err(other(e));
            }
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------------------------
// Worker side.

static USER_PID: AtomicI32 = AtomicI32::new(0);

extern "C" fn forward(sig: libc::c_int) {
    let pid = USER_PID.load(Ordering::Relaxed);
    if pid > 0 {
        // SAFETY: kill(2) is async-signal-safe.
        unsafe { libc::kill(pid, sig) };
    }
}

fn set_handler(sig: libc::c_int, h: libc::sighandler_t) {
    // SAFETY: a zeroed sigaction with an empty mask and a plain handler is valid.
    unsafe {
        let mut sa: libc::sigaction = std::mem::zeroed();
        sa.sa_sigaction = h;
        libc::sigemptyset(&mut sa.sa_mask);
        libc::sigaction(sig, &sa, std::ptr::null_mut());
    }
}

fn close_from(lo: u32, hi: u32) {
    if lo <= hi {
        // SAFETY: close_range(2) only closes fds.
        unsafe { libc::syscall(libc::SYS_close_range, lo, hi, 0) };
    }
}

fn reply(sock: RawFd, r: &Rep) -> bool {
    send(sock, &r.encode(), None).is_ok()
}

/// Looks up an account's passwd entry.
pub fn lookup_account(name: &str) -> io::Result<Account> {
    let c = CString::new(name).map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
    // SAFETY: zeroed passwd is a valid out-param.
    let mut pw: libc::passwd = unsafe { std::mem::zeroed() };
    let mut buf = vec![0 as libc::c_char; 16 * 1024];
    let mut out = std::ptr::null_mut();
    // SAFETY: all pointers reference live locals sized as passed.
    let rc =
        unsafe { libc::getpwnam_r(c.as_ptr(), &mut pw, buf.as_mut_ptr(), buf.len(), &mut out) };
    if rc != 0 {
        return Err(io::Error::from_raw_os_error(rc));
    }
    if out.is_null() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("no user {name:?}"),
        ));
    }
    let s = |p: *const libc::c_char| {
        if p.is_null() {
            String::new()
        } else {
            // SAFETY: getpwnam_r strings are NUL-terminated inside buf.
            unsafe { std::ffi::CStr::from_ptr(p) }
                .to_string_lossy()
                .into_owned()
        }
    };
    let shell = s(pw.pw_shell);
    Ok(Account {
        name: s(pw.pw_name),
        uid: pw.pw_uid,
        gid: pw.pw_gid,
        home: s(pw.pw_dir),
        shell: if shell.is_empty() {
            "/bin/sh".into()
        } else {
            shell
        },
    })
}

/// The worker process body; returns its exit code.
fn worker_main(sock: RawFd, class: Class, user: &str) -> i32 {
    // The daemon's handlers write to its signal pipe, which this process must not touch.
    for sig in [libc::SIGTERM, libc::SIGINT, libc::SIGCHLD, libc::SIGHUP] {
        set_handler(sig, libc::SIG_DFL);
    }
    // Keep stdio (the journal) and the socket; drop every other daemon fd.
    if sock > 3 {
        close_from(3, sock as u32 - 1);
    }
    close_from(sock as u32 + 1, u32::MAX);

    // Wait for the first Step before starting PAM.
    match recv(sock) {
        Ok(Some((b, _))) if matches!(Req::decode(&b), Ok(Req::Step(_))) => {}
        _ => return 1,
    }
    let prompter: crate::pam::Prompter = Box::new(move |kind, text| {
        if !reply(sock, &Rep::Prompt(kind, text)) {
            return Err(());
        }
        match recv(sock) {
            Ok(Some((b, _))) => match Req::decode(&b) {
                Ok(Req::Step(a)) => Ok(a),
                _ => Err(()),
            },
            _ => Err(()),
        }
    });
    let mut pam = match Pam::start(class.service(), user, prompter) {
        Ok(p) => p,
        Err(e) => {
            reply(sock, &Rep::Error(e.message));
            return wait_close(sock);
        }
    };
    let auth = (|| {
        if class == Class::User {
            pam.authenticate()?;
        }
        pam.acct_mgmt()?;
        pam.user()
    })();
    let account = match auth {
        Ok(a) => a,
        Err(e) => {
            tracing::info!("PAM authentication for {user:?} failed: {}", e.message);
            reply(sock, &Rep::AuthError(e.message));
            drop(pam);
            return wait_close(sock);
        }
    };
    if !reply(sock, &Rep::Authenticated(account.clone())) {
        return 1;
    }
    let mut opened: Option<(u32, Class)> = None;
    let mut cred = false;
    let code = loop {
        let msg = match recv(sock) {
            Ok(Some(m)) => m,
            _ => break 0,
        };
        let (req, fd) = match Req::decode(&msg.0) {
            Ok(r) => (r, msg.1),
            Err(e) => {
                reply(sock, &Rep::Error(e));
                continue;
            }
        };
        match req {
            Req::Close => break 0,
            Req::Step(_) => {
                reply(sock, &Rep::Error("already authenticated".into()));
            }
            Req::Open { vt, class, env } => {
                if opened.is_some() {
                    reply(sock, &Rep::Error("session already open".into()));
                    continue;
                }
                let r = (|| {
                    pam.set_tty(&format!("tty{vt}"))?;
                    for kv in session_pam_env(vt, class, &env) {
                        pam.putenv(&kv)?;
                    }
                    pam.setcred(PAM_ESTABLISH_CRED)?;
                    cred = true;
                    pam.open_session()
                })();
                match r {
                    Ok(()) => {
                        opened = Some((vt, class));
                        reply(sock, &Rep::Ok);
                    }
                    Err(e) => {
                        reply(
                            sock,
                            &Rep::Error(format!("opening the session: {}", e.message)),
                        );
                    }
                }
            }
            Req::Spawn {
                account: acct_name,
                cmd,
                env,
                vt,
                target_fd,
            } => {
                let Some((_, class)) = opened else {
                    reply(sock, &Rep::Error("session not open".into()));
                    continue;
                };
                if acct_name != account {
                    reply(
                        sock,
                        &Rep::Error("spawn account differs from PAM_USER".into()),
                    );
                    continue;
                }
                let extra = match (fd, target_fd) {
                    (Some(f), Some(t)) if t >= 3 => Some((f, t)),
                    (None, None) => None,
                    _ => {
                        reply(sock, &Rep::Error("bad extra fd".into()));
                        continue;
                    }
                };
                let pam_env = pam.envlist();
                match spawn_user(&account, &cmd, &env, vt, class, &pam_env, extra) {
                    Ok(pid) => {
                        // Opened in this (parent) process after the fork, so the user process
                        // never holds it (pidfds are always close-on-exec). This process is
                        // the only one that reaps `pid`, and only in `wait_user` below, so the
                        // pid cannot be recycled before this open.
                        let pidfd = open_pidfd(pid);
                        USER_PID.store(pid, Ordering::Relaxed);
                        for sig in [libc::SIGTERM, libc::SIGINT, libc::SIGHUP] {
                            set_handler(sig, forward as *const () as libc::sighandler_t);
                        }
                        let _ = send(
                            sock,
                            &Rep::Spawned(pid).encode(),
                            pidfd.as_ref().map(AsRawFd::as_raw_fd),
                        );
                        // The daemon holds its own copy now.
                        drop(pidfd);
                        break wait_user(pid);
                    }
                    Err(e) => {
                        reply(sock, &Rep::Error(format!("spawn: {e}")));
                    }
                }
            }
        }
    };
    if opened.is_some()
        && let Err(e) = pam.close_session()
    {
        tracing::warn!("pam_close_session: {}", e.message);
    }
    if cred && let Err(e) = pam.setcred(PAM_DELETE_CRED) {
        tracing::warn!("pam_setcred(DELETE_CRED): {}", e.message);
    }
    drop(pam);
    code
}

/// After a failure: wait for the daemon's Close (or EOF), then exit.
fn wait_close(sock: RawFd) -> i32 {
    loop {
        match recv(sock) {
            Ok(Some((b, _))) => match Req::decode(&b) {
                Ok(Req::Close) => return 0,
                _ => {
                    reply(sock, &Rep::Error("conversation ended".into()));
                }
            },
            _ => return 0,
        }
    }
}

/// `pidfd_open(pid, 0)`; `None` (logged) on failure.
fn open_pidfd(pid: i32) -> Option<OwnedFd> {
    // SAFETY: pidfd_open(2) only creates an fd.
    let r = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0u32) };
    if r < 0 {
        tracing::warn!("pidfd_open({pid}): {}", io::Error::last_os_error());
        return None;
    }
    // SAFETY: a fresh fd we own.
    Some(unsafe { OwnedFd::from_raw_fd(r as RawFd) })
}

/// Reaps the user process; returns the worker's exit code mirroring its status (a signal is
/// reported as 128 + signal number).
fn wait_user(pid: i32) -> i32 {
    let mut st = 0;
    loop {
        // SAFETY: blocking wait on our own child.
        let r = unsafe { libc::waitpid(pid, &mut st, 0) };
        if r == pid {
            break;
        }
        if r < 0 && io::Error::last_os_error().kind() != io::ErrorKind::Interrupted {
            return 1;
        }
    }
    USER_PID.store(0, Ordering::Relaxed);
    match decode_status(st) {
        ExitStatus::Code(c) => c,
        ExitStatus::Signal(s) => 128 + s,
    }
}

fn cstrs(v: &[String]) -> io::Result<Vec<CString>> {
    v.iter()
        .map(|s| {
            CString::new(s.as_str()).map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))
        })
        .collect()
}

fn ptrs(v: &[CString]) -> Vec<*const libc::c_char> {
    v.iter()
        .map(|c| c.as_ptr())
        .chain(std::iter::once(std::ptr::null()))
        .collect()
}

/// Forks the user process. Everything is prepared before the fork; the child only makes
/// syscalls and exits 1 without exec on any failure.
#[allow(clippy::too_many_arguments)]
fn spawn_user(
    account: &str,
    cmd: &[String],
    extra_env: &[String],
    vt: u32,
    class: Class,
    pam_env: &[String],
    extra: Option<(OwnedFd, RawFd)>,
) -> io::Result<i32> {
    if cmd.is_empty() {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "empty command"));
    }
    let acct = lookup_account(account)?;
    let env = cstrs(&build_env(pam_env, &acct, vt, class, extra_env))?;
    let etc_shells = std::fs::read_to_string("/etc/shells").unwrap_or_default();
    let run_shell = exec_shell(&acct.shell, &etc_shells);
    let argv = cstrs(&login_argv(run_shell, cmd))?;
    let envp = ptrs(&env);
    let argvp = ptrs(&argv);
    let shell = CString::new(run_shell)?;
    let tty = CString::new(format!("/dev/tty{vt}"))?;
    let home = CString::new(acct.home.as_str())?;
    let name = CString::new(acct.name.as_str())?;
    let root = c"/";
    let ranges = close_ranges(extra.as_ref().map(|(_, t)| *t));
    let (src, target) = match &extra {
        Some((f, t)) => (f.as_raw_fd(), *t),
        None => (-1, -1),
    };
    // SAFETY: the worker is single-threaded; the child below calls only async-signal-safe
    // functions on memory prepared above, and leaves through execve or _exit.
    let pid = unsafe { libc::fork() };
    if pid < 0 {
        return Err(io::Error::last_os_error());
    }
    if pid == 0 {
        // SAFETY: see above.
        unsafe {
            let fail = || -> ! { libc::_exit(1) };
            let mut set: libc::sigset_t = std::mem::zeroed();
            libc::sigemptyset(&mut set);
            libc::sigprocmask(libc::SIG_SETMASK, &set, std::ptr::null_mut());
            for sig in [
                libc::SIGPIPE,
                libc::SIGTERM,
                libc::SIGINT,
                libc::SIGHUP,
                libc::SIGCHLD,
            ] {
                libc::signal(sig, libc::SIG_DFL);
            }
            if libc::setsid() < 0 {
                fail();
            }
            // Move the extra fd out of the way of stdio and its target first.
            let hi = if src >= 0 {
                let h = libc::fcntl(src, libc::F_DUPFD_CLOEXEC, target.max(64) + 1);
                if h < 0 {
                    fail();
                }
                h
            } else {
                -1
            };
            let t = libc::open(tty.as_ptr(), libc::O_RDWR | libc::O_CLOEXEC);
            if t < 0 || libc::ioctl(t, libc::TIOCSCTTY, 1) < 0 {
                fail();
            }
            for n in 0..3 {
                if libc::dup2(t, n) < 0 {
                    fail();
                }
            }
            if hi >= 0 && libc::dup2(hi, target) < 0 {
                fail();
            }
            for (lo, up) in &ranges {
                libc::syscall(libc::SYS_close_range, *lo, *up, 0);
            }
            if libc::initgroups(name.as_ptr(), acct.gid as _) < 0
                || libc::setgid(acct.gid) < 0
                || libc::setuid(acct.uid) < 0
            {
                fail();
            }
            if acct.uid != 0 && (libc::setuid(0) == 0 || libc::geteuid() == 0) {
                fail();
            }
            if libc::chdir(home.as_ptr()) < 0 && libc::chdir(root.as_ptr()) < 0 {
                fail();
            }
            libc::execve(shell.as_ptr(), argvp.as_ptr(), envp.as_ptr());
            fail();
        }
    }
    // The worker's copy of the extra fd goes now; the user process holds the only one.
    drop(extra);
    Ok(pid)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| (*x).to_owned()).collect()
    }

    fn acct() -> Account {
        Account {
            name: "alice".into(),
            uid: 1000,
            gid: 1000,
            home: "/home/alice".into(),
            shell: "/bin/bash".into(),
        }
    }

    #[test]
    fn requests_round_trip() {
        for r in [
            Req::Step(None),
            Req::Step(Some("pass word".into())),
            Req::Open {
                vt: 8,
                class: Class::User,
                env: s(&["A=1"]),
            },
            Req::Spawn {
                account: "alice".into(),
                cmd: s(&["cde", "a b"]),
                env: s(&["CDE_LOGIN_READY_FD=3"]),
                vt: 8,
                target_fd: Some(3),
            },
            Req::Spawn {
                account: "g".into(),
                cmd: s(&["x"]),
                env: vec![],
                vt: 7,
                target_fd: None,
            },
            Req::Close,
        ] {
            assert_eq!(Req::decode(&r.encode()).unwrap(), r);
        }
        assert!(Req::decode(&[9]).is_err());
        assert!(Req::decode(&[0, 1, 5, 0]).is_err());
    }

    #[test]
    fn replies_round_trip() {
        for r in [
            Rep::Prompt(AuthMessageType::Secret, "Password: ".into()),
            Rep::Prompt(AuthMessageType::Info, "hi".into()),
            Rep::Authenticated("alice".into()),
            Rep::AuthError("Authentication failure".into()),
            Rep::Error("x".into()),
            Rep::Ok,
            Rep::Spawned(4242),
        ] {
            assert_eq!(Rep::decode(&r.encode()).unwrap(), r);
        }
        let mut b = Rep::Ok.encode();
        b.push(0);
        assert!(Rep::decode(&b).is_err());
    }

    #[test]
    fn env_is_built_from_scratch() {
        let env = build_env(&[], &acct(), 8, Class::User, &[]);
        assert!(
            env.iter()
                .all(|e| !e.starts_with("CARGO") && !e.starts_with("RUST"))
        );
        for want in [
            "HOME=/home/alice",
            "USER=alice",
            "LOGNAME=alice",
            "SHELL=/bin/bash",
            "PATH=/usr/local/bin:/usr/bin:/bin",
            "XDG_SESSION_TYPE=wayland",
            "XDG_SESSION_CLASS=user",
            "XDG_SEAT=seat0",
            "XDG_VTNR=8",
        ] {
            assert!(env.contains(&want.to_owned()), "{want}");
        }
        assert_eq!(env.len(), 9);
    }

    #[test]
    fn env_merges_pam_then_base_then_extra() {
        let pam = s(&[
            "XDG_RUNTIME_DIR=/run/user/1000",
            "HOME=/bogus",
            "XDG_SESSION_ID=3",
        ]);
        let extra = s(&[
            "CDE_LOGIN_READY_FD=3",
            "XDG_SEAT=seat0",
            "GREETD_SOCK=/run/x",
        ]);
        let env = build_env(&pam, &acct(), 7, Class::Greeter, &extra);
        assert!(env.contains(&"XDG_RUNTIME_DIR=/run/user/1000".to_owned()));
        assert!(env.contains(&"XDG_SESSION_ID=3".to_owned()));
        assert!(env.contains(&"HOME=/home/alice".to_owned()));
        assert!(!env.contains(&"HOME=/bogus".to_owned()));
        assert!(env.contains(&"XDG_SESSION_CLASS=greeter".to_owned()));
        assert!(env.contains(&"CDE_LOGIN_READY_FD=3".to_owned()));
        assert_eq!(env.iter().filter(|e| e.starts_with("XDG_SEAT=")).count(), 1);
    }

    #[test]
    fn pam_env_before_open() {
        let v = session_pam_env(8, Class::User, &s(&["GREETD_SOCK=/s"]));
        assert_eq!(
            v,
            s(&[
                "XDG_SESSION_TYPE=wayland",
                "XDG_SESSION_CLASS=user",
                "XDG_SEAT=seat0",
                "XDG_VTNR=8",
                "GREETD_SOCK=/s"
            ])
        );
    }

    #[test]
    fn exec_shell_falls_back_for_unlisted_shells() {
        let shells = "# /etc/shells: valid login shells\n/bin/sh\n/bin/bash\n/usr/bin/dash\n";
        assert_eq!(exec_shell("/bin/bash", shells), "/bin/bash");
        assert_eq!(exec_shell("/usr/sbin/nologin", shells), "/bin/sh");
        assert_eq!(exec_shell("/bin/false", shells), "/bin/sh");
        assert_eq!(
            exec_shell("# /etc/shells: valid login shells", shells),
            "/bin/sh"
        );
        assert_eq!(exec_shell("/bin/bash", ""), "/bin/sh");
    }

    #[test]
    fn login_argv_keeps_words_intact() {
        let cmd = s(&[
            "/opt/cde/bin/crucible",
            "greeter",
            "a b",
            "it's \"q\"",
            "$HOME",
        ]);
        let argv = login_argv("/bin/bash", &cmd);
        assert_eq!(
            &argv[..5],
            &s(&["/bin/bash", "-l", "-c", "exec \"$@\"", "cde-login-session"])[..]
        );
        assert_eq!(&argv[5..], &cmd[..]);
    }

    #[test]
    fn login_argv_runs_through_a_real_shell() {
        let cmd = s(&["printf", "%s|", "a b", "it's \"q\"", "$HOME"]);
        let argv = login_argv("/bin/sh", &cmd);
        // `-l` is dropped here so the test does not depend on the host's profile.
        let out = std::process::Command::new(&argv[0])
            .args(&argv[2..])
            .env_clear()
            .output()
            .unwrap();
        assert_eq!(
            String::from_utf8(out.stdout).unwrap(),
            "a b|it's \"q\"|$HOME|"
        );
    }

    #[test]
    fn fd_plan_keeps_stdio_and_target_only() {
        let survives = |t: Option<RawFd>, fd: u32| {
            fd < 3
                || !close_ranges(t)
                    .iter()
                    .any(|(lo, hi)| (*lo..=*hi).contains(&fd))
        };
        for fd in 0..200 {
            assert_eq!(survives(Some(3), fd), fd <= 3, "fd {fd}");
            assert_eq!(survives(Some(5), fd), fd < 3 || fd == 5, "fd {fd}");
            assert_eq!(survives(None, fd), fd < 3, "fd {fd}");
        }
    }
}
