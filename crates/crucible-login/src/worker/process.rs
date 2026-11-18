//! The worker process body: PAM conversation, session open, user spawn, reap and teardown.
#![allow(unsafe_code)]

use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::sync::atomic::{AtomicI32, Ordering};

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
                            // Flags 0 (no SA_RESTART), as before: the worker's waits see EINTR.
                            // The server installs its handlers with SA_RESTART instead.
                            if let Err(e) =
                                set_handler(sig, forward as *const () as libc::sighandler_t, 0)
                            {
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
