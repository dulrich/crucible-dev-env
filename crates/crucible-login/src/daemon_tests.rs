//! Daemon lifecycle and protocol tests against a fake seam and a fake VT.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use crucible_greetd::AuthMessageType;

use super::*;
use crate::seam::{SignalTarget, signal_target};

#[derive(Default)]
struct Proc {
    exit: Option<ExitStatus>,
    ignores_term: bool,
    /// `try_wait` fails this many more times before it reports again.
    wait_errs: usize,
    /// Every signal sent, with where the real backend would aim it.
    signals: Vec<(i32, SignalTarget)>,
    /// The handle this process's session closes when it ends (as the real worker does after
    /// reaping the user process): its id and class.
    session: Option<(usize, Class)>,
}

struct FakeChild {
    pid: i32,
    proc_: Rc<RefCell<Proc>>,
    s: S,
}

/// The process ends with `st` (unless it already has); its session then closes its handle.
fn end_proc(s: &S, p: &Rc<RefCell<Proc>>, st: ExitStatus) {
    let mut p = p.borrow_mut();
    if p.exit.is_some() {
        return;
    }
    p.exit = Some(st);
    if let Some((id, class)) = p.session.take() {
        let mut s = s.borrow_mut();
        s.log.push(format!("close #{id} {}", class.xdg_class()));
        s.closed_open += 1;
    }
}

impl Child for FakeChild {
    fn pid(&self) -> i32 {
        self.pid
    }
    fn try_wait(&mut self) -> Result<Option<ExitStatus>, SeamError> {
        if self.s.borrow().log_waits {
            self.s.borrow_mut().log.push(format!("wait {}", self.pid));
        }
        let mut p = self.proc_.borrow_mut();
        if p.wait_errs > 0 {
            p.wait_errs -= 1;
            return Err(SeamError::Other("ECHILD".into()));
        }
        Ok(p.exit)
    }
    fn signal(&mut self, sig: i32) -> Result<(), SeamError> {
        if self.s.borrow().log_waits {
            self.s
                .borrow_mut()
                .log
                .push(format!("signal {} {sig}", self.pid));
        }
        let dies = {
            let mut p = self.proc_.borrow_mut();
            p.signals.push((sig, signal_target(sig)));
            sig == libc::SIGKILL || !p.ignores_term
        };
        if dies {
            end_proc(&self.s, &self.proc_, ExitStatus::Signal(sig));
        }
        Ok(())
    }
}

#[derive(Default)]
struct Shared {
    log: Vec<String>,
    procs: Vec<(String, Rc<RefCell<Proc>>)>,
    opened: usize,
    closed_open: usize,
    fail_greeter: usize,
    /// The next this many greeter steps fail.
    fail_greeter_step: usize,
    /// Greeter steps answer with a prompt (a conversation that never finishes).
    greeter_prompts: bool,
    /// The next this many opens fail.
    fail_open: usize,
    /// The next this many spawns fail.
    fail_spawn: usize,
    /// The next this many spawns see the worker exit before its reply (EOF).
    spawn_eof: usize,
    /// While set, every reply stays outstanding ("not yet readable").
    hold: bool,
    /// Log every `try_wait` and signal (for the shutdown order).
    log_waits: bool,
}

type S = Rc<RefCell<Shared>>;

struct FakeBackend {
    s: S,
    remap: HashMap<String, String>,
}

struct FakeHandle {
    s: S,
    id: usize,
    class: Class,
    user: String,
    step: usize,
    /// The account authentication resolved, as the real worker takes it from `PAM_USER`.
    account: Option<String>,
    /// The VT given to `open`.
    vt: Option<u32>,
    /// The reply to the request sent, collected by `take_reply`.
    reply: Option<Result<Reply, SeamError>>,
}

impl Backend for FakeBackend {
    type Handle = FakeHandle;
    fn begin(&mut self, class: Class, user: &str) -> Result<FakeHandle, SeamError> {
        let mut s = self.s.borrow_mut();
        if class == Class::Greeter && s.fail_greeter > 0 {
            s.fail_greeter -= 1;
            return Err(SeamError::Other("greeter broken".into()));
        }
        let id = s.log.len();
        s.log.push(format!("begin {} {user}", class.xdg_class()));
        let user = self.remap.get(user).cloned().unwrap_or(user.to_owned());
        Ok(FakeHandle {
            s: self.s.clone(),
            id,
            class,
            user,
            step: 0,
            account: None,
            vt: None,
            reply: None,
        })
    }
}

