//! Shutdown and stop paths (review E1, E2, A13).

use super::*;

#[test]
fn e1_runs_as_resolved_account() {
    let (mut d, s, now) = setup(&[("alice", "bob")]);
    login(&mut d, "alice", now);
    let l = log(&s);
    assert!(l.iter().any(|e| e == "open user bob vt8"));
    assert!(l.iter().any(|e| e.starts_with("spawn bob cde-session")));
    assert!(!l.iter().any(|e| e.starts_with("spawn alice")));
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
