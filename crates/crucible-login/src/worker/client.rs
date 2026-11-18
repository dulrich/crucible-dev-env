//! The daemon side of a worker: [`PamBackend`], [`PamHandle`] and [`WorkerChild`].
#![allow(unsafe_code)]

use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};

use crucible_greetd::{Secret, wipe};

use super::process::worker_main;
use super::proto::{Rep, Req, recv_flags, send};
use crate::seam::{
    AuthStep, Backend, Child, Class, ExitStatus, OpenSpec, Reply, SeamError, SessionHandle,
    SignalTarget, SpawnSpec, signal_target,
};

/// The production backend.
#[derive(Debug, Default)]
pub struct PamBackend;

/// The daemon's end of one worker.
#[derive(Debug)]
pub struct PamHandle {
    sock: OwnedFd,
    pid: i32,
    /// The request whose reply is outstanding.
    sent: Option<Sent>,
    /// The user pid and pidfd from the `Spawned` reply, for [`SessionHandle::into_child`].
    spawned: Option<(i32, Option<OwnedFd>)>,
}

/// Which request a reply answers.
#[derive(Debug, Clone, Copy)]
enum Sent {
    Step,
    Open,
    Spawn,
}

/// The worker process, as the daemon sees the session (or a closing unspawned handle, reaped on
/// `SIGCHLD`). At spawn it takes over the handle's socket and worker pid; the socket is kept
/// (unused) until the session is dropped, and the worker closes PAM itself once it has reaped the
/// user process. A closed handle keeps only the pid: its socket is dropped right after `Close`,
/// so a worker waiting in a prompt or after a failure sees EOF and exits.
#[derive(Debug)]
pub struct WorkerChild {
    _sock: Option<OwnedFd>,
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
            sent: None,
            spawned: None,
        })
    }
}

impl PamHandle {
    /// Sends one request (with an optional fd); the encoded request is wiped after sending (it
    /// may carry a `Step` answer). The reply is collected by [`SessionHandle::take_reply`].
    fn request(&mut self, req: &Req, fd: Option<RawFd>, sent: Sent) -> Result<(), SeamError> {
        if self.sent.is_some() {
            return Err(other("a worker request is already outstanding"));
        }
        let mut data = req.encode();
        let r = send(self.sock.as_raw_fd(), &data, fd);
        wipe(&mut data);
        r.map_err(|e| other(format!("worker: {e}")))?;
        self.sent = Some(sent);
        Ok(())
    }

    /// Tells the worker to close PAM and exit; it is reaped later, without blocking.
    fn end(self) -> WorkerChild {
        let _ = send(self.sock.as_raw_fd(), &Req::Close.encode(), None);
        // EOF after `Close`: a worker that consumed `Close` inside a prompt waits for a second
        // `Close` or EOF (`wait_close`), so the socket must not outlive the close.
        drop(self.sock);
        WorkerChild {
            _sock: None,
            worker: self.pid,
            user: 0,
            user_pidfd: None,
            status: None,
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

    fn fd(&self) -> RawFd {
        self.sock.as_raw_fd()
    }

    fn send_step(&mut self, answer: Option<Secret>) -> Result<(), SeamError> {
        self.request(&Req::Step(answer), None, Sent::Step)
    }

    fn send_open(&mut self, spec: &OpenSpec) -> Result<(), SeamError> {
        let req = Req::Open {
            vt: spec.vt,
            env: spec.env.clone(),
        };
        self.request(&req, None, Sent::Open)
    }

    fn send_spawn(&mut self, spec: SpawnSpec) -> Result<(), SeamError> {
        let (fd, target) = match &spec.extra_fd {
            Some((fd, t)) => (Some(fd.as_raw_fd()), Some(*t)),
            None => (None, None),
        };
        let req = Req::Spawn {
            cmd: spec.cmd.clone(),
            env: spec.env.clone(),
            target_fd: target,
        };
        // Our copy of the extra fd is dropped with `spec`; the worker drops its own after the
        // fork.
        self.request(&req, fd, Sent::Spawn)
    }

    fn take_reply(&mut self) -> Result<Option<Reply>, SeamError> {
        let Some(sent) = self.sent else {
            return Ok(None);
        };
        let (b, rfd) = match recv_flags(self.sock.as_raw_fd(), libc::MSG_DONTWAIT) {
            Ok(Some(m)) => m,
            Ok(None) => {
                self.sent = None;
                return Err(other("session worker exited"));
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => return Ok(None),
            Err(e) => {
                self.sent = None;
                return Err(other(format!("worker: {e}")));
            }
        };
        self.sent = None;
        let rep = Rep::decode(&b).map_err(|e| other(format!("worker reply: {e}")))?;
        match (sent, rep) {
            (Sent::Step, Rep::Prompt(k, t)) => Ok(Some(Reply::Step(AuthStep::Prompt(k, t)))),
            (Sent::Step, Rep::Authenticated(account)) => {
                Ok(Some(Reply::Step(AuthStep::Authenticated { account })))
            }
            (Sent::Open, Rep::Ok) => Ok(Some(Reply::Opened)),
            (Sent::Spawn, Rep::Spawned(user)) => {
                if rfd.is_none() {
                    tracing::warn!("no pidfd for user process {user}; SIGKILL will not reach it");
                }
                self.spawned = Some((user, rfd));
                Ok(Some(Reply::Spawned))
            }
            (_, r) => Err(unexpected(r)),
        }
    }

    fn into_child(self) -> WorkerChild {
        let (user, user_pidfd) = self.spawned.unwrap_or((0, None));
        WorkerChild {
            _sock: Some(self.sock),
            worker: self.pid,
            user,
            user_pidfd,
            status: None,
        }
    }

    fn close(self) -> Result<WorkerChild, SeamError> {
        Ok(self.end())
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::worker::proto::recv;

    #[test]
    fn close_sends_close_then_eof() {
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
        assert_eq!(rc, 0);
        // SAFETY: both fds are fresh and ours.
        let (ours, peer) = unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) };
        let h = PamHandle {
            sock: ours,
            pid: 0,
            sent: None,
            spawned: None,
        };
        let w = h.close().unwrap();
        let (b, _) = recv(peer.as_raw_fd()).unwrap().expect("Close datagram");
        assert_eq!(Req::decode(&b).unwrap(), Req::Close);
        assert!(recv(peer.as_raw_fd()).unwrap().is_none(), "EOF after Close");
        drop(w);
    }
}
