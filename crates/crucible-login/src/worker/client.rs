//! The daemon side of a worker: [`PamBackend`], [`PamHandle`] and [`WorkerChild`].
#![allow(unsafe_code)]

use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};

use crucible_greetd::Secret;

use super::process::worker_main;
use super::proto::{Rep, Req, recv, send};
use crate::seam::{
    AuthStep, Backend, Child, Class, ExitStatus, OpenSpec, SeamError, SessionHandle, SignalTarget,
    SpawnSpec, signal_target,
};
use crate::sys::{cvt, retry_eintr};

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
        // SAFETY: waiting on our own child.
        retry_eintr(|| cvt(unsafe { libc::waitpid(self.pid, &mut st, 0) }))
            .map(|_| ())
            .map_err(|e| other(format!("waiting for the session worker: {e}")))
    }
}

pub(super) fn decode_status(st: libc::c_int) -> ExitStatus {
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
