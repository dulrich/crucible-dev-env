//! `crucible greeter`: the compositor in kiosk mode under greetd. It loads no workspaces,
//! autostart or user binds and saves nothing; it draws a native login panel ([`panel`]) in the
//! profile's style ([`profile`]), runs `/usr/bin/swaybg` for the background, and drives
//! `crucible-greetd`'s login machine on a worker thread (greetd replies block, e.g. the PAM fail
//! delay). Input admission is the lock's: only VT switching and `set-layout` chords run, every
//! other key goes to the panel. It never reads `$HOME` or `$XDG_CONFIG_HOME`.

pub mod panel;
pub mod profile;
pub mod publish;

/// Re-exported for the `crucible greeter` entry point (the fake greetd and the codec).
pub use crucible_greetd;

use std::os::unix::net::UnixStream;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::Ordering;
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

use crucible_config::{Config, HeaderConfig, HeaderMode, IdleConfig};
use crucible_greetd::{Event, Secret, Session, View};
use smithay::input::keyboard::Keysym;
use smithay::reexports::calloop::channel::{self, Event as ChannelEvent};
use tracing::{error, info, warn};

use crate::state::{Backend, CrucibleState};
use panel::{ELLIPSIS_STEP, PanelFrame, ellipsis, field_text, layout_label};
use profile::{GreeterProfile, ProfileBackground};

/// The swaybg binary (a system path: the greeter user has no `$PATH` worth trusting).
pub const SWAYBG: &str = "/usr/bin/swaybg";

/// What the backends need to start greeter mode.
pub struct GreeterSetup {
    /// The profile (already loaded, or the fallback).
    pub profile: GreeterProfile,
    /// The connection to greetd (`$GREETD_SOCK`, or the in-process fake).
    pub stream: UnixStream,
}

/// One key the panel understands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GreeterKey {
    /// A printable character.
    Char(char),
    /// Delete the last character.
    Backspace,
    /// Submit the field.
    Enter,
    /// Cancel back to the username.
    Escape,
}

/// Maps a key press (its keysym and the character it types, if any) to a panel key.
#[must_use]
pub fn greeter_key(keysym: Keysym, ch: Option<char>) -> Option<GreeterKey> {
    match keysym {
        Keysym::Return | Keysym::KP_Enter => Some(GreeterKey::Enter),
        Keysym::Escape => Some(GreeterKey::Escape),
        Keysym::BackSpace => Some(GreeterKey::Backspace),
        _ => ch.filter(|c| !c.is_control()).map(GreeterKey::Char),
    }
}

/// The field buffer and the busy flag (pure).
#[derive(Debug, Default)]
pub struct LoginInput {
    text: String,
    busy: bool,
}

impl LoginInput {
    /// Applies a key; returns the event to feed the login machine, if any. While busy (an
    /// exchange with greetd is in flight) every key is ignored.
    pub fn key(&mut self, key: GreeterKey) -> Option<Event> {
        if self.busy {
            return None;
        }
        match key {
            GreeterKey::Char(c) => self.text.push(c),
            GreeterKey::Backspace => {
                self.text.pop();
            }
            GreeterKey::Enter => {
                self.busy = true;
                return Some(Event::Submit(Secret::new(std::mem::take(&mut self.text))));
            }
            GreeterKey::Escape => {
                self.clear();
                self.busy = true;
                return Some(Event::Escape);
            }
        }
        None
    }

    /// The exchange finished.
    pub fn done(&mut self) {
        self.busy = false;
    }

    /// The current text.
    #[must_use]
    pub fn text(&self) -> &str {
        &self.text
    }

    /// Whether an exchange is in flight.
    #[must_use]
    pub fn busy(&self) -> bool {
        self.busy
    }

    fn clear(&mut self) {
        let mut bytes = std::mem::take(&mut self.text).into_bytes();
        bytes.fill(0);
    }
}

impl Drop for LoginInput {
    fn drop(&mut self) {
        self.clear();
    }
}

