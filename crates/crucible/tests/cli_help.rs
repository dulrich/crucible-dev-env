//! A bare `--help` (alone or beside a backend flag) prints usage and exits 0 without starting a
//! compositor backend — in particular `--tty-udev --help` must not take the DRM/KMS device.
use std::process::Command;

#[test]
fn help_prints_usage_and_starts_no_backend() {
    let dir = std::env::temp_dir().join(format!("cde-cli-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    for args in [
        &["--help"][..],
        &["-h"][..],
        &["--winit", "--help"][..],
        &["--tty-udev", "--help"][..],
        &["--help", "--tty-udev"][..],
    ] {
        let out = Command::new(env!("CARGO_BIN_EXE_crucible"))
            .args(args)
            .current_dir(&dir)
            .output()
            .expect("spawn crucible");
        assert_eq!(out.status.code(), Some(0), "{args:?}: {out:?}");
        let text = String::from_utf8_lossy(&out.stdout);
        assert!(text.contains("usage: crucible"), "{args:?}: {text}");
    }
    assert_eq!(
        std::fs::read_dir(&dir).unwrap().count(),
        0,
        "help created files"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Each subcommand answers `--help` with its own usage on stdout, exit 0, no side effects — even
/// with `CDE_SOCKET` unset and without a terminal.
#[test]
fn subcommand_help_prints_usage_with_no_side_effects() {
    let dir = std::env::temp_dir().join(format!("cde-cli-sub-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    for sub in ["pick", "resume", "msg"] {
        for flag in ["--help", "-h"] {
            let out = Command::new(env!("CARGO_BIN_EXE_crucible"))
                .args([sub, flag])
                .env_remove("CDE_SOCKET")
                .current_dir(&dir)
                .output()
                .expect("spawn crucible");
            assert_eq!(out.status.code(), Some(0), "{sub} {flag}: {out:?}");
            let text = String::from_utf8_lossy(&out.stdout);
            assert!(
                text.contains(&format!("usage: crucible {sub}")),
                "{sub} {flag}: {text}"
            );
        }
    }
    assert_eq!(
        std::fs::read_dir(&dir).unwrap().count(),
        0,
        "help created files"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn msg_without_socket_env_is_a_usage_error() {
    let out = Command::new(env!("CARGO_BIN_EXE_crucible"))
        .args(["msg", "candidates"])
        .env_remove("CDE_SOCKET")
        .output()
        .expect("spawn crucible");
    assert_eq!(out.status.code(), Some(2), "{out:?}");
    assert!(String::from_utf8_lossy(&out.stderr).contains("CDE_SOCKET"));
    assert!(out.stdout.is_empty());
}

#[test]
fn msg_rejects_invalid_requests_before_connecting() {
    let out = Command::new(env!("CARGO_BIN_EXE_crucible"))
        .args(["msg", "bogus"])
        .env("CDE_SOCKET", "/nonexistent/cde.sock")
        .output()
        .expect("spawn crucible");
    assert_eq!(out.status.code(), Some(2), "{out:?}");
}

#[test]
fn pick_without_socket_env_exits_2() {
    let out = Command::new(env!("CARGO_BIN_EXE_crucible"))
        .arg("pick")
        .env_remove("CDE_SOCKET")
        .output()
        .expect("spawn crucible");
    assert_eq!(out.status.code(), Some(2), "{out:?}");
}

#[test]
fn msg_round_trips_against_a_socket() {
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::UnixListener;
    let dir = std::env::temp_dir().join(format!("cde-cli-msg-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let sock = dir.join("s.sock");
    let listener = UnixListener::bind(&sock).unwrap();
    let t = std::thread::spawn(move || {
        for reply in ["ok", "err nope"] {
            let (s, _) = listener.accept().unwrap();
            let mut line = String::new();
            BufReader::new(s.try_clone().unwrap())
                .read_line(&mut line)
                .unwrap();
            assert_eq!(line, "open my%3Aname\n");
            writeln!(&s, "{reply}").unwrap();
        }
    });
    for (code, text) in [(0, "ok"), (1, "err nope")] {
        let out = Command::new(env!("CARGO_BIN_EXE_crucible"))
            .args(["msg", "open", "my%3Aname"])
            .env("CDE_SOCKET", &sock)
            .output()
            .expect("spawn crucible");
        assert_eq!(out.status.code(), Some(code), "{out:?}");
        assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), text);
    }
    t.join().unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn resume_without_command_execs_the_shell_in_cwd() {
    let dir = std::env::temp_dir().join(format!("cde-cli-resume-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_crucible"))
        .args(["resume", "--cwd"])
        .arg(&dir)
        .env("SHELL", "/bin/pwd")
        .output()
        .expect("spawn crucible");
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("CDE restored this terminal in"), "{text}");
    assert!(
        text.contains(dir.canonicalize().unwrap().to_str().unwrap()),
        "{text}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn resume_never_runs_the_command_without_enter() {
    use std::process::Stdio;
    let dir = std::env::temp_dir().join(format!("cde-cli-resume2-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let marker = dir.join("ran");
    // EOF on stdin -> straight to the shell (here /bin/true), command not run.
    let out = Command::new(env!("CARGO_BIN_EXE_crucible"))
        .args(["resume", "--cwd"])
        .arg(&dir)
        .arg("--")
        .arg(format!("touch {}", marker.display()))
        .env("SHELL", "/bin/true")
        .stdin(Stdio::null())
        .output()
        .expect("spawn crucible");
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    assert!(String::from_utf8_lossy(&out.stdout).contains("Last command: touch"));
    assert!(!marker.exists(), "command ran without Enter");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn resume_runs_the_command_on_enter() {
    use std::io::Write;
    use std::process::Stdio;
    let dir = std::env::temp_dir().join(format!("cde-cli-resume3-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_crucible"))
        .args(["resume", "--cwd"])
        .arg(&dir)
        .args(["--", "touch ran"])
        .env("SHELL", "/bin/sh")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .spawn()
        .expect("spawn crucible");
    // After the command resume execs an interactive shell, which exits at EOF.
    child.stdin.take().unwrap().write_all(b"\n").unwrap();
    assert!(child.wait().unwrap().success());
    assert!(dir.join("ran").exists(), "command did not run on Enter");
    let _ = std::fs::remove_dir_all(&dir);
}

/// While the restored command runs, its parent is still `crucible resume` with the banner argv,
/// so CDE's capture (`ResumeWrapper::from_argv`) keeps recording the command across the next restart.
#[test]
fn resume_stays_resident_and_capturable_while_the_command_runs() {
    use std::io::Write;
    use std::process::Stdio;
    let dir = std::env::temp_dir().join(format!("cde-cli-resume4-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let cmd = "echo ran; cat /proc/$PPID/cmdline > parent; sleep 0";
    let mut child = Command::new(env!("CARGO_BIN_EXE_crucible"))
        .args(["resume", "--cwd"])
        .arg(&dir)
        .args(["--", cmd])
        .env("SHELL", "/bin/sh")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn crucible");
    child.stdin.take().unwrap().write_all(b"\n").unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success(), "{out:?}");
    assert!(String::from_utf8_lossy(&out.stdout).contains("ran"));
    let raw = std::fs::read(dir.join("parent")).expect("command recorded its parent");
    let argv: Vec<String> = raw
        .split(|b| *b == 0)
        .filter(|s| !s.is_empty())
        .map(|s| String::from_utf8_lossy(s).into_owned())
        .collect();
    assert!(argv.iter().any(|a| a == "resume"), "{argv:?}");
    assert!(argv.iter().any(|a| a == "--cwd"), "{argv:?}");
    let cap = crucible_persist::ResumeWrapper::from_argv(&argv).expect("banner argv decodes");
    assert_eq!(cap.command.as_deref(), Some(cmd));
    assert_eq!(cap.cwd, dir);
    let _ = std::fs::remove_dir_all(&dir);
}
