//! The PAM backend: one root worker process per session handle.
//!
//! `pam_systemd` registers the logind session for the process that calls `pam_open_session`
//! (and moves it into the session scope), so PAM never runs in the daemon. [`PamBackend::begin`]
//! forks a worker that owns the PAM handle for the session's whole life and talks to the daemon
//! over a `SOCK_SEQPACKET` socketpair, one `serde_json` document per datagram ([`proto`]):
//!
//! - `Step(answer)` → `Prompt` | `Authenticated` | `AuthError` | `Error` (the first `Step` starts
//!   PAM; PAM's push-driven conversation blocks on the next `Step` for each message);
//! - `Open { vt, env }` → `Ok` | `Error`; `Spawn { cmd, env, target_fd }` (+ the extra fd over
//!   `SCM_RIGHTS`) → `Spawned(pid)` (+ a pidfd for the user process over `SCM_RIGHTS`, absent if
//!   `pidfd_open` failed) | `Error`; `Close` → the worker closes PAM and exits.
//!
//! The worker knows its class from the fork, takes the account from PAM (`PAM_USER` after
//! `pam_acct_mgmt`) and the VT from `Open`; that one VT is `PAM_TTY`, the user process's
//! `/dev/tty<n>` and `XDG_VTNR`, so no request can contradict them. A `Spawn` that fails is
//! followed by the daemon side's `Close` ([`PamHandle`]'s spawn consumes the handle).
//!
//! After `Spawned`, the worker forwards `SIGTERM`/`SIGINT`/`SIGHUP` to the user process, reaps
//! it, runs `pam_close_session` + `pam_setcred(DELETE_CRED)` + `pam_end`, and exits with the
//! user process's status, so the daemon-side [`crate::seam::Child`] is the worker itself. SIGKILL cannot be
//! forwarded, so the daemon sends it through the user process's pidfd (never a bare pid).
//!
//! [`client`] is the daemon side, [`process`] the worker process body, and [`spawn`] the user
//! process (environment, argv, fd plan, fork + exec).

mod client;
mod process;
mod proto;
mod spawn;

pub use client::{PamBackend, PamHandle, WorkerChild};
pub use proto::{Rep, Req};
pub use spawn::{
    Account, SESSION_PATH, build_env, close_ranges, exec_shell, login_argv, session_pam_env,
};