/// A finished exchange: the new view and whether the session started, or the failure.
type Reply = Result<(View, bool), String>;

/// The "Loading CDE" text (the animated ellipsis follows it).
pub const LOADING_TEXT: &str = "Loading CDE";

/// How long the worker waits for the compositor's presentation ack before it aborts the
/// exchange (a deadlock guard only, far above the gate's deadline).
pub const ACK_BACKSTOP: Duration = Duration::from_secs(2);

/// What the greetd worker tells the compositor.
enum WorkerMsg {
    /// Authentication succeeded and `start_session` is next: show "Loading CDE" and ack once it
    /// is on screen (the worker writes `start_session` only after the ack).
    Loading(mpsc::Sender<()>),
    /// A finished exchange.
    Reply(Reply),
}

/// The live greeter state on [`CrucibleState`].
pub struct Greeter {
    profile: GreeterProfile,
    view: View,
    input: LoginInput,
    events: mpsc::Sender<Event>,
    swaybg: Option<Child>,
    /// The active layout's index in the profile's `layouts` (the xkb group; 0 at start).
    layout: usize,
    /// Since when the post-auth "Loading CDE" panel shows (`None`: the login panel).
    loading: Option<Instant>,
    /// The process exit status once the loop stops: 0 after `start_session` succeeded.
    pub exit_code: i32,
}

impl std::fmt::Debug for Greeter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Greeter")
            .field("view", &self.view)
            .field("busy", &self.input.busy)
            .finish_non_exhaustive()
    }
}

