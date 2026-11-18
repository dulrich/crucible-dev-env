//! The poll loop: the greeter socket, signals (self-pipe), the session and greeter pipes, the
//! PAM worker sockets and timers, turned into [`Daemon`] calls. One `poll(2)` loop, no threads,
//! no async runtime. A PAM round trip never blocks it: a request returns at once, the worker's
//! socket becomes readable, the daemon collects the reply and puts the client's answer in its
//! reply slot, and the loop writes it to the client that asked.

use std::fs;
use std::io;
use std::os::fd::{AsRawFd, IntoRawFd, OwnedFd, RawFd};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicI32, Ordering};
use std::time::{Duration, Instant};

use crucible_greetd::Request;
use crucible_greetd::codec::{read_message, write_message};

use crate::daemon::{Daemon, peer_allowed};
use crate::seam::Backend;
use crate::vt::Vt;

/// Grace between SIGTERM and SIGKILL at daemon stop.
pub const STOP_GRACE: Duration = Duration::from_secs(3);

static SIG_WRITE: AtomicI32 = AtomicI32::new(-1);

extern "C" fn on_signal(sig: libc::c_int) {
    let fd = SIG_WRITE.load(Ordering::Relaxed);
    let b = sig as u8;
    #[allow(unsafe_code)]
    // SAFETY: write(2) is async-signal-safe; the pipe is non-blocking.
    unsafe {
        libc::write(fd, (&b as *const u8).cast(), 1);
    }
}

fn signal_pipe() -> io::Result<OwnedFd> {
    let (r, w) = crate::sys::pipe_cloexec(libc::O_NONBLOCK)?;
    // The write end stays open for the process lifetime.
    SIG_WRITE.store(w.into_raw_fd(), Ordering::Relaxed);
    // SIGHUP stops the daemon like SIGTERM: the unit's `SendSIGHUP=yes` sends it alongside the
    // stop signal, and its default action would end the daemon before `shutdown` runs (A7).
    for sig in [libc::SIGTERM, libc::SIGINT, libc::SIGHUP, libc::SIGCHLD] {
        crate::sys::set_handler(sig, on_signal as *const () as usize, libc::SA_RESTART)?;
    }
    Ok(r)
}

/// Looks up `user`'s uid and gid.
pub fn lookup_user(user: &str) -> io::Result<(u32, u32)> {
    crate::sys::lookup_account(user).map(|a| (a.uid, a.gid))
}

/// Creates `runtime_dir` (0755) and binds a fresh greeter socket in it, owned by the greeter
/// and mode 0600.
pub fn bind_socket(runtime_dir: &Path, uid: u32, gid: u32) -> io::Result<(UnixListener, PathBuf)> {
    fs::create_dir_all(runtime_dir)?;
    fs::set_permissions(runtime_dir, fs::Permissions::from_mode(0o755))?;
    let path = runtime_dir.join(format!("greetd-{}.sock", std::process::id()));
    let _ = fs::remove_file(&path);
    let listener = UnixListener::bind(&path)?;
    std::os::unix::fs::chown(&path, Some(uid), Some(gid))?;
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
    listener.set_nonblocking(true)?;
    Ok((listener, path))
}

#[allow(unsafe_code)]
fn peer_uid(s: &UnixStream) -> io::Result<u32> {
    let mut cred: libc::ucred = unsafe { std::mem::zeroed() };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    // SAFETY: SO_PEERCRED writes one ucred of `len` bytes.
    let rc = unsafe {
        libc::getsockopt(
            s.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&mut cred as *mut libc::ucred).cast(),
            &mut len,
        )
    };
    if rc < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(cred.uid)
}

struct Client {
    id: u64,
    stream: UnixStream,
}