impl FakeHandle {
    fn step_reply(&mut self, answer: Option<Secret>) -> Result<AuthStep, SeamError> {
        let ok = if self.class == Class::Greeter {
            let mut s = self.s.borrow_mut();
            if s.fail_greeter_step > 0 {
                s.fail_greeter_step -= 1;
                false
            } else if s.greeter_prompts {
                return Ok(AuthStep::Prompt(AuthMessageType::Info, "hi".into()));
            } else {
                true
            }
        } else {
            self.step += 1;
            match self.step {
                1 => {
                    return Ok(AuthStep::Prompt(
                        AuthMessageType::Secret,
                        "Password: ".into(),
                    ));
                }
                _ => answer.as_ref().map(Secret::expose) == Some("pw"),
            }
        };
        if !ok {
            return Err(SeamError::Auth("Authentication failure".into()));
        }
        self.account = Some(self.user.clone());
        Ok(AuthStep::Authenticated {
            account: self.user.clone(),
        })
    }
}

impl SessionHandle for FakeHandle {
    type Child = FakeChild;
    fn fd(&self) -> RawFd {
        1000 + self.id as RawFd
    }
    fn send_step(&mut self, answer: Option<Secret>) -> Result<(), SeamError> {
        assert!(self.reply.is_none(), "a request is outstanding");
        self.reply = Some(self.step_reply(answer).map(Reply::Step));
        Ok(())
    }
    fn send_open(&mut self, spec: &OpenSpec) -> Result<(), SeamError> {
        assert!(self.reply.is_none(), "a request is outstanding");
        let account = self.account.as_deref().expect("open before authentication");
        let mut s = self.s.borrow_mut();
        if s.fail_open > 0 {
            s.fail_open -= 1;
            self.reply = Some(Err(SeamError::Other("open failed".into())));
            return Ok(());
        }
        s.log.push(format!(
            "open {} {account} vt{}",
            self.class.xdg_class(),
            spec.vt
        ));
        s.opened += 1;
        self.vt = Some(spec.vt);
        self.reply = Some(Ok(Reply::Opened));
        Ok(())
    }
    fn send_spawn(&mut self, spec: SpawnSpec) -> Result<(), SeamError> {
        assert!(self.reply.is_none(), "a request is outstanding");
        assert!(self.vt.is_some(), "spawn before open");
        let account = self.account.clone().expect("spawn before authentication");
        let fd = spec.extra_fd.as_ref().map(|(_, n)| *n);
        let mut s = self.s.borrow_mut();
        s.log.push(format!(
            "spawn {account} {} env={} fd={fd:?}",
            spec.cmd.join(" "),
            spec.env.join(",")
        ));
        // The spec (and the extra fd) is dropped once the request is sent.
        drop(spec);
        self.reply = Some(if s.fail_spawn > 0 {
            s.fail_spawn -= 1;
            Err(SeamError::Other("spawn failed".into()))
        } else if s.spawn_eof > 0 {
            s.spawn_eof -= 1;
            Err(SeamError::Other("session worker exited".into()))
        } else {
            Ok(Reply::Spawned)
        });
        Ok(())
    }
    fn take_reply(&mut self) -> Result<Option<Reply>, SeamError> {
        if self.s.borrow().hold {
            return Ok(None);
        }
        self.reply.take().transpose()
    }
    fn into_child(self) -> FakeChild {
        let account = self.account.clone().expect("spawned before authentication");
        let mut s = self.s.borrow_mut();
        let p = Rc::new(RefCell::new(Proc {
            session: Some((self.id, self.class)),
            ..Proc::default()
        }));
        s.procs.push((account, p.clone()));
        FakeChild {
            pid: 100 + s.procs.len() as i32,
            proc_: p,
            s: self.s.clone(),
        }
    }
    fn close(self) -> Result<FakeChild, SeamError> {
        let mut s = self.s.borrow_mut();
        s.log
            .push(format!("close #{} {}", self.id, self.class.xdg_class()));
        if self.vt.is_some() {
            s.closed_open += 1;
        }
        // The closed worker, already exited (reaped at once).
        Ok(FakeChild {
            pid: 0,
            proc_: Rc::new(RefCell::new(Proc {
                exit: Some(ExitStatus::Code(0)),
                ..Proc::default()
            })),
            s: self.s.clone(),
        })
    }
}