impl Drop for Greeter {
    fn drop(&mut self) {
        if let Some(child) = &mut self.swaybg {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

impl Greeter {
    /// What the panel draws this frame.
    #[must_use]
    pub fn frame(&self) -> PanelFrame {
        self.frame_at(Instant::now())
    }

    /// Takes a finished exchange's view. A non-final reply (a `start_session` error reply, back
    /// to the prompt with its message) clears the loading panel; the started session keeps it
    /// (the keep-lit exit holds that frame). Returns whether loading was cleared.
    fn accept(&mut self, view: View, done: bool) -> bool {
        self.view = view;
        !done && self.loading.take().is_some()
    }

    /// Whether the post-auth "Loading CDE" panel shows.
    #[must_use]
    pub fn loading(&self) -> bool {
        self.loading.is_some()
    }

    /// [`frame`](Self::frame) at `now` (the ellipsis phase).
    fn frame_at(&self, now: Instant) -> PanelFrame {
        let clear = match &self.profile.background {
            ProfileBackground::Color(c) => c.to_rgba_f32(),
            ProfileBackground::Image(_) => [0.0, 0.0, 0.0, 1.0],
        };
        if let Some(since) = self.loading {
            // No prompt, no field (nor its frame): the hand-off panel.
            return PanelFrame {
                prompt: format!(
                    "{LOADING_TEXT}{}",
                    ellipsis(now.saturating_duration_since(since))
                ),
                field: None,
                message: None,
                layout: None,
                palette: self.profile.palette,
                alpha: self.profile.alpha,
                position: self.profile.position,
                clear,
            };
        }
        PanelFrame {
            prompt: self.view.prompt.clone(),
            field: Some(field_text(self.input.text(), self.view.secret)),
            message: if self.input.busy() {
                Some("Authenticating\u{2026}".to_string())
            } else {
                self.view.message.clone()
            },
            layout: layout_label(&self.profile.layouts, self.layout),
            palette: self.profile.palette,
            alpha: self.profile.alpha,
            position: self.profile.position,
            clear,
        }
    }
}

/// The config greeter mode runs with: the profile's layouts and `set-layout` binds over the
/// defaults, no header; everything else default (no user config is read).
#[must_use]
pub fn greeter_config(p: &GreeterProfile) -> Config {
    Config {
        keyboard: p.keyboard(),
        binds: p.binds.clone(),
        // Blank on the profile's timeout; `lock_after` stays unset: the greeter never locks.
        idle: IdleConfig {
            lock_after: None,
            blank_after: Some(p.blank_after),
        },
        header: HeaderConfig {
            mode: HeaderMode::Off,
            ..HeaderConfig::default()
        },
        ..Config::default()
    }
}

/// swaybg's arguments for the profile background.
#[must_use]
pub fn swaybg_args(bg: &ProfileBackground) -> Vec<String> {
    let mut args = vec!["-o".to_string(), "*".to_string()];
    match bg {
        ProfileBackground::Image(p) => {
            args.extend(["-i".into(), p.to_string_lossy().into_owned()]);
            args.extend(["-m".into(), "fill".into()]);
        }
        ProfileBackground::Color(c) => {
            args.extend(["-c".into(), format!("{c:x}")]);
        }
    }
    args
}

fn spawn_swaybg(bg: &ProfileBackground, socket: Option<&str>) -> Option<Child> {
    let mut cmd = Command::new(SWAYBG);
    cmd.args(swaybg_args(bg))
        .stdin(Stdio::null())
        .stdout(Stdio::null());
    if let Some(s) = socket {
        cmd.env("WAYLAND_DISPLAY", s);
    }
    match cmd.spawn() {
        Ok(child) => Some(child),
        Err(err) => {
            warn!(%err, "cannot run {SWAYBG}; the panel draws over the profile colour");
            None
        }
    }
}

/// Owns the greetd session on a thread: feeds each event and hands the reply to `send` (which
/// returns `false` once nobody listens). Just before `start_session` it sends
/// [`WorkerMsg::Loading`] and blocks until the compositor acks (the loading frame is on screen);
/// no ack within [`ACK_BACKSTOP`], or a dropped ack, aborts the exchange without writing it. The
/// thread ends after an error or the started session.
fn spawn_worker(
    mut session: Session<UnixStream>,
    send: impl Fn(WorkerMsg) -> bool + Send + Sync + 'static,
) -> mpsc::Sender<Event> {
    let send = Arc::new(send);
    let progress = Arc::clone(&send);
    session.set_before_start(Box::new(move || {
        let (ack_tx, ack_rx) = mpsc::channel::<()>();
        if !progress(WorkerMsg::Loading(ack_tx)) {
            return false;
        }
        match ack_rx.recv_timeout(ACK_BACKSTOP) {
            Ok(()) => true,
            Err(err) => {
                warn!(%err, "no presentation ack before start_session; aborting");
                false
            }
        }
    }));
    let (tx, rx) = mpsc::channel::<Event>();
    std::thread::spawn(move || {
        for event in rx {
            let reply = session
                .feed(event)
                .map(|view| (view, session.is_done()))
                .map_err(|e| e.to_string());
            let stop = reply.is_err() || reply.as_ref().is_ok_and(|(_, done)| *done);
            if !send(WorkerMsg::Reply(reply)) || stop {
                break;
            }
        }
    });
    tx
}

impl<B: Backend + 'static> CrucibleState<B> {
    /// Enters greeter mode (instead of `apply_config` + `start_persistence` + the session):
    /// binds, keyboard and font from the profile, the IPC socket (for nested `capture` checks;
    /// it lives in the greeter user's runtime dir), swaybg and the greetd worker.
    pub fn start_greeter(&mut self, setup: GreeterSetup) {
        let GreeterSetup { profile, stream } = setup;
        let config = greeter_config(&profile);
        let (bindings, err) = crate::config::plan_bindings(&self.bindings, &config);
        if let Some(err) = err {
            warn!(%err, "invalid set-layout binds in the greeter profile; using the defaults");
        }
        self.bindings = bindings;
        self.header_text = crate::text::HeaderFonts::new(crate::text::TextRenderer::load(
            None,
            crucible_config::DEFAULT_HEADER_FONT,
        ));
        self.config = config;
        self.start_ipc();
        // Greeter mode skips `start_persistence`, which is what arms the idle timer; arm only it
        // (blank on the profile's `blank-after`; `greeter_config` leaves the lock off).
        self.start_idle_timer();
        let swaybg = spawn_swaybg(&profile.background, self.socket_name.as_deref());
        let session = Session::new(stream, &profile.session);
        let view = session.view();
        let (reply_tx, reply_rx) = channel::channel::<WorkerMsg>();
        if let Err(err) = self
            .handle
            .insert_source(reply_rx, |event, _, data| match event {
                ChannelEvent::Msg(WorkerMsg::Reply(reply)) => data.on_greeter_reply(reply),
                ChannelEvent::Msg(WorkerMsg::Loading(ack)) => data.on_greeter_loading(ack),
                ChannelEvent::Closed => {}
            })
        {
            error!(%err, "cannot watch the greetd worker");
        }
        let events = spawn_worker(session, move |r| reply_tx.send(r).is_ok());
        info!(style = ?profile.style, "greeter started");
        self.greeter = Some(Box::new(Greeter {
            profile,
            view,
            input: LoginInput::default(),
            events,
            swaybg,
            layout: 0,
            loading: None,
            exit_code: 1,
        }));
    }

    fn on_greeter_reply(&mut self, reply: Reply) {
        let Some(g) = self.greeter.as_mut() else {
            return;
        };
        g.input.done();
        match reply {
            Ok((view, done)) => {
                let was_loading = g.accept(view, done);
                if done {
                    // The "Loading CDE" frame stays: the keep-lit exit holds the last frame.
                    info!("session started; greeter exiting");
                    g.exit_code = 0;
                    self.running.store(false, Ordering::SeqCst);
                } else if was_loading {
                    self.set_idle_suppressed(false);
                }
            }
            Err(err) => {
                error!(%err, "greetd exchange failed; greeter exiting");
                g.exit_code = 1;
                self.running.store(false, Ordering::SeqCst);
            }
        }
        self.redraw_greeter();
    }

    /// Authentication succeeded: show "Loading CDE", animate it, hold idle blanking off and arm
    /// the presentation gate, whose release acks the worker (which then writes `start_session`).
    fn on_greeter_loading(&mut self, ack: mpsc::Sender<()>) {
        let Some(g) = self.greeter.as_mut() else {
            return;
        };
        g.loading = Some(Instant::now());
        self.set_idle_suppressed(true);
        let timer = smithay::reexports::calloop::timer::Timer::from_duration(ELLIPSIS_STEP);
        if let Err(err) = self.handle.insert_source(timer, |_, _, data| {
            use smithay::reexports::calloop::timer::TimeoutAction;
            if data.greeter.as_ref().is_some_and(|g| g.loading()) {
                data.redraw_greeter();
                TimeoutAction::ToDuration(ELLIPSIS_STEP)
            } else {
                TimeoutAction::Drop
            }
        }) {
            warn!(%err, "no ellipsis timer; the loading panel is static");
        }
        // `arm_present_gate` redraws every armed output itself.
        let armed = self.arm_present_gate(
            "greeter start_session",
            Box::new(move |_| {
                // The worker may have given up (backstop): nothing to do then.
                let _ = ack.send(());
            }),
        );
        if !armed {
            // Already armed (never in the greeter): dropping the ack aborts the exchange, the
            // transport-failure exit; `start_session` is never written ungated.
            warn!("present gate already armed; aborting the login");
        }
        self.redraw_greeter();
    }

    /// Feeds a panel key (only called in greeter mode).
    pub(crate) fn greeter_key(&mut self, key: GreeterKey) {
        let Some(g) = self.greeter.as_mut() else {
            return;
        };
        if let Some(event) = g.input.key(key)
            && g.events.send(event).is_err()
        {
            error!("the greetd worker is gone; greeter exiting");
            self.running.store(false, Ordering::SeqCst);
        }
        self.redraw_greeter();
    }

    /// Records that `set-layout` switched to group `idx` and redraws the indicator (a no-op
    /// outside greeter mode).
    pub(crate) fn greeter_layout_changed(&mut self, idx: usize) {
        let Some(g) = self.greeter.as_mut() else {
            return;
        };
        g.layout = idx;
        self.redraw_greeter();
    }

    fn redraw_greeter(&mut self) {
        let outputs: Vec<_> = self.space.outputs().cloned().collect();
        for o in &outputs {
            B::schedule_render(self, o);
        }
    }

    /// Whether the greeter's wallpaper client is running (`None` outside greeter mode).
    #[must_use]
    pub fn greeter_has_swaybg(&self) -> Option<bool> {
        self.greeter.as_ref().map(|g| g.swaybg.is_some())
    }

    /// The exit status greeter mode ends with (`None` outside greeter mode).
    #[must_use]
    pub fn greeter_exit_code(&self) -> Option<i32> {
        self.greeter.as_ref().map(|g| g.exit_code)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::input::{TestInputMode, admit};
    use crucible_keys::{Action, Bindings, Modifiers};

    #[test]
    fn keys_edit_the_field_and_submit_clears_it() {
        assert_eq!(
            greeter_key(Keysym::a, Some('a')),
            Some(GreeterKey::Char('a'))
        );
        assert_eq!(
            greeter_key(Keysym::Return, Some('\r')),
            Some(GreeterKey::Enter)
        );
        assert_eq!(greeter_key(Keysym::Tab, Some('\t')), None);
        let mut i = LoginInput::default();
        for c in "ab".chars() {
            i.key(GreeterKey::Char(c));
        }
        i.key(GreeterKey::Backspace);
        assert_eq!(i.text(), "a");
        assert_eq!(
            i.key(GreeterKey::Enter),
            Some(Event::Submit(Secret::new("a")))
        );
        assert_eq!(i.text(), "", "the buffer is cleared after Submit");
        assert_eq!(
            i.key(GreeterKey::Char('x')),
            None,
            "input is ignored while busy"
        );
        assert_eq!(i.text(), "");
        i.done();
        i.key(GreeterKey::Char('x'));
        assert_eq!(i.text(), "x");
    }

    #[test]
    fn greeter_config_blanks_on_the_profile_timeout_and_never_locks() {
        let mut p = GreeterProfile::fallback();
        p.blank_after = 90;
        let cfg = greeter_config(&p);
        assert_eq!(cfg.idle.blank_after, Some(90));
        assert_eq!(cfg.idle.lock_after, None, "the greeter never locks");
        assert_eq!(
            greeter_config(&GreeterProfile::fallback()).idle.blank_after,
            Some(600)
        );
        // The machine `start_idle_timer` drives, fed this config: blanks at the timeout, never
        // asks for a lock (the timer arming itself needs a live compositor and is untested).
        let mut m = crate::idle::IdleMachine::default();
        let tick = |m: &mut crate::idle::IdleMachine, s| {
            m.tick(std::time::Duration::from_secs(s), false, false, &cfg.idle)
        };
        assert_eq!(tick(&mut m, 89), crate::idle::IdleStep::default());
        let step = tick(&mut m, 1);
        assert!(step.blank && !step.lock);
        assert!(!tick(&mut m, 100_000).lock);
    }

    #[test]
    fn only_vt_switch_and_set_layout_chords_run_in_the_greeter() {
        let b = Bindings::default();
        let sup = Modifiers {
            logo: true,
            shift: false,
            ctrl: false,
        };
        let run = crucible_keys::resolve(&b, sup, Keysym::Return).expect("Super+Return is bound");
        let greeter_admits = |action: &Action| admit(&TestInputMode::<()>::Greeter, action).is_ok();
        assert!(
            !greeter_admits(&run),
            "a user bind like Super+Return does nothing"
        );
        assert!(greeter_admits(&Action::VtSwitch(2)));
        assert!(greeter_admits(&Action::SetLayout("us".into())));
    }

    #[test]
    fn scripted_login_through_the_worker_reaches_the_started_session() {
        let script = "{\"type\":\"auth_message\",\"auth_message_type\":\"secret\",\"auth_message\":\"Password:\"}\n{\"type\":\"success\"}\n{\"type\":\"success\"}\n";
        let replies = crucible_greetd::fake::parse_replies(script).unwrap();
        let (stream, server) = crucible_greetd::fake::connect(replies).unwrap();
        let (tx, rx) = mpsc::channel::<WorkerMsg>();
        let events = spawn_worker(Session::new(stream, "cde-session"), move |r| {
            tx.send(r).is_ok()
        });
        let mut input = LoginInput::default();
        let mut loadings = 0;
        let mut submit = |text: &str| {
            for c in text.chars() {
                input.key(GreeterKey::Char(c));
            }
            events.send(input.key(GreeterKey::Enter).unwrap()).unwrap();
            loop {
                match rx.recv().unwrap() {
                    // The compositor's gate released: ack, the worker writes start_session.
                    WorkerMsg::Loading(ack) => {
                        loadings += 1;
                        ack.send(()).unwrap();
                    }
                    WorkerMsg::Reply(reply) => {
                        input.done();
                        return (reply.unwrap(), loadings);
                    }
                }
            }
        };
        let ((view, done), n) = submit("dulrich");
        assert_eq!(n, 0, "no loading before start_session");
        assert_eq!(
            (view.prompt.as_str(), view.secret, done),
            ("Password:", true, false)
        );
        let ((_, done), n) = submit("pw");
        assert!(done, "start_session succeeded");
        assert_eq!(n, 1, "one loading ack, before start_session");
        assert_eq!(server.join().unwrap().len(), 3);
    }

    #[test]
    fn swaybg_gets_the_profile_background() {
        assert_eq!(
            swaybg_args(&ProfileBackground::Color(crucible_config::Rgb(
                0, 0x10, 0xff
            ))),
            ["-o", "*", "-c", "0010ff"]
        );
        assert_eq!(
            swaybg_args(&ProfileBackground::Image("/b.png".into())),
            ["-o", "*", "-i", "/b.png", "-m", "fill"]
        );
    }
    fn test_greeter() -> Greeter {
        let (events, _) = mpsc::channel();
        Greeter {
            profile: GreeterProfile::fallback(),
            view: View {
                prompt: "Password:".into(),
                secret: true,
                message: None,
            },
            input: LoginInput::default(),
            events,
            swaybg: None,
            layout: 0,
            loading: None,
            exit_code: 1,
        }
    }

    #[test]
    fn loading_frame_has_no_prompt_or_field_and_animates() {
        let mut g = test_greeter();
        assert_eq!(
            g.frame().field.as_deref(),
            Some(""),
            "the login panel has a field"
        );
        let t0 = Instant::now();
        g.loading = Some(t0);
        let f = g.frame_at(t0);
        assert_eq!(f.prompt, "Loading CDE");
        assert_eq!((f.field, f.message, f.layout), (None, None, None));
        assert_eq!(g.frame_at(t0 + ELLIPSIS_STEP * 3).prompt, "Loading CDE...");
    }

    #[test]
    fn an_error_reply_after_loading_returns_to_the_prompt_with_its_message() {
        let mut g = test_greeter();
        g.loading = Some(Instant::now());
        let err = View {
            prompt: "Username".into(),
            secret: false,
            message: Some("session failed".into()),
        };
        assert!(g.accept(err.clone(), false), "loading cleared");
        assert!(!g.loading());
        let f = g.frame();
        assert_eq!(f.prompt, "Username");
        assert_eq!(f.field.as_deref(), Some(""));
        assert_eq!(f.message.as_deref(), Some("session failed"));
    }
    #[test]
    fn the_started_session_keeps_the_loading_frame() {
        let mut g = test_greeter();
        g.loading = Some(Instant::now());
        assert!(!g.accept(g.view.clone(), true));
        assert!(g.loading(), "the last frame stays Loading CDE");
    }
}