/// Runs the daemon until SIGTERM/SIGINT/SIGHUP; then stops it in order, removes the socket and
/// returns.
///
/// Each iteration polls the signal pipe, the listener, the session channel, the logout
/// greeter's ready pipe, the worker sockets the daemon waits on and the clients; it only reads
/// bytes and hands them to the daemon. A client request is tagged with its client id; its reply
/// (immediate or after a worker reply, a ready wait or a queued start) comes out of the
/// daemon's reply slot and is written to that client.
pub fn serve<B: Backend, V: Vt>(
    mut daemon: Daemon<B, V>,
    listener: UnixListener,
    sock_path: &Path,
    greeter_uid: u32,
) -> io::Result<()> {
    let sig = signal_pipe()?;
    daemon.start(Instant::now());
    let mut clients: Vec<Client> = Vec::new();
    let mut next_id = 0u64;
    'main: loop {
        let ready_fd = daemon.ready_fd();
        let greeter_fd = daemon.greeter_ready_fd();
        let worker_fds = daemon.worker_fds();
        let mut fds: Vec<libc::pollfd> = vec![
            pfd(sig.as_raw_fd()),
            pfd(listener.as_raw_fd()),
            pfd(ready_fd.unwrap_or(-1)),
            pfd(greeter_fd.unwrap_or(-1)),
        ];
        fds.extend(worker_fds.iter().map(|&fd| pfd(fd)));
        let first_client = fds.len();
        fds.extend(clients.iter().map(|c| pfd(c.stream.as_raw_fd())));
        let timeout = daemon.next_deadline().map_or(-1, |d| {
            let ms = d.saturating_duration_since(Instant::now()).as_millis();
            i32::try_from(ms).unwrap_or(i32::MAX).saturating_add(1)
        });
        #[allow(unsafe_code)]
        // SAFETY: `fds` is a live, correctly sized pollfd array.
        let rc = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, timeout) };
        if rc < 0 {
            let e = io::Error::last_os_error();
            if e.kind() != io::ErrorKind::Interrupted {
                return Err(e);
            }
        }
        let now = Instant::now();

        if fds[0].revents != 0 {
            let mut buf = [0u8; 64];
            #[allow(unsafe_code)]
            // SAFETY: non-blocking read into a local buffer.
            let n = unsafe { libc::read(sig.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len()) };
            let got = &buf[..usize::try_from(n).unwrap_or(0)];
            if stop_requested(got) {
                tracing::info!("stopping");
                break 'main;
            }
            daemon.poll_children(now);
        }
        if fds[1].revents != 0 {
            match listener.accept() {
                Ok((stream, _)) => match peer_uid(&stream) {
                    Ok(uid) if peer_allowed(uid, greeter_uid) => {
                        let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
                        next_id += 1;
                        clients.push(Client {
                            id: next_id,
                            stream,
                        });
                    }
                    Ok(uid) => tracing::warn!("dropping greeter-socket peer uid {uid}"),
                    Err(e) => tracing::warn!("SO_PEERCRED: {e}"),
                },
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
                Err(e) => tracing::warn!("accept: {e}"),
            }
        }
        if let Some(rfd) = ready_fd
            && fds[2].revents != 0
        {
            let (got, eof) = read_pipe(rfd);
            daemon.on_session_input(&got, eof, now);
        }
        if let Some(gfd) = greeter_fd
            && fds[3].revents != 0
        {
            let (got, eof) = read_pipe(gfd);
            daemon.on_greeter_input(&got, eof);
        }
        if fds[4..first_client].iter().any(|pf| pf.revents != 0) {
            daemon.on_worker_input(now);
        }
        let mut gone = Vec::new();
        for (i, pf) in fds[first_client..].iter().enumerate() {
            if pf.revents == 0 {
                continue;
            }
            let c = &mut clients[i];
            let req: Request = match read_message(&mut c.stream) {
                Ok(r) => r,
                Err(_) => {
                    gone.push(c.id);
                    daemon.on_client_gone(c.id);
                    continue;
                }
            };
            if let Err(refusal) = daemon.handle(c.id, req, now) {
                // Busy: this client's request is refused at once.
                if write_message(&mut c.stream, &refusal).is_err() {
                    gone.push(c.id);
                }
                continue;
            }
            take_and_send(&mut daemon, &mut clients, &mut gone);
        }
        daemon.on_tick(Instant::now());
        take_and_send(&mut daemon, &mut clients, &mut gone);
        clients.retain(|c| !gone.contains(&c.id));
    }
    daemon.shutdown(STOP_GRACE);
    let _ = fs::remove_file(sock_path);
    Ok(())
}

/// Writes the daemon's pending reply (if any) to the client that asked.
fn take_and_send<B: Backend, V: Vt>(
    daemon: &mut Daemon<B, V>,
    clients: &mut [Client],
    gone: &mut Vec<u64>,
) {
    if let Some((id, resp)) = daemon.take_reply()
        && let Some(c) = clients
            .iter_mut()
            .find(|c| c.id == id && !gone.contains(&id))
        && let Err(e) = write_message(&mut c.stream, &resp)
    {
        tracing::warn!("greeter client write failed: {e}");
        gone.push(id);
    }
}

/// Whether the signal-pipe bytes carry a stop signal (SIGTERM, SIGINT or SIGHUP).
fn stop_requested(sig_bytes: &[u8]) -> bool {
    sig_bytes
        .iter()
        .any(|&s| [libc::SIGTERM, libc::SIGINT, libc::SIGHUP].contains(&i32::from(s)))
}

/// One read from a daemon-owned pipe: the bytes and whether it hit EOF (or failed).
fn read_pipe(fd: RawFd) -> (Vec<u8>, bool) {
    let mut buf = [0u8; 64];
    #[allow(unsafe_code)]
    // SAFETY: read into a local buffer from a daemon-owned pipe.
    let n = unsafe { libc::read(fd, buf.as_mut_ptr().cast(), buf.len()) };
    match usize::try_from(n) {
        Ok(n) if n > 0 => (buf[..n].to_vec(), false),
        _ => (Vec::new(), true),
    }
}

fn pfd(fd: RawFd) -> libc::pollfd {
    libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sighup_stops_like_sigterm_and_sigchld_does_not() {
        for sig in [libc::SIGTERM, libc::SIGINT, libc::SIGHUP] {
            assert!(stop_requested(&[libc::SIGCHLD as u8, sig as u8]), "{sig}");
        }
        assert!(!stop_requested(&[libc::SIGCHLD as u8]));
        assert!(!stop_requested(&[]));
    }
}
