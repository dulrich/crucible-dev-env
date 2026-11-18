//! The user session half of the daemon: the session channel, `quit` and the session start.

use super::*;

impl<B: Backend, V: Vt> Daemon<B, V> {
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

    pub(super) fn start_session(&mut self, cmd: Vec<String>, env: Vec<String>, now: Instant) {
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
    pub(super) fn open_session(
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

    pub(super) fn drive_session(&mut self, now: Instant) -> bool {
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
}
