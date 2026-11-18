//! Reply-event tests (review A6, A11): PAM replies arrive as worker-fd events, the loop is
//! served while one is outstanding, and the reply slot routes each answer to the client that
//! asked. Shares the fake seam of the parent module.

use super::*;

fn hold(s: &S, on: bool) {
    s.borrow_mut().hold = on;
}

/// The held replies become readable and the daemon collects them.
fn release(d: &mut D, s: &S, now: Instant) {
    hold(s, false);
    d.on_worker_input(now);
}

fn fresh(s: &S) -> (D, Instant) {
    let b = FakeBackend {
        s: s.clone(),
        remap: HashMap::new(),
    };
    let mut d = Daemon::new(b, FakeVt { s: s.clone() }, cfg());
    let now = Instant::now();
    d.start(now);
    (d, now)
}

fn create_from(
    d: &mut D,
    client: ClientId,
    user: &str,
    now: Instant,
) -> Option<(ClientId, Response)> {
    req_from(
        d,
        client,
        Request::CreateSession {
            username: user.into(),
        },
        now,
    )
}

fn user_closes(s: &S) -> usize {
    log(s)
        .iter()
        .filter(|e| e.starts_with("close #") && e.ends_with(" user"))
        .count()
}

#[test]
fn loop_is_served_while_an_auth_reply_is_outstanding() {
    let (mut d, s, now) = setup(&[]);
    running_session(&mut d, &s, now);
    assert_eq!(input(&mut d, b"quit\n", false, now), None);
    hold(&s, true);
    assert_eq!(
        create(&mut d, "bob", now),
        None,
        "the step reply is outstanding"
    );
    assert!(d.busy());
    assert_eq!(d.worker_fds().len(), 1);
    // The session channel is served: a repeated quit is ignored, EOF retires the channel.
    assert_eq!(input(&mut d, b"quit\n", true, now), None);
    assert!(d.ready_fd().is_none() && d.handed_off());
    // A tick fires the logout fallback deadline.
    assert_eq!(tick(&mut d, now + LOGOUT_WAIT), None);
    assert_eq!(count_after(&log(&s), "running", "activate vt7"), 1);
    // SIGCHLD reaps the old session.
    exit(&s, "alice");
    assert_eq!(children(&mut d, now), None);
    assert!(!d.session_active());
    release(&mut d, &s, now);
    assert!(matches!(reply(&mut d), Some(Response::AuthMessage { .. })));
    assert!(!d.busy() && d.worker_fds().is_empty());
}

#[test]
fn the_reply_goes_to_the_client_that_asked() {
    let (mut d, s, now) = setup(&[]);
    hold(&s, true);
    assert_eq!(create_from(&mut d, 2, "alice", now), None);
    // Another client's request meanwhile is refused at once; the outstanding one is untouched.
    assert!(matches!(
        req_from(&mut d, 1, Request::CancelSession, now),
        Some((1, Response::Error { .. }))
    ));
    assert_eq!(user_closes(&s), 0);
    release(&mut d, &s, now);
    assert!(matches!(
        d.take_reply(),
        Some((2, Response::AuthMessage { .. }))
    ));
    assert_eq!(
        req_from(&mut d, 1, Request::CancelSession, now),
        Some((1, Response::Success))
    );
}

#[test]
fn the_deferred_start_reply_goes_to_its_client() {
    let (mut d, _s, now) = setup(&[]);
    assert!(create_from(&mut d, 4, "alice", now).is_some());
    let pw = Request::PostAuthMessageResponse {
        response: Some(Secret::new("pw")),
    };
    assert_eq!(req_from(&mut d, 5, pw, now), Some((5, Response::Success)));
    let start = Request::StartSession {
        cmd: vec!["cde-session".into()],
        env: Vec::new(),
    };
    assert_eq!(req_from(&mut d, 6, start, now), None);
    d.on_session_input(b"ready\n", false, now);
    assert_eq!(d.take_reply(), Some((6, Response::Success)));
}

#[test]
fn worker_exit_racing_the_spawn_reply_fails_the_start() {
    let (mut d, s, now) = setup(&[]);
    create(&mut d, "alice", now);
    assert_eq!(answer(&mut d, "pw", now), Some(Response::Success));
    s.borrow_mut().spawn_eof = 1;
    hold(&s, true);
    assert_eq!(start_cmd(&mut d, now), None);
    // SIGCHLD before the reply is read: nothing to reap yet, the start stays outstanding.
    assert_eq!(children(&mut d, now), None);
    assert_eq!(d.worker_fds().len(), 1);
    release(&mut d, &s, now);
    assert!(matches!(reply(&mut d), Some(Response::Error { .. })));
    assert!(!d.session_active() && d.ready_fd().is_none() && d.worker_fds().is_empty());
    let l = log(&s);
    assert_eq!(count_after(&l, "spawn alice", "close #"), 1, "{l:#?}");
    d.shutdown(Duration::from_millis(50));
    assert_all_closed_once(&s);
}