struct FakeVt {
    s: S,
}

impl Vt for FakeVt {
    fn set_graphics(&mut self, vt: u32) -> io::Result<()> {
        self.s.borrow_mut().log.push(format!("graphics vt{vt}"));
        Ok(())
    }
    fn activate(&mut self, vt: u32) -> io::Result<()> {
        self.s.borrow_mut().log.push(format!("activate vt{vt}"));
        Ok(())
    }
}

fn cfg() -> Config {
    Config {
        greeter_user: "_greetd".into(),
        greeter_cmd: vec!["greeter".into()],
        greeter_vt: 7,
        session_vt: 8,
        ready_timeout: Duration::from_secs(10),
        sock_path: "/run/cde-login/g.sock".into(),
    }
}

fn setup(remap: &[(&str, &str)]) -> (Daemon<FakeBackend, FakeVt>, S, Instant) {
    let s: S = Rc::default();
    let b = FakeBackend {
        s: s.clone(),
        remap: remap
            .iter()
            .map(|(a, b)| ((*a).to_owned(), (*b).to_owned()))
            .collect(),
    };
    let mut d = Daemon::new(b, FakeVt { s: s.clone() }, cfg());
    let now = Instant::now();
    d.start(now);
    (d, s, now)
}

fn log(s: &S) -> Vec<String> {
    s.borrow().log.clone()
}

fn mark(s: &S, m: &str) {
    s.borrow_mut().log.push(m.to_owned());
}

fn pos(l: &[String], prefix: &str) -> usize {
    l.iter()
        .rposition(|e| e.starts_with(prefix))
        .unwrap_or_else(|| panic!("{prefix} not in {l:#?}"))
}

type D = Daemon<FakeBackend, FakeVt>;

/// The client id the single-client tests use.
const C: ClientId = 1;

/// The reply in the slot (asserting it is for `C`).
fn reply(d: &mut D) -> Option<Response> {
    d.take_reply().map(|(id, r)| {
        assert_eq!(id, C, "reply routed to the wrong client");
        r
    })
}

/// Sends `r` from `client`; a busy refusal or the slot's reply.
fn req_from(d: &mut D, client: ClientId, r: Request, now: Instant) -> Option<(ClientId, Response)> {
    match d.handle(client, r, now) {
        Err(refusal) => Some((client, refusal)),
        Ok(()) => d.take_reply(),
    }
}

fn req(d: &mut D, r: Request, now: Instant) -> Option<Response> {
    req_from(d, C, r, now).map(|(id, r)| {
        assert_eq!(id, C);
        r
    })
}

fn ready(d: &mut D) -> Option<Response> {
    d.on_ready();
    reply(d)
}

fn tick(d: &mut D, t: Instant) -> Option<Response> {
    d.on_tick(t);
    reply(d)
}

fn children(d: &mut D, t: Instant) -> Option<Response> {
    d.poll_children(t);
    reply(d)
}

fn input(d: &mut D, buf: &[u8], eof: bool, t: Instant) -> Option<Response> {
    d.on_session_input(buf, eof, t);
    reply(d)
}

fn create(d: &mut D, user: &str, now: Instant) -> Option<Response> {
    req(
        d,
        Request::CreateSession {
            username: user.into(),
        },
        now,
    )
}

fn answer(d: &mut D, pw: &str, now: Instant) -> Option<Response> {
    req(
        d,
        Request::PostAuthMessageResponse {
            response: Some(Secret::new(pw)),
        },
        now,
    )
}

fn start_cmd(d: &mut D, now: Instant) -> Option<Response> {
    req(
        d,
        Request::StartSession {
            cmd: vec!["cde-session".into()],
            env: vec!["X=1".into()],
        },
        now,
    )
}

