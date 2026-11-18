//! The user-process spawn: the account, environment, argv and fd plan, and the fork + exec.
#![allow(unsafe_code)]

use std::ffi::CString;
use std::io;
use std::os::fd::{AsRawFd, OwnedFd, RawFd};

use crate::seam::Class;

/// The `PATH` every session starts with.
pub const SESSION_PATH: &str = "/usr/local/bin:/usr/bin:/bin";

/// The account fields the spawn needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Account {
    /// Login name.
    pub name: String,
    /// uid.
    pub uid: u32,
    /// Primary gid.
    pub gid: u32,
    /// Home directory.
    pub home: String,
    /// Login shell (`/bin/sh` if empty).
    pub shell: String,
}

/// The four XDG session entries, in this order: they go into PAM before `pam_open_session`
/// and again into the user process's exec defaults, so a PAM module cannot change them.
fn xdg_entries(vt: u32, class: Class) -> [String; 4] {
    [
        "XDG_SESSION_TYPE=wayland".to_owned(),
        format!("XDG_SESSION_CLASS={}", class.xdg_class()),
        "XDG_SEAT=seat0".to_owned(),
        format!("XDG_VTNR={vt}"),
    ]
}

/// The user process environment, built from scratch: the PAM env, then the account and XDG
/// variables, then `extra`; a later entry replaces an earlier one with the same key. Nothing
/// from the daemon's own environment is included.
pub fn build_env(
    pam_env: &[String],
    acct: &Account,
    vt: u32,
    class: Class,
    extra: &[String],
) -> Vec<String> {
    let account = [
        format!("HOME={}", acct.home),
        format!("USER={}", acct.name),
        format!("LOGNAME={}", acct.name),
        format!("SHELL={}", acct.shell),
        format!("PATH={SESSION_PATH}"),
    ];
    let xdg = xdg_entries(vt, class);
    let mut out: Vec<String> = Vec::new();
    for kv in pam_env.iter().chain(&account).chain(&xdg).chain(extra) {
        let Some((k, _)) = kv.split_once('=') else {
            continue;
        };
        out.retain(|e| e.split_once('=').map(|(ek, _)| ek) != Some(k));
        out.push(kv.clone());
    }
    out
}

/// The PAM env entries set before `pam_open_session`.
pub fn session_pam_env(vt: u32, class: Class, extra: &[String]) -> Vec<String> {
    let mut v = xdg_entries(vt, class).to_vec();
    v.extend(extra.iter().cloned());
    v
}

/// The shell the command runs through: the account's shell when `/etc/shells` lists it, else
/// `/bin/sh`. A system account such as the greeter's has `/usr/sbin/nologin`, which would
/// refuse the command (greetd always runs `/bin/sh`).
pub fn exec_shell<'a>(shell: &'a str, etc_shells: &str) -> &'a str {
    let listed = etc_shells
        .lines()
        .map(str::trim)
        .any(|l| !l.starts_with('#') && l == shell);
    if listed { shell } else { "/bin/sh" }
}

/// The exec argv: the login shell as a login shell running the command words as `"$@"`, so
/// no word is ever re-split or re-quoted.
pub fn login_argv(shell: &str, cmd: &[String]) -> Vec<String> {
    let mut v = vec![
        shell.to_owned(),
        "-l".to_owned(),
        "-c".to_owned(),
        "exec \"$@\"".to_owned(),
        "cde-login-session".to_owned(),
    ];
    v.extend(cmd.iter().cloned());
    v
}

/// The fds that survive a close: stdio plus `keep` (the extra fd's target in the user process,
/// the daemon socket in the worker). Returns the inclusive ranges to close (`close_range`),
/// everything else.
pub fn close_ranges(keep: Option<RawFd>) -> Vec<(u32, u32)> {
    match keep {
        Some(t) if t > 3 => vec![(3, t as u32 - 1), (t as u32 + 1, u32::MAX)],
        Some(3) => vec![(4, u32::MAX)],
        _ => vec![(3, u32::MAX)],
    }
}

/// `close_range(2)` over each of `ranges` (async-signal-safe: one syscall per range).
pub(super) fn close_all(ranges: &[(u32, u32)]) {
    for (lo, hi) in ranges {
        // SAFETY: close_range(2) only closes fds.
        unsafe { libc::syscall(libc::SYS_close_range, *lo, *hi, 0) };
    }
}

fn cstrs(v: &[String]) -> io::Result<Vec<CString>> {
    v.iter()
        .map(|s| {
            CString::new(s.as_str()).map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))
        })
        .collect()
}

