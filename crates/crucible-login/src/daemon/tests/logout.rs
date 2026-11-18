//! Logout, quit hand-off and queued-start tests.

use super::*;

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