/// Logs in `user` with the right password; leaves the session starting.
fn login(d: &mut Daemon<FakeBackend, FakeVt>, user: &str, now: Instant) {
    assert!(matches!(
        create(d, user, now),
        Some(Response::AuthMessage { .. })
    ));
    assert_eq!(answer(d, "pw", now), Some(Response::Success));
    assert_eq!(start_cmd(d, now), None);
}

fn proc_of(s: &S, account: &str) -> Rc<RefCell<Proc>> {
    s.borrow()
        .procs
        .iter()
        .rev()
        .find(|(a, _)| a == account)
        .map(|(_, p)| p.clone())
        .unwrap()
}

fn exit(s: &S, account: &str) {
    end_proc(s, &proc_of(s, account), ExitStatus::Code(0));
}

#[test]
fn start_runs_greeter_then_activates_its_vt() {
    let (d, s, _) = setup(&[]);
    let l = log(&s);
    assert!(d.greeter_running());
    assert!(l.iter().any(|e| e == "open greeter _greetd vt7"));
    assert!(l.iter().any(|e| {
        e.contains("spawn _greetd greeter env=GREETD_SOCK=/run/cde-login/g.sock fd=None")
    }));
    assert!(pos(&l, "spawn _greetd") < pos(&l, "activate vt7"));
}

#[test]
fn success_path_order() {
    let (mut d, s, now) = setup(&[]);
    login(&mut d, "alice", now);
    assert!(d.ready_fd().is_some());
    let reply = ready(&mut d);
    mark(&s, "reply");
    assert_eq!(reply, Some(Response::Success));
    let l = log(&s);
    let open = pos(&l, "open user alice vt8");
    let gfx = pos(&l, "graphics vt8");
    let spawn = pos(&l, "spawn alice cde-session");
    let act = pos(&l, "activate vt8");
    assert!(
        open < gfx && gfx < spawn && spawn < act && act < pos(&l, "reply"),
        "{l:#?}"
    );
    assert!(l[spawn].contains("env=X=1,CDE_LOGIN_READY_FD=3 fd=Some(3)"));
    assert!(!l.iter().any(|e| e.contains("pw")), "no password in logs");
}

#[test]
fn ready_pipe_write_end_is_closed_in_daemon() {
    let (mut d, _s, now) = setup(&[]);
    login(&mut d, "alice", now);
    let fd = d.ready_fd().unwrap();
    let mut buf = [0u8; 8];
    #[allow(unsafe_code)]
    // SAFETY: reading into a local buffer from a pipe fd the daemon owns.
    let n = unsafe { libc::read(fd, buf.as_mut_ptr().cast(), buf.len()) };
    assert_eq!(n, 0, "EOF: the backend dropped the only write end");
}

#[test]
fn wrong_password_then_retry() {
    let (mut d, _s, now) = setup(&[]);
    create(&mut d, "alice", now);
    assert!(matches!(
        answer(&mut d, "nope", now),
        Some(Response::Error {
            error_type: ErrorType::AuthError,
            ..
        })
    ));
    assert_eq!(
        req(&mut d, Request::CancelSession, now),
        Some(Response::Success)
    );
    login(&mut d, "alice", now);
    assert_eq!(ready(&mut d), Some(Response::Success));
}

#[test]
fn cancel_closes_the_pending_auth() {
    let (mut d, s, now) = setup(&[]);
    create(&mut d, "alice", now);
    assert_eq!(
        req(&mut d, Request::CancelSession, now),
        Some(Response::Success)
    );
    assert!(
        log(&s)
            .iter()
            .any(|e| e.ends_with(" user") && e.starts_with("close"))
    );
    assert!(matches!(
        answer(&mut d, "pw", now),
        Some(Response::Error { .. })
    ));
}

#[test]
fn start_before_auth_is_refused() {
    let (mut d, _s, now) = setup(&[]);
    assert!(matches!(
        start_cmd(&mut d, now),
        Some(Response::Error { .. })
    ));
    create(&mut d, "alice", now);
    assert!(matches!(
        start_cmd(&mut d, now),
        Some(Response::Error { .. })
    ));
}

#[test]
fn child_dies_before_ready() {
    let (mut d, s, now) = setup(&[]);
    login(&mut d, "alice", now);
    exit(&s, "alice");
    assert!(matches!(
        children(&mut d, now),
        Some(Response::Error { .. })
    ));
    assert!(!d.session_active());
    assert!(d.greeter_running());
    let l = log(&s);
    assert!(!l.iter().any(|e| e == "activate vt8"));
    assert_eq!(s.borrow().closed_open, 1);
}

