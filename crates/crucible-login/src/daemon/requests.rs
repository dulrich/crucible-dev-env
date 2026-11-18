//! The greeter's requests: cancelling and dropping, the request dispatch and the auth steps.

use super::*;

impl<B: Backend, V: Vt> Daemon<B, V> {
    // ---- greeter requests --------------------------------------------------------------------

    /// Cancels the user's conversation: an idle handle is closed now; one with a step request
    /// outstanding is closed when its late reply has been consumed.
    pub(super) fn cancel_auth(&mut self) {
        match self.auth.take() {
            Some(Auth::Waiting(h) | Auth::Cancelled(h)) => self.auth = Some(Auth::Cancelled(h)),
            Some(Auth::Pending(h) | Auth::Authenticated(h, _)) => {
                self.close_handle(h, "authentication");
            }
            None => {}
        }
    }

    /// Drops a queued start (its client gets an error).
    pub(super) fn drop_queued(&mut self, why: &str) {
        if let UserSession::Closing { queued, .. } = &mut self.session
            && let Some(q) = queued.take()
        {
            self.close_handle(q.handle, "queued session");
            self.answer(error(why));
        }
    }

    /// Serves one greeter request from `client`. Every reply, immediate or after worker
    /// replies, a ready wait or a queued start, goes to the reply slot for `client`
    /// ([`Daemon::take_reply`]). While a request is in flight ([`Daemon::busy`]) another one is
    /// refused at once with the returned error, which the server writes to that client.
    pub fn handle(&mut self, client: ClientId, req: Request, now: Instant) -> Result<(), Response> {
        if self.busy() {
            return Err(error("another request is in progress"));
        }
        self.asker = Some(Asker::Client(client));
        match req {
            Request::CreateSession { username } => self.create_session(&username),
            Request::PostAuthMessageResponse { response } => self.post_response(response),
            Request::StartSession { cmd, env } => self.start_session(cmd, env, now),
            Request::CancelSession => {
                self.cancel_auth();
                self.answer(Response::Success);
            }
        }
        self.drive(now);
        Ok(())
    }

    fn create_session(&mut self, username: &str) {
        if self.session_blocks() {
            return self.answer(error("a session is already active"));
        }
        self.cancel_auth();
        tracing::info!("create_session for {username:?}");
        match self.backend.begin(Class::User, username) {
            Ok(h) => self.send_step(h, None),
            Err(e) => self.answer(seam_reply(&e)),
        }
    }

    fn send_step(&mut self, mut h: B::Handle, answer: Option<Secret>) {
        match h.send_step(answer) {
            Ok(()) => self.auth = Some(Auth::Waiting(h)),
            Err(e) => {
                self.close_handle(h, "authentication");
                self.answer(seam_reply(&e));
            }
        }
    }

    fn post_response(&mut self, answer: Option<Secret>) {
        match self.auth.take() {
            Some(Auth::Pending(h)) => self.send_step(h, answer),
            other => {
                let msg = match other {
                    None => "no session under configuration",
                    Some(Auth::Authenticated(..)) => "session has no pending questions",
                    Some(Auth::Waiting(_) | Auth::Cancelled(_) | Auth::Pending(_)) => {
                        "a step is already in progress"
                    }
                };
                self.auth = other;
                self.answer(error(msg));
            }
        }
    }
}
