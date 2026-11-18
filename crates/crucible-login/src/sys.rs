//! Small libc wrappers shared by the daemon, the poll loop and the session worker: pipes,
//! signal handlers, `EINTR` retry and the passwd lookup.
#![allow(unsafe_code)]

use std::ffi::CString;
use std::io;
use std::os::fd::{FromRawFd, OwnedFd};

use crate::worker::Account;

/// `pipe2(O_CLOEXEC | extra_flags)`; returns `(read, write)`.
pub fn pipe_cloexec(extra_flags: libc::c_int) -> io::Result<(OwnedFd, OwnedFd)> {
    let mut fds = [0 as libc::c_int; 2];
    // SAFETY: pipe2 writes two fds into the array; on success we take ownership of both.
    if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC | extra_flags) } < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: both fds were just created and are owned by nobody else.
    Ok(unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) })
}

/// Installs `handler` for `sig` with an empty mask and exactly `flags` (`SA_RESTART` or `0`:
/// the caller chooses, since whether interrupted calls restart is part of its design).
pub fn set_handler(
    sig: libc::c_int,
    handler: libc::sighandler_t,
    flags: libc::c_int,
) -> io::Result<()> {
    // SAFETY: a zeroed sigaction with a plain handler, an empty mask and the given flags is
    // valid.
    unsafe {
        let mut sa: libc::sigaction = std::mem::zeroed();
        sa.sa_sigaction = handler;
        sa.sa_flags = flags;
        libc::sigemptyset(&mut sa.sa_mask);
        if libc::sigaction(sig, &sa, std::ptr::null_mut()) < 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

/// Runs `f` until it returns anything but an `Interrupted` error.
pub fn retry_eintr<T>(mut f: impl FnMut() -> io::Result<T>) -> io::Result<T> {
    loop {
        match f() {
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            r => return r,
        }
    }
}

/// `Ok(rc)` for a non-negative syscall return, else the `errno` error.
pub fn cvt<T: Copy + Default + PartialOrd>(rc: T) -> io::Result<T> {
    if rc < T::default() {
        Err(io::Error::last_os_error())
    } else {
        Ok(rc)
    }
}

/// Looks up an account's passwd entry (`NotFound` for a missing user).
pub fn lookup_account(name: &str) -> io::Result<Account> {
    let c = CString::new(name).map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
    // SAFETY: zeroed passwd is a valid out-param.
    let mut pw: libc::passwd = unsafe { std::mem::zeroed() };
    let mut buf = vec![0 as libc::c_char; 16 * 1024];
    let mut out = std::ptr::null_mut();
    // SAFETY: all pointers reference live locals sized as passed.
    let rc =
        unsafe { libc::getpwnam_r(c.as_ptr(), &mut pw, buf.as_mut_ptr(), buf.len(), &mut out) };
    if rc != 0 {
        return Err(io::Error::from_raw_os_error(rc));
    }
    if out.is_null() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("no user {name:?}"),
        ));
    }
    let s = |p: *const libc::c_char| {
        if p.is_null() {
            String::new()
        } else {
            // SAFETY: getpwnam_r strings are NUL-terminated inside buf.
            unsafe { std::ffi::CStr::from_ptr(p) }
                .to_string_lossy()
                .into_owned()
        }
    };
    let shell = s(pw.pw_shell);
    Ok(Account {
        name: s(pw.pw_name),
        uid: pw.pw_uid,
        gid: pw.pw_gid,
        home: s(pw.pw_dir),
        shell: if shell.is_empty() {
            "/bin/sh".into()
        } else {
            shell
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retry_eintr_retries_only_interrupted() {
        let mut n = 0;
        let r = retry_eintr(|| {
            n += 1;
            if n < 3 {
                Err(io::Error::from(io::ErrorKind::Interrupted))
            } else {
                Ok(n)
            }
        });
        assert_eq!(r.unwrap(), 3);
        let mut m = 0;
        let e = retry_eintr(|| -> io::Result<()> {
            m += 1;
            Err(io::Error::from(io::ErrorKind::WouldBlock))
        });
        assert_eq!(e.unwrap_err().kind(), io::ErrorKind::WouldBlock);
        assert_eq!(m, 1);
    }

    #[test]
    fn missing_account_is_not_found() {
        let e = lookup_account("cde-login-no-such-user-x").unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::NotFound);
    }
}