#[test]
fn ready_timeout_activates_anyway() {
    let (mut d, s, now) = setup(&[]);
    login(&mut d, "alice", now);
    assert_eq!(d.next_deadline(), Some(now + Duration::from_secs(10)));
    assert_eq!(tick(&mut d, now + Duration::from_secs(5)), None);
    assert_eq!(
        tick(&mut d, now + Duration::from_secs(10)),
        Some(Response::Success)
    );
    assert_eq!(pos(&log(&s), "activate"), pos(&log(&s), "activate vt8"));
}

#[test]
fn logout_restarts_greeter_and_activates_its_vt() {
    let (mut d, s, now) = setup(&[]);
    login(&mut d, "alice", now);
    ready(&mut d);
    exit(&s, "_greetd");
    assert_eq!(children(&mut d, now), None);
    assert!(!d.greeter_running());
    assert_eq!(d.greeter_due(), None, "no restart while the session runs");
    exit(&s, "alice");
    children(&mut d, now);
    let l = log(&s);
    assert!(d.greeter_running());
    assert!(pos(&l, "close") < pos(&l, "spawn _greetd"));
    assert!(pos(&l, "spawn _greetd") < pos(&l, "activate vt7"));
    assert!(pos(&l, "activate vt8") < pos(&l, "activate vt7"));
    let sh = s.borrow();
    assert_eq!(
        sh.opened - sh.closed_open,
        1,
        "only the new greeter is open"
    );
}

#[test]
fn second_create_session_refused_while_session_runs() {
    let (mut d, _s, now) = setup(&[]);
    login(&mut d, "alice", now);
    assert!(matches!(
        create(&mut d, "bob", now),
        Some(Response::Error { .. })
    ));
    ready(&mut d);
    assert!(matches!(
        create(&mut d, "bob", now),
        Some(Response::Error { .. })
    ));
}

#[test]
fn greeter_crash_backs_off() {
    let (mut d, s, now) = setup(&[]);
    let mut t = now;
    for expect in [1u64, 2, 4] {
        exit(&s, "_greetd");
        children(&mut d, t);
        assert!(!d.greeter_running());
        let due = d.greeter_due().unwrap();
        assert_eq!(due - t, Duration::from_secs(expect));
        tick(&mut d, due - Duration::from_millis(1));
        assert!(!d.greeter_running());
        tick(&mut d, due);
        assert!(d.greeter_running());
        t = due;
    }
}

#[test]
fn greeter_start_failure_is_retried() {
    let s: S = Rc::default();
    s.borrow_mut().fail_greeter = 1;
    let b = FakeBackend {
        s: s.clone(),
        remap: HashMap::new(),
    };
    let mut d = Daemon::new(b, FakeVt { s: s.clone() }, cfg());
    let now = Instant::now();
    d.start(now);
    assert!(!d.greeter_running());
    d.on_tick(d.greeter_due().unwrap());
    assert!(d.greeter_running());
}

#[test]
fn backoff_doubles_caps_and_resets() {
    let mut b = Backoff::default();
    let ds: Vec<u64> = (0..7)
        .map(|_| b.on_failure(Duration::ZERO).as_secs())
        .collect();
    assert_eq!(ds, [1, 2, 4, 8, 16, 30, 30]);
    assert_eq!(b.on_failure(Backoff::STABLE), Backoff::BASE);
}

#[test]
fn peer_uid_rule() {
    assert!(peer_allowed(115, 115));
    assert!(peer_allowed(0, 115));
    assert!(!peer_allowed(1000, 115));
}

#[test]
fn ready_bytes_rule() {
    assert!(ready_seen(b"ready\n"));
    assert!(!ready_seen(b"rea"));
}

#[test]
fn e1_runs_as_resolved_account() {
    let (mut d, s, now) = setup(&[("alice", "bob")]);
    login(&mut d, "alice", now);
    let l = log(&s);
    assert!(l.iter().any(|e| e == "open user bob vt8"));
    assert!(l.iter().any(|e| e.starts_with("spawn bob cde-session")));
    assert!(!l.iter().any(|e| e.starts_with("spawn alice")));
}

