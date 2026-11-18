//! Timers, child polling and shutdown, and the reap helpers they share.

use super::*;

impl<B: Backend, V: Vt> Daemon<B, V> {
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

pub(super) fn reap<C: Child>(child: &mut C, what: &str) -> Option<ExitStatus> {
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
