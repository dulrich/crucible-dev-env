//! Greeter and session start flow, retries and the pure rules.

use super::*;

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