fn assert_all_closed_once(s: &S) {
    let sh = s.borrow();
    assert_eq!(sh.opened, sh.closed_open, "{:#?}", sh.log);
    let mut ids: Vec<&String> = sh.log.iter().filter(|e| e.starts_with("close #")).collect();
    let n = ids.len();
    ids.sort();
    ids.dedup();
    assert_eq!(ids.len(), n, "a handle closed twice");
    assert!(sh.procs.iter().all(|(_, p)| p.borrow().exit.is_some()));
}

#[test]
fn e2_stop_during_authentication() {
    let (mut d, s, now) = setup(&[]);
    create(&mut d, "alice", now);
    d.shutdown(Duration::from_millis(50));
    assert_all_closed_once(&s);
    // The pending auth handle was closed too (begin user → close user).
    assert!(
        log(&s)
            .iter()
            .any(|e| e.starts_with("close") && e.ends_with(" user"))
    );
}

#[test]
fn e2_stop_during_ready_wait() {
    let (mut d, s, now) = setup(&[]);
    login(&mut d, "alice", now);
    d.shutdown(Duration::from_millis(50));
    assert_all_closed_once(&s);
    assert!(!d.session_active() && !d.greeter_running());
}

#[test]
fn e2_stop_during_session_kills_stubborn_child() {
    let (mut d, s, now) = setup(&[]);
    login(&mut d, "alice", now);
    ready(&mut d);
    proc_of(&s, "alice").borrow_mut().ignores_term = true;
    d.shutdown(Duration::from_millis(50));
    assert_eq!(
        proc_of(&s, "alice").borrow().exit,
        Some(ExitStatus::Signal(libc::SIGKILL))
    );
    assert_all_closed_once(&s);
}

#[test]
fn a13_sigkill_escalation_aims_at_the_user_pidfd() {
    let (mut d, s, now) = setup(&[]);
    login(&mut d, "alice", now);
    ready(&mut d);
    proc_of(&s, "alice").borrow_mut().ignores_term = true;
    d.shutdown(Duration::from_millis(50));
    assert_eq!(
        proc_of(&s, "alice").borrow().signals,
        vec![
            (libc::SIGTERM, SignalTarget::Session),
            (libc::SIGKILL, SignalTarget::UserPidfd)
        ]
    );
}

#[test]
fn a13_shutdown_wait_error_is_not_an_exit() {
    let (mut d, s, now) = setup(&[]);
    login(&mut d, "alice", now);
    ready(&mut d);
    // SIGTERM "works", but every wait fails: the child is not counted dead, so it is still
    // escalated, and its handle is still closed.
    proc_of(&s, "alice").borrow_mut().wait_errs = usize::MAX;
    d.shutdown(Duration::from_millis(50));
    let p = proc_of(&s, "alice");
    assert!(
        p.borrow()
            .signals
            .contains(&(libc::SIGKILL, SignalTarget::UserPidfd))
    );
    p.borrow_mut().wait_errs = 0;
    assert_all_closed_once(&s);
}

#[test]
fn a13_reap_error_keeps_the_session_running() {
    let (mut d, s, now) = setup(&[]);
    running_session(&mut d, &s, now);
    proc_of(&s, "alice").borrow_mut().wait_errs = 1;
    children(&mut d, now);
    assert!(d.session_active());
    assert_eq!(count_after(&log(&s), "running", "close"), 0);
    exit(&s, "alice");
    children(&mut d, now);
    assert!(!d.session_active());
}

/// Logs `alice` in, makes the session ready, lets the boot greeter exit (as a real greeter does
/// after `start_session`) and marks the log.
fn running_session(d: &mut Daemon<FakeBackend, FakeVt>, s: &S, now: Instant) {
    login(d, "alice", now);
    assert_eq!(input(d, b"ready\n", false, now), Some(Response::Success));
    exit(s, "_greetd");
    children(d, now);
    assert!(!d.greeter_running());
    mark(s, "running");
}

fn count_after(l: &[String], m: &str, prefix: &str) -> usize {
    l[pos(l, m)..]
        .iter()
        .filter(|e| e.starts_with(prefix))
        .count()
}