#[test]
fn a_disconnect_with_a_reply_outstanding_discards_the_late_reply() {
    let (mut d, s, now) = setup(&[]);
    hold(&s, true);
    assert_eq!(create(&mut d, "alice", now), None);
    d.on_client_gone(C);
    release(&mut d, &s, now);
    assert_eq!(d.take_reply(), None, "discarded");
    assert!(!d.busy());
    assert_eq!(user_closes(&s), 0, "as today, the conversation stays");
    // The next conversation closes it, once.
    assert!(matches!(
        create_from(&mut d, 2, "bob", now),
        Some((2, Response::AuthMessage { .. }))
    ));
    assert_eq!(user_closes(&s), 1);
    d.shutdown(Duration::from_millis(50));
    assert_eq!(user_closes(&s), 2);
    assert_all_closed_once(&s);
}

#[test]
fn greeter_exit_with_an_auth_reply_outstanding_closes_after_the_reply() {
    let (mut d, s, now) = setup(&[]);
    hold(&s, true);
    assert_eq!(create(&mut d, "alice", now), None);
    exit(&s, "_greetd");
    assert_eq!(children(&mut d, now), None);
    assert_eq!(
        user_closes(&s),
        0,
        "not closed under an outstanding request"
    );
    release(&mut d, &s, now);
    assert!(matches!(reply(&mut d), Some(Response::Error { .. })));
    assert_eq!(user_closes(&s), 1);
    assert!(!d.busy() && d.worker_fds().is_empty());
}

#[test]
fn greeter_start_is_driven_by_replies() {
    let s: S = Rc::default();
    hold(&s, true);
    let (mut d, now) = fresh(&s);
    assert!(!d.greeter_running());
    assert_eq!(d.worker_fds().len(), 1);
    assert!(!log(&s).iter().any(|e| e.starts_with("activate")));
    release(&mut d, &s, now);
    let l = log(&s);
    assert!(d.greeter_running() && d.worker_fds().is_empty());
    assert!(pos(&l, "open greeter") < pos(&l, "spawn _greetd"));
    assert!(pos(&l, "spawn _greetd") < pos(&l, "activate vt7"));
}

#[test]
fn greeter_start_failure_at_each_step_backs_off() {
    type Fail = fn(&mut Shared);
    let cases: [(&str, Fail); 5] = [
        ("begin", |s| s.fail_greeter = 1),
        ("step", |s| s.fail_greeter_step = 1),
        ("prompts", |s| s.greeter_prompts = true),
        ("open", |s| s.fail_open = 1),
        ("spawn", |s| s.fail_spawn = 1),
    ];
    for (what, fail) in cases {
        let s: S = Rc::default();
        fail(&mut s.borrow_mut());
        hold(&s, true);
        let (mut d, now) = fresh(&s);
        release(&mut d, &s, now);
        assert!(!d.greeter_running(), "{what}");
        assert_eq!(d.greeter_due(), Some(now + Backoff::BASE), "{what}");
        s.borrow_mut().greeter_prompts = false;
        d.on_tick(now + Backoff::BASE);
        assert!(d.greeter_running(), "{what}");
        d.shutdown(Duration::from_millis(50));
        assert_all_closed_once(&s);
    }
}

#[test]
fn logout_fallback_fires_while_the_greeter_start_is_outstanding() {
    let (mut d, s, now) = setup(&[]);
    running_session(&mut d, &s, now);
    hold(&s, true);
    assert_eq!(input(&mut d, b"quit\n", false, now), None);
    assert!(!d.greeter_running() && d.handed_off());
    assert_eq!(d.next_deadline(), Some(now + LOGOUT_WAIT));
    d.on_tick(now + LOGOUT_WAIT);
    assert_eq!(count_after(&log(&s), "running", "activate vt7"), 1);
    release(&mut d, &s, now);
    assert!(d.greeter_running() && d.greeter_ready_fd().is_none());
    assert_eq!(
        count_after(&log(&s), "running", "activate vt7"),
        1,
        "no second switch"
    );
}

#[test]
fn held_start_reports_success_only_after_the_vt_switch() {
    let (mut d, s, now) = setup(&[]);
    create(&mut d, "alice", now);
    answer(&mut d, "pw", now);
    hold(&s, true);
    assert_eq!(start_cmd(&mut d, now), None);
    assert!(d.ready_fd().is_none(), "open outstanding");
    release(&mut d, &s, now);
    assert_eq!(reply(&mut d), None, "spawned, waiting for ready");
    d.on_session_input(b"ready\n", false, now);
    let r = reply(&mut d);
    mark(&s, "reply");
    assert_eq!(r, Some(Response::Success));
    let l = log(&s);
    assert!(pos(&l, "spawn alice") < pos(&l, "activate vt8"));
    assert!(pos(&l, "activate vt8") < pos(&l, "reply"));
}

#[test]
fn shutdown_signals_every_child_before_the_first_wait() {
    let (mut d, s, now) = setup(&[]);
    login(&mut d, "alice", now);
    assert_eq!(ready(&mut d), Some(Response::Success));
    s.borrow_mut().log_waits = true;
    mark(&s, "stop");
    d.shutdown(Duration::from_millis(50));
    let l = log(&s);
    let after = &l[pos(&l, "stop")..];
    let signals: Vec<usize> = (0..after.len())
        .filter(|&i| after[i].starts_with("signal"))
        .collect();
    let first_wait = after.iter().position(|e| e.starts_with("wait")).unwrap();
    assert_eq!(signals.len(), 2, "{after:#?}");
    assert!(signals.iter().all(|&i| i < first_wait), "{after:#?}");
    assert_all_closed_once(&s);
}
