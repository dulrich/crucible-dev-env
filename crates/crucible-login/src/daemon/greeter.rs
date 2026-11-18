//! The greeter half of the daemon: starting, restarting and handing off to the greeter.

use super::*;

impl<B: Backend, V: Vt> Daemon<B, V> {
    // ---- greeter -----------------------------------------------------------------------------

    /// Starts a greeter: `begin` and the first step request; the replies drive the rest
    /// ([`Daemon::drive_greeter`]). A logout start passes the ready pipe (read end, write end):
    /// the write end goes to [`READY_FD`] with [`READY_ENV`] set. Greeters started otherwise
    /// get none.
    pub(super) fn start_greeter(&mut self, now: Instant, hand_off: Option<(OwnedFd, OwnedFd)>) {
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

    pub(super) fn greeter_failed(&mut self, now: Instant, e: &SeamError, logout: bool) {
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

    pub(super) fn drive_greeter(&mut self, now: Instant) -> bool {
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

    pub(super) fn switch_to_greeter(&mut self) {
        let vt = self.cfg.greeter_vt;
        if self.activate(vt).is_ok() {
            tracing::info!("logout: switched to VT {vt}");
        }
    }
}