#[test]
fn quit_rule() {
    assert!(quit_seen(b"quit\n"));
    assert!(quit_seen(b"ready\nquit\n"));
    assert!(!quit_seen(b"ready\n"));
}

#[test]
fn logout_quit_greeter_ready_switch_then_leader_exit() {
    let (mut d, s, now) = setup(&[]);
    running_session(&mut d, &s, now);
    assert!(d.ready_fd().is_some(), "channel stays open after ready");
    assert_eq!(input(&mut d, b"quit\n", false, now), None);
    assert!(d.greeter_running() && d.handed_off());
    assert!(d.greeter_ready_fd().is_some());
    let l = log(&s);
    assert!(
        l.iter().any(|e| e.starts_with("spawn _greetd greeter")
            && e.contains("CDE_LOGIN_READY_FD=3")
            && e.ends_with("fd=Some(3)")),
        "{l:#?}"
    );
    assert_eq!(count_after(&l, "running", "activate"), 0, "no switch yet");
    d.on_greeter_ready();
    assert!(d.greeter_ready_fd().is_none());
    mark(&s, "switched");
    exit(&s, "alice");
    assert_eq!(children(&mut d, now), None);
    assert!(!d.session_active() && d.greeter_running());
    let l = log(&s);
    assert_eq!(count_after(&l, "running", "activate vt7"), 1);
    assert_eq!(
        count_after(&l, "switched", "activate"),
        0,
        "no second switch"
    );
    assert_eq!(count_after(&l, "switched", "spawn"), 0, "no second greeter");
    assert_eq!(count_after(&l, "running", "spawn _greetd"), 1);
    let sh = s.borrow();
    assert_eq!(
        sh.opened - sh.closed_open,
        1,
        "only the new greeter is open"
    );
}

#[test]
fn ready_and_quit_in_one_read() {
    let (mut d, s, now) = setup(&[]);
    login(&mut d, "alice", now);
    mark(&s, "running");
    assert_eq!(
        input(&mut d, b"ready\nquit\n", false, now),
        Some(Response::Success)
    );
    // The boot greeter is still up (already drawn), so the hand-off switches to it at once.
    assert!(d.handed_off() && d.greeter_ready_fd().is_none());
    let l = log(&s);
    assert!(pos(&l, "running") < pos(&l, "activate vt8"));
    assert!(pos(&l, "activate vt8") < pos(&l, "activate vt7"));
    assert_eq!(count_after(&l, "running", "spawn _greetd"), 0);
}

#[test]
fn logout_greeter_dies_before_ready() {
    let (mut d, s, now) = setup(&[]);
    running_session(&mut d, &s, now);
    input(&mut d, b"quit\n", false, now);
    exit(&s, "_greetd");
    children(&mut d, now);
    assert!(!d.greeter_running() && d.greeter_ready_fd().is_none());
    assert!(d.greeter_due().is_some(), "today's restart path");
    assert_eq!(count_after(&log(&s), "running", "activate vt7"), 1);
    exit(&s, "alice");
    children(&mut d, now);
    let due = d.greeter_due().unwrap();
    tick(&mut d, due);
    assert!(d.greeter_running());
}

#[test]
fn logout_greeter_start_failure_switches_anyway() {
    let (mut d, s, now) = setup(&[]);
    running_session(&mut d, &s, now);
    s.borrow_mut().fail_greeter = 1;
    input(&mut d, b"quit\n", false, now);
    assert!(!d.greeter_running() && d.greeter_due().is_some());
    assert_eq!(count_after(&log(&s), "running", "activate vt7"), 1);
}

#[test]
fn logout_fallback_after_fixed_wait() {
    let (mut d, s, now) = setup(&[]);
    running_session(&mut d, &s, now);
    input(&mut d, b"quit\n", false, now);
    assert_eq!(d.next_deadline(), Some(now + LOGOUT_WAIT));
    tick(&mut d, now + LOGOUT_WAIT - Duration::from_millis(1));
    assert_eq!(count_after(&log(&s), "running", "activate"), 0);
    tick(&mut d, now + LOGOUT_WAIT);
    assert_eq!(count_after(&log(&s), "running", "activate vt7"), 1);
    assert!(d.greeter_ready_fd().is_none() && d.greeter_running());
}

