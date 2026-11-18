//! The worker process body, in phases: [`authenticate`] (PAM conversation + account check),
//! then [`serve`] (`Open` in [`serve_open`], `Spawn` in [`serve_spawn`], which reaps the user
//! process), then the one [`teardown`] keyed on the [`Phase`] reached. Every received request
//! buffer is wiped after decoding (it may carry a `Step` answer).
#![allow(unsafe_code)]

use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::sync::atomic::{AtomicI32, Ordering};

use crucible_greetd::wipe;

use super::client::decode_status;
use super::proto::{Rep, Req, recv, send};
use super::spawn::{close_all, close_ranges, session_pam_env, spawn_user};
use crate::pam::{PAM_DELETE_CRED, PAM_ESTABLISH_CRED, Pam};
use crate::seam::{Class, ExitStatus};
use crate::sys::{cvt, retry_eintr, set_handler};

static USER_PID: AtomicI32 = AtomicI32::new(0);

extern "C" fn forward(sig: libc::c_int) {
    let pid = USER_PID.load(Ordering::Relaxed);
    if pid > 0 {
        // SAFETY: kill(2) is async-signal-safe.
        unsafe { libc::kill(pid, sig) };
    }
}

fn reply(sock: RawFd, r: &Rep) -> bool {
    send(sock, &r.encode(), None).is_ok()
}

/// Receives and decodes one request, wiping the received bytes (they may hold a `Step` answer).
/// `None`: the daemon is gone (EOF or a socket error); `Some(Err)`: the request did not decode.
fn recv_req(sock: RawFd) -> Option<Result<(Req, Option<OwnedFd>), String>> {
    let (mut b, fd) = recv(sock).ok()??;
    let req = Req::decode(&b);
    wipe(&mut b);
    Some(req.map(|r| (r, fd)))
}

/// How far the PAM handle got after authentication; [`teardown`] undoes exactly that.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    /// `pam_authenticate` + `pam_acct_mgmt` passed.
    Authenticated,
    /// `pam_setcred(ESTABLISH_CRED)` passed (`pam_open_session` not, or not yet).
    CredEstablished,
    /// `pam_open_session` passed on this VT.
    Opened { vt: u32 },
}

/// What teardown runs for a phase, before `pam_end`.
#[derive(Debug, PartialEq, Eq)]
struct Teardown {
    close_session: bool,
    delete_cred: bool,
}

fn teardown_for(phase: Phase) -> Teardown {
    match phase {
        Phase::Authenticated => Teardown {
            close_session: false,
            delete_cred: false,
        },
        // Credentials established before a failed pam_open_session are still deleted.
        Phase::CredEstablished => Teardown {
            close_session: false,
            delete_cred: true,
        },
        Phase::Opened { .. } => Teardown {
            close_session: true,
            delete_cred: true,
        },
    }
}

/// The one teardown: `pam_close_session` if opened, `pam_setcred(DELETE_CRED)` if credentials
/// were established, then `pam_end` (drop).
fn teardown(mut pam: Pam, phase: Phase) {
    let t = teardown_for(phase);
    if t.close_session
        && let Err(e) = pam.close_session()
    {
        tracing::warn!("pam_close_session: {}", e.message);
    }
    if t.delete_cred
        && let Err(e) = pam.setcred(PAM_DELETE_CRED)
    {
        tracing::warn!("pam_setcred(DELETE_CRED): {}", e.message);
    }
    drop(pam);
}

/// The worker process body; returns its exit code.
pub(super) fn worker_main(sock: RawFd, class: Class, user: &str) -> i32 {
    // The daemon's handlers write to its signal pipe, which this process must not touch.
    for sig in [libc::SIGTERM, libc::SIGINT, libc::SIGCHLD, libc::SIGHUP] {
        if let Err(e) = set_handler(sig, libc::SIG_DFL, 0) {
            tracing::warn!("resetting signal {sig}: {e}");
        }
    }
    // Keep stdio (the journal) and the socket; drop every other daemon fd.
    close_all(&close_ranges(Some(sock)));

    // Wait for the first Step before starting PAM.
    if !matches!(recv_req(sock), Some(Ok((Req::Step(_), _)))) {
        return 1;
    }
    let Some((mut pam, account)) = authenticate(sock, class, user) else {
        return wait_close(sock);
    };
    if !reply(sock, &Rep::Authenticated(account.clone())) {
        return 1;
    }
    let mut phase = Phase::Authenticated;
    let code = serve(sock, &mut pam, class, &account, &mut phase);
    teardown(pam, phase);
    code
}

/// Serves requests after authentication until `Close`, EOF or the user process's exit (its
/// status is the returned code); `phase` records how far PAM got.
fn serve(sock: RawFd, pam: &mut Pam, class: Class, account: &str, phase: &mut Phase) -> i32 {
    loop {
        let (req, fd) = match recv_req(sock) {
            None => return 0,
            Some(Err(e)) => {
                reply(sock, &Rep::Error(e));
                continue;
            }
            Some(Ok(m)) => m,
        };
        match req {
            Req::Close => return 0,
            Req::Step(_) => {
                reply(sock, &Rep::Error("already authenticated".into()));
            }
            Req::Open { vt, env } => {
                if matches!(phase, Phase::Opened { .. }) {
                    reply(sock, &Rep::Error("session already open".into()));
                } else {
                    serve_open(sock, pam, class, vt, &env, phase);
                }
            }
            Req::Spawn {
                cmd,
                env,
                target_fd,
            } => {
                let Phase::Opened { vt } = *phase else {
                    reply(sock, &Rep::Error("session not open".into()));
                    continue;
                };
                let extra = match (fd, target_fd) {
                    (Some(f), Some(t)) if t >= 3 => Some((f, t)),
                    (None, None) => None,
                    _ => {
                        reply(sock, &Rep::Error("bad extra fd".into()));
                        continue;
                    }
                };
                let user = UserSpawn {
                    account,
                    class,
                    vt,
                    cmd: &cmd,
                    env: &env,
                };
                if let Some(code) = serve_spawn(sock, pam, &user, extra) {
                    return code;
                }
            }
        }
    }
}

