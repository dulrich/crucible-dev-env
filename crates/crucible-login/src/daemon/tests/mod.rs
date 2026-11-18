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

mod events;
mod flow;
mod logout;
mod shutdown;