#[test]
fn crash_without_quit_keeps_todays_path() {
    let (mut d, s, now) = setup(&[]);
    running_session(&mut d, &s, now);
    exit(&s, "alice");
    children(&mut d, now);
    let l = log(&s);
    assert!(d.greeter_running());
    assert!(pos(&l, "running") < pos(&l, "spawn _greetd"));
    assert!(pos(&l, "spawn _greetd") < pos(&l, "activate vt7"));
}

#[test]
fn eof_after_ready_retires_the_channel() {
    let (mut d, s, now) = setup(&[]);
    running_session(&mut d, &s, now);
    assert_eq!(input(&mut d, b"", true, now), None);
    assert!(d.ready_fd().is_none() && !d.handed_off() && !d.greeter_running());
    assert_eq!(log(&s).last().map(String::as_str), Some("running"));
}

#[test]
fn login_while_old_session_closes_waits_then_opens() {
    let (mut d, s, now) = setup(&[]);
    running_session(&mut d, &s, now);
    input(&mut d, b"quit\n", false, now);
    d.on_greeter_ready();
    login(&mut d, "bob", now);
    assert!(
        !log(&s).iter().any(|e| e.starts_with("open user bob")),
        "session VT not opened while alice is open"
    );
    exit(&s, "alice");
    assert_eq!(children(&mut d, now), None, "now starting");
    let l = log(&s);
    assert!(pos(&l, "close") < pos(&l, "open user bob vt8"));
    assert_eq!(
        input(&mut d, b"ready\n", false, now),
        Some(Response::Success)
    );
    assert!(!d.handed_off() && d.session_active());
}

#[test]
fn queued_start_times_out() {
    let (mut d, s, now) = setup(&[]);
    running_session(&mut d, &s, now);
    input(&mut d, b"quit\n", false, now);
    d.on_greeter_ready();
    login(&mut d, "bob", now);
    assert!(matches!(
        tick(&mut d, now + LOGOUT_WAIT),
        Some(Response::Error { .. })
    ));
    exit(&s, "alice");
    children(&mut d, now);
    assert!(!d.session_active());
    assert!(!log(&s).iter().any(|e| e.starts_with("open user bob")));
}

#[test]
fn a4_failed_spawn_is_closed_once_by_the_backend() {
    let (mut d, s, now) = setup(&[]);
    s.borrow_mut().fail_spawn = 1;
    create(&mut d, "alice", now);
    assert_eq!(answer(&mut d, "pw", now), Some(Response::Success));
    assert!(matches!(
        start_cmd(&mut d, now),
        Some(Response::Error { .. })
    ));
    assert!(!d.session_active() && d.ready_fd().is_none());
    let l = log(&s);
    assert_eq!(count_after(&l, "spawn alice", "close #"), 1, "{l:#?}");
    assert!(
        l[pos(&l, "spawn alice")..]
            .iter()
            .any(|e| e.ends_with(" user"))
    );
    d.shutdown(Duration::from_millis(50));
    assert_all_closed_once(&s);
}

#[test]
fn a4_failed_greeter_spawn_is_closed_once_and_retried() {
    let (mut d, s, now) = setup(&[]);
    exit(&s, "_greetd");
    s.borrow_mut().fail_spawn = 1;
    children(&mut d, now);
    d.on_tick(d.greeter_due().unwrap());
    assert!(!d.greeter_running(), "the restart's spawn failed");
    d.on_tick(d.greeter_due().unwrap());
    assert!(d.greeter_running());
    d.shutdown(Duration::from_millis(50));
    assert_all_closed_once(&s);
}

#[test]
fn a4_a_session_that_ends_is_closed_by_its_child_not_the_daemon() {
    let (mut d, s, now) = setup(&[]);
    running_session(&mut d, &s, now);
    exit(&s, "alice");
    mark(&s, "exited");
    children(&mut d, now);
    let l = log(&s);
    // The one close of alice's handle came with its end, before the daemon reaped it.
    assert_eq!(count_after(&l, "running", "close #"), 1, "{l:#?}");
    assert_eq!(count_after(&l, "exited", "close #"), 0, "{l:#?}");
    assert!(!d.session_active());
}

#[path = "daemon_event_tests.rs"]
mod events;