fn ptrs(v: &[CString]) -> Vec<*const libc::c_char> {
    v.iter()
        .map(|c| c.as_ptr())
        .chain(std::iter::once(std::ptr::null()))
        .collect()
}

/// Forks the user process. Everything is prepared before the fork; the child only makes
/// syscalls and exits 1 without exec on any failure.
#[allow(clippy::too_many_arguments)]
pub(super) fn spawn_user(
    account: &str,
    cmd: &[String],
    extra_env: &[String],
    vt: u32,
    class: Class,
    pam_env: &[String],
    extra: Option<(OwnedFd, RawFd)>,
) -> io::Result<i32> {
    if cmd.is_empty() {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "empty command"));
    }
    let acct = crate::sys::lookup_account(account)?;
    let env = cstrs(&build_env(pam_env, &acct, vt, class, extra_env))?;
    let etc_shells = std::fs::read_to_string("/etc/shells").unwrap_or_default();
    let run_shell = exec_shell(&acct.shell, &etc_shells);
    let argv = cstrs(&login_argv(run_shell, cmd))?;
    let envp = ptrs(&env);
    let argvp = ptrs(&argv);
    let shell = CString::new(run_shell)?;
    let tty = CString::new(format!("/dev/tty{vt}"))?;
    let home = CString::new(acct.home.as_str())?;
    let name = CString::new(acct.name.as_str())?;
    let root = c"/";
    let ranges = close_ranges(extra.as_ref().map(|(_, t)| *t));
    let (src, target) = match &extra {
        Some((f, t)) => (f.as_raw_fd(), *t),
        None => (-1, -1),
    };
    // SAFETY: the worker is single-threaded; the child below calls only async-signal-safe
    // functions on memory prepared above, and leaves through execve or _exit.
    let pid = unsafe { libc::fork() };
    if pid < 0 {
        return Err(io::Error::last_os_error());
    }
    if pid == 0 {
        // SAFETY: see above.
        unsafe {
            let fail = || -> ! { libc::_exit(1) };
            let mut set: libc::sigset_t = std::mem::zeroed();
            libc::sigemptyset(&mut set);
            libc::sigprocmask(libc::SIG_SETMASK, &set, std::ptr::null_mut());
            for sig in [
                libc::SIGPIPE,
                libc::SIGTERM,
                libc::SIGINT,
                libc::SIGHUP,
                libc::SIGCHLD,
            ] {
                libc::signal(sig, libc::SIG_DFL);
            }
            if libc::setsid() < 0 {
                fail();
            }
            // Move the extra fd out of the way of stdio and its target first.
            let hi = if src >= 0 {
                let h = libc::fcntl(src, libc::F_DUPFD_CLOEXEC, target.max(64) + 1);
                if h < 0 {
                    fail();
                }
                h
            } else {
                -1
            };
            let t = libc::open(tty.as_ptr(), libc::O_RDWR | libc::O_CLOEXEC);
            if t < 0 || libc::ioctl(t, libc::TIOCSCTTY, 1) < 0 {
                fail();
            }
            for n in 0..3 {
                if libc::dup2(t, n) < 0 {
                    fail();
                }
            }
            if hi >= 0 && libc::dup2(hi, target) < 0 {
                fail();
            }
            close_all(&ranges);
            if libc::initgroups(name.as_ptr(), acct.gid as _) < 0
                || libc::setgid(acct.gid) < 0
                || libc::setuid(acct.uid) < 0
            {
                fail();
            }
            if acct.uid != 0 && (libc::setuid(0) == 0 || libc::geteuid() == 0) {
                fail();
            }
            if libc::chdir(home.as_ptr()) < 0 && libc::chdir(root.as_ptr()) < 0 {
                fail();
            }
            libc::execve(shell.as_ptr(), argvp.as_ptr(), envp.as_ptr());
            fail();
        }
    }
    // The worker's copy of the extra fd goes now; the user process holds the only one.
    drop(extra);
    Ok(pid)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| (*x).to_owned()).collect()
    }

    fn acct() -> Account {
        Account {
            name: "alice".into(),
            uid: 1000,
            gid: 1000,
            home: "/home/alice".into(),
            shell: "/bin/bash".into(),
        }
    }

    #[test]
    fn env_is_built_from_scratch() {
        let env = build_env(&[], &acct(), 8, Class::User, &[]);
        assert!(
            env.iter()
                .all(|e| !e.starts_with("CARGO") && !e.starts_with("RUST"))
        );
        for want in [
            "HOME=/home/alice",
            "USER=alice",
            "LOGNAME=alice",
            "SHELL=/bin/bash",
            "PATH=/usr/local/bin:/usr/bin:/bin",
            "XDG_SESSION_TYPE=wayland",
            "XDG_SESSION_CLASS=user",
            "XDG_SEAT=seat0",
            "XDG_VTNR=8",
        ] {
            assert!(env.contains(&want.to_owned()), "{want}");
        }
        assert_eq!(env.len(), 9);
    }

    #[test]
    fn env_merges_pam_then_base_then_extra() {
        let pam = s(&[
            "XDG_RUNTIME_DIR=/run/user/1000",
            "HOME=/bogus",
            "XDG_SESSION_ID=3",
        ]);
        let extra = s(&[
            "CDE_LOGIN_READY_FD=3",
            "XDG_SEAT=seat0",
            "GREETD_SOCK=/run/x",
        ]);
        let env = build_env(&pam, &acct(), 7, Class::Greeter, &extra);
        assert!(env.contains(&"XDG_RUNTIME_DIR=/run/user/1000".to_owned()));
        assert!(env.contains(&"XDG_SESSION_ID=3".to_owned()));
        assert!(env.contains(&"HOME=/home/alice".to_owned()));
        assert!(!env.contains(&"HOME=/bogus".to_owned()));
        assert!(env.contains(&"XDG_SESSION_CLASS=greeter".to_owned()));
        assert!(env.contains(&"CDE_LOGIN_READY_FD=3".to_owned()));
        assert_eq!(env.iter().filter(|e| e.starts_with("XDG_SEAT=")).count(), 1);
    }

    #[test]
    fn pam_env_before_open() {
        let v = session_pam_env(8, Class::User, &s(&["GREETD_SOCK=/s"]));
        assert_eq!(
            v,
            s(&[
                "XDG_SESSION_TYPE=wayland",
                "XDG_SESSION_CLASS=user",
                "XDG_SEAT=seat0",
                "XDG_VTNR=8",
                "GREETD_SOCK=/s"
            ])
        );
    }

    #[test]
    fn exec_shell_falls_back_for_unlisted_shells() {
        let shells = "# /etc/shells: valid login shells\n/bin/sh\n/bin/bash\n/usr/bin/dash\n";
        assert_eq!(exec_shell("/bin/bash", shells), "/bin/bash");
        assert_eq!(exec_shell("/usr/sbin/nologin", shells), "/bin/sh");
        assert_eq!(exec_shell("/bin/false", shells), "/bin/sh");
        assert_eq!(
            exec_shell("# /etc/shells: valid login shells", shells),
            "/bin/sh"
        );
        assert_eq!(exec_shell("/bin/bash", ""), "/bin/sh");
    }

    #[test]
    fn login_argv_keeps_words_intact() {
        let cmd = s(&[
            "/opt/cde/bin/crucible",
            "greeter",
            "a b",
            "it's \"q\"",
            "$HOME",
        ]);
        let argv = login_argv("/bin/bash", &cmd);
        assert_eq!(
            &argv[..5],
            &s(&["/bin/bash", "-l", "-c", "exec \"$@\"", "cde-login-session"])[..]
        );
        assert_eq!(&argv[5..], &cmd[..]);
    }

    #[test]
    fn login_argv_runs_through_a_real_shell() {
        let cmd = s(&["printf", "%s|", "a b", "it's \"q\"", "$HOME"]);
        let argv = login_argv("/bin/sh", &cmd);
        // `-l` is dropped here so the test does not depend on the host's profile.
        let out = std::process::Command::new(&argv[0])
            .args(&argv[2..])
            .env_clear()
            .output()
            .unwrap();
        assert_eq!(
            String::from_utf8(out.stdout).unwrap(),
            "a b|it's \"q\"|$HOME|"
        );
    }

    #[test]
    fn fd_plan_keeps_stdio_and_target_only() {
        let survives = |t: Option<RawFd>, fd: u32| {
            fd < 3
                || !close_ranges(t)
                    .iter()
                    .any(|(lo, hi)| (*lo..=*hi).contains(&fd))
        };
        for fd in 0..200 {
            assert_eq!(survives(Some(3), fd), fd <= 3, "fd {fd}");
            assert_eq!(survives(Some(5), fd), fd < 3 || fd == 5, "fd {fd}");
            assert_eq!(survives(None, fd), fd < 3, "fd {fd}");
            // The worker keeps its daemon socket the same way.
            assert_eq!(survives(Some(9), fd), fd < 3 || fd == 9, "fd {fd}");
        }
    }
}