/// `Open`: `PAM_TTY`, the XDG entries into the PAM environment, `pam_setcred(ESTABLISH_CRED)`,
/// `pam_open_session`; advances `phase` as each step passes.
fn serve_open(
    sock: RawFd,
    pam: &mut Pam,
    class: Class,
    vt: u32,
    env: &[String],
    phase: &mut Phase,
) {
    let r = (|| {
        pam.set_tty(&format!("tty{vt}"))?;
        for kv in session_pam_env(vt, class, env) {
            pam.putenv(&kv)?;
        }
        pam.setcred(PAM_ESTABLISH_CRED)?;
        *phase = Phase::CredEstablished;
        pam.open_session()?;
        *phase = Phase::Opened { vt };
        Ok::<(), crate::pam::PamError>(())
    })();
    match r {
        Ok(()) => reply(sock, &Rep::Ok),
        Err(e) => reply(
            sock,
            &Rep::Error(format!("opening the session: {}", e.message)),
        ),
    };
}

/// What the user process is: the account from authentication, the class from the fork, the VT
/// from `Open`, and the `Spawn` request's command and env.
struct UserSpawn<'a> {
    account: &'a str,
    class: Class,
    vt: u32,
    cmd: &'a [String],
    env: &'a [String],
}

/// `Spawn`: starts the user process, sends `Spawned` (+ its pidfd) and waits for it;
/// `Some(code)` once it was reaped, `None` after a replied failure.
fn serve_spawn(
    sock: RawFd,
    pam: &mut Pam,
    u: &UserSpawn<'_>,
    extra: Option<(OwnedFd, RawFd)>,
) -> Option<i32> {
    let pam_env = pam.envlist();
    let pid = match spawn_user(u.account, u.cmd, u.env, u.vt, u.class, &pam_env, extra) {
        Ok(pid) => pid,
        Err(e) => {
            reply(sock, &Rep::Error(format!("spawn: {e}")));
            return None;
        }
    };
    // Opened in this (parent) process after the fork, so the user process never holds it
    // (pidfds are always close-on-exec). This process is the only one that reaps `pid`, and
    // only in `wait_user` below, so the pid cannot be recycled before this open.
    let pidfd = open_pidfd(pid);
    USER_PID.store(pid, Ordering::Relaxed);
    for sig in [libc::SIGTERM, libc::SIGINT, libc::SIGHUP] {
        // Flags 0 (no SA_RESTART), as before: the worker's waits see EINTR.
        // The server installs its handlers with SA_RESTART instead.
        if let Err(e) = set_handler(sig, forward as *const () as libc::sighandler_t, 0) {
            tracing::warn!("forwarding signal {sig}: {e}");
        }
    }
    let _ = send(
        sock,
        &Rep::Spawned(pid).encode(),
        pidfd.as_ref().map(AsRawFd::as_raw_fd),
    );
    // The daemon holds its own copy now.
    drop(pidfd);
    Some(wait_user(pid))
}

/// Starts PAM and runs the conversation and the account check; `None` once the failure has been
/// replied. The account is `PAM_USER` afterwards.
fn authenticate(sock: RawFd, class: Class, user: &str) -> Option<(Pam, String)> {
    let prompter: crate::pam::Prompter = Box::new(move |kind, text| {
        if !reply(sock, &Rep::Prompt(kind, text)) {
            return Err(());
        }
        match recv_req(sock) {
            Some(Ok((Req::Step(a), _))) => Ok(a),
            _ => Err(()),
        }
    });
    let mut pam = match Pam::start(class.service(), user, prompter) {
        Ok(p) => p,
        Err(e) => {
            reply(sock, &Rep::Error(e.message));
            return None;
        }
    };
    let auth = (|| {
        if class == Class::User {
            pam.authenticate()?;
        }
        pam.acct_mgmt()?;
        pam.user()
    })();
    match auth {
        Ok(a) => Some((pam, a)),
        Err(e) => {
            tracing::info!("PAM authentication for {user:?} failed: {}", e.message);
            reply(sock, &Rep::AuthError(e.message));
            drop(pam);
            None
        }
    }
}
/// After a failure: wait for the daemon's Close (or EOF), then exit.
fn wait_close(sock: RawFd) -> i32 {
    loop {
        match recv_req(sock) {
            Some(Ok((Req::Close, _))) | None => return 0,
            Some(_) => {
                reply(sock, &Rep::Error("conversation ended".into()));
            }
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
    // SAFETY: blocking wait on our own child.
    if retry_eintr(|| cvt(unsafe { libc::waitpid(pid, &mut st, 0) })).is_err() {
        return 1;
    }
    USER_PID.store(0, Ordering::Relaxed);
    match decode_status(st) {
        ExitStatus::Code(c) => c,
        ExitStatus::Signal(s) => 128 + s,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn teardown_undoes_exactly_the_phase_reached() {
        let t = |close_session, delete_cred| Teardown {
            close_session,
            delete_cred,
        };
        assert_eq!(teardown_for(Phase::Authenticated), t(false, false));
        // Credentials from before a failed pam_open_session are still deleted.
        assert_eq!(teardown_for(Phase::CredEstablished), t(false, true));
        assert_eq!(teardown_for(Phase::Opened { vt: 8 }), t(true, true));
    }
}
