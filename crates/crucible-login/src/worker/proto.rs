//! The daemon-to-worker wire: [`Req`], [`Rep`], one `serde_json` document per `SOCK_SEQPACKET`
//! datagram, and the socket I/O with an optional fd over `SCM_RIGHTS` on the same datagram.
//! A message that does not decode is an error, and a message over [`MAX_MSG`] is refused by
//! [`send`] and detected (never silently truncated) by [`recv`].
#![allow(unsafe_code)]

use std::io;
use std::os::fd::{FromRawFd, OwnedFd, RawFd};

use crucible_greetd::codec::MAX_MESSAGE_LEN;
use crucible_greetd::{AuthMessageType, Secret};
use serde::{Deserialize, Serialize};

use crate::sys::{cvt, retry_eintr};

/// The datagram budget. The largest request is a `Spawn`: its `cmd` and `env` come from one
/// `start_session` frame (at most [`MAX_MESSAGE_LEN`] together); a `Step` answer comes from one
/// `post_auth_message_response` frame (at most [`MAX_MESSAGE_LEN`]). Those strings were decoded
/// from JSON, and `serde_json` re-encodes each with the shortest escape, which is never longer
/// than the escape the greetd peer had to send, so each frame's strings re-encode to at most its
/// own length. On top come the daemon's ready-fd env entry (`CDE_LOGIN_READY_FD=3`, ~25 bytes),
/// the variant and field names and numbers (~100 bytes): 4 KiB covers that with room to spare.
/// The second [`MAX_MESSAGE_LEN`] dates from when `Spawn` also carried the account name; it is
/// kept so this refactor leaves the budget (and its boundary tests) unchanged, as headroom for
/// one more frame-sized field. 132 KiB stays well under the default unix socket send buffer
/// (`net.core.wmem_default`, 208 KiB on Debian), so no `SO_SNDBUF` tuning is needed.
pub const MAX_MSG: usize = 2 * MAX_MESSAGE_LEN + 4 * 1024;

/// Daemon → worker.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub enum Req {
    /// Advance the conversation (answer to the previous prompt; the first one starts PAM).
    /// The answer stays a [`Secret`] (serialized transparently, so the bytes are those of a
    /// plain string) and wipes itself when dropped.
    Step(Option<Secret>),
    /// Open the session (the class came with the worker's fork, the account from PAM).
    Open {
        /// VT number: `PAM_TTY`, the user process's tty and `XDG_VTNR`.
        vt: u32,
        /// Extra PAM env entries.
        env: Vec<String>,
    },
    /// Spawn the user process; `target_fd` is where the passed fd lands (if one is passed).
    Spawn {
        /// Command words.
        cmd: Vec<String>,
        /// Extra env entries.
        env: Vec<String>,
        /// Target fd number for the `SCM_RIGHTS` fd.
        target_fd: Option<i32>,
    },
    /// Close PAM and exit.
    Close,
}

/// Worker → daemon.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub enum Rep {
    /// A conversation message (the kind uses the greetd wire spelling).
    Prompt(AuthMessageType, String),
    /// Auth + account check passed; `PAM_USER`.
    Authenticated(String),
    /// Authentication failed.
    AuthError(String),
    /// Any other failure.
    Error(String),
    /// The request succeeded.
    Ok,
    /// The user process was started.
    Spawned(i32),
}

fn to_json(v: &impl Serialize) -> Vec<u8> {
    // These types hold only strings, integers and unit variants: serialization cannot fail.
    serde_json::to_vec(v).expect("serializing a wire message")
}

impl Req {
    /// Encodes the request as one JSON document.
    pub fn encode(&self) -> Vec<u8> {
        to_json(self)
    }

    /// Decodes a request; anything but exactly one valid document is an error.
    pub fn decode(b: &[u8]) -> Result<Self, String> {
        serde_json::from_slice(b).map_err(|e| format!("bad request: {e}"))
    }
}

impl Rep {
    /// Encodes the reply as one JSON document.
    pub fn encode(&self) -> Vec<u8> {
        to_json(self)
    }

    /// Decodes a reply; anything but exactly one valid document is an error.
    pub fn decode(b: &[u8]) -> Result<Self, String> {
        serde_json::from_slice(b).map_err(|e| format!("bad reply: {e}"))
    }
}

// ---------------------------------------------------------------------------------------------
// Socket I/O with optional SCM_RIGHTS.

/// Sends one datagram (an empty one would read as EOF, and a document is never empty), with
/// `fd` attached; refuses `data` over [`MAX_MSG`] before calling `sendmsg`.
pub(super) fn send(sock: RawFd, data: &[u8], fd: Option<RawFd>) -> io::Result<()> {
    if data.len() > MAX_MSG {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("message of {} bytes exceeds {MAX_MSG}", data.len()),
        ));
    }
    let mut iov = libc::iovec {
        iov_base: data.as_ptr() as *mut _,
        iov_len: data.len(),
    };
    // SAFETY: CMSG_SPACE is a pure size computation.
    let space = unsafe { libc::CMSG_SPACE(std::mem::size_of::<RawFd>() as u32) } as usize;
    let mut cbuf = vec![0u8; space];
    // SAFETY: zeroed msghdr is valid; pointers reference live locals.
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    if let Some(fd) = fd {
        msg.msg_control = cbuf.as_mut_ptr().cast();
        msg.msg_controllen = space as _;
        // SAFETY: the control buffer is CMSG_SPACE(sizeof fd) bytes.
        unsafe {
            let c = libc::CMSG_FIRSTHDR(&msg);
            (*c).cmsg_level = libc::SOL_SOCKET;
            (*c).cmsg_type = libc::SCM_RIGHTS;
            (*c).cmsg_len = libc::CMSG_LEN(std::mem::size_of::<RawFd>() as u32) as _;
            std::ptr::write_unaligned(libc::CMSG_DATA(c).cast::<RawFd>(), fd);
        }
    }
    // SAFETY: msg is fully initialised.
    retry_eintr(|| cvt(unsafe { libc::sendmsg(sock, &msg, libc::MSG_NOSIGNAL) })).map(|_| ())
}

/// Receives one message (`None`: peer closed) and an optional passed fd (close-on-exec). A
/// datagram or control message the buffers could not hold is an error.
pub(super) fn recv(sock: RawFd) -> io::Result<Option<(Vec<u8>, Option<OwnedFd>)>> {
    recv_flags(sock, 0)
}

/// [`recv`] with extra `recvmsg` flags (`MSG_DONTWAIT` for the daemon's non-blocking receive:
/// an empty socket is `ErrorKind::WouldBlock`).
pub(super) fn recv_flags(
    sock: RawFd,
    flags: libc::c_int,
) -> io::Result<Option<(Vec<u8>, Option<OwnedFd>)>> {
    // One byte more than the budget, so an over-budget datagram also shows as truncated.
    let mut buf = vec![0u8; MAX_MSG + 1];
    let mut iov = libc::iovec {
        iov_base: buf.as_mut_ptr().cast(),
        iov_len: buf.len(),
    };
    // SAFETY: pure size computation.
    let space = unsafe { libc::CMSG_SPACE(std::mem::size_of::<RawFd>() as u32) } as usize;
    let mut cbuf = vec![0u8; space];
    // SAFETY: zeroed msghdr is valid.
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = cbuf.as_mut_ptr().cast();
    msg.msg_controllen = space as _;
    // SAFETY: msg points at live buffers.
    let n = retry_eintr(|| {
        cvt(unsafe { libc::recvmsg(sock, &mut msg, libc::MSG_CMSG_CLOEXEC | flags) })
    })? as usize;
    let mut fd = None;
    // SAFETY: walking the control buffer the kernel filled.
    unsafe {
        let c = libc::CMSG_FIRSTHDR(&msg);
        if !c.is_null() && (*c).cmsg_level == libc::SOL_SOCKET && (*c).cmsg_type == libc::SCM_RIGHTS
        {
            let raw = std::ptr::read_unaligned(libc::CMSG_DATA(c).cast::<RawFd>());
            fd = Some(OwnedFd::from_raw_fd(raw));
        }
    }
    if msg.msg_flags & libc::MSG_CTRUNC != 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "control data truncated",
        ));
    }
    if msg.msg_flags & libc::MSG_TRUNC != 0 || n > MAX_MSG {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("message exceeds {MAX_MSG} bytes"),
        ));
    }
    if n == 0 && fd.is_none() {
        return Ok(None);
    }
    buf.truncate(n);
    Ok(Some((buf, fd)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::fd::AsRawFd;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| (*x).to_owned()).collect()
    }

    fn pair() -> (OwnedFd, OwnedFd) {
        let mut fds = [0 as libc::c_int; 2];
        // SAFETY: socketpair fills two fds.
        let rc = unsafe {
            libc::socketpair(
                libc::AF_UNIX,
                libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC,
                0,
                fds.as_mut_ptr(),
            )
        };
        assert_eq!(rc, 0);
        // SAFETY: both fds are fresh and ours.
        unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) }
    }

    fn spawn_req(cmd: Vec<String>, env: Vec<String>) -> Req {
        Req::Spawn {
            cmd,
            env,
            target_fd: Some(3),
        }
    }

    #[test]
    fn requests_round_trip() {
        for r in [
            Req::Step(None),
            Req::Step(Some(Secret::new("pass word"))),
            Req::Open {
                vt: 8,
                env: s(&["A=1"]),
            },
            Req::Open { vt: 7, env: vec![] },
            spawn_req(
                s(&["cde", "a b", "q\"\\\n\u{1}é"]),
                s(&["CDE_LOGIN_READY_FD=3"]),
            ),
            Req::Spawn {
                cmd: s(&["x"]),
                env: vec![],
                target_fd: None,
            },
            Req::Close,
        ] {
            assert_eq!(Req::decode(&r.encode()).unwrap(), r);
        }
        // A Secret answer is the same bytes a plain string was.
        assert_eq!(
            Req::Step(Some(Secret::new("pw"))).encode(),
            br#"{"Step":"pw"}"#
        );
    }

    #[test]
    fn replies_round_trip() {
        for k in [
            AuthMessageType::Visible,
            AuthMessageType::Secret,
            AuthMessageType::Info,
            AuthMessageType::Error,
        ] {
            let r = Rep::Prompt(k, "Password: ".into());
            assert_eq!(Rep::decode(&r.encode()).unwrap(), r);
        }
        for r in [
            Rep::Authenticated("alice".into()),
            Rep::AuthError("Authentication failure".into()),
            Rep::Error("x".into()),
            Rep::Ok,
            Rep::Spawned(4242),
        ] {
            assert_eq!(Rep::decode(&r.encode()).unwrap(), r);
        }
    }

    #[test]
    fn undecodable_messages_are_errors() {
        for b in [
            &b""[..],
            b"\x09",
            b"garbage",
            br#""Reboot""#,
            br#"{"Spawned":"x"}"#,
            br#"{"Open":{"vt":8,"env":[],"extra":1}}"#,
            br#"{"Open":{"vt":8,"class":"User","env":[]}}"#,
            br#"{"Open":{"env":[]}}"#,
        ] {
            assert!(Req::decode(b).is_err(), "{b:?}");
            assert!(Rep::decode(b).is_err(), "{b:?}");
        }
        assert!(Rep::decode(br#"{"Prompt":["whisper","x"]}"#).is_err());
        let mut b = Rep::Ok.encode();
        b.push(b'0');
        assert!(Rep::decode(&b).is_err());
        let mut b = Req::Close.encode();
        b.extend_from_slice(b" \"Close\"");
        assert!(Req::decode(&b).is_err());
    }

    #[test]
    fn maximal_greetd_request_fits_the_budget() {
        // A `start_session` frame filled to the greetd cap with words full of characters JSON
        // escapes, plus the daemon's ready-fd entry.
        let word = "a\"b\\c\nd\u{1}e".repeat(64);
        let frame = |cmd: &[String], env: &[String]| {
            serde_json::to_vec(&serde_json::json!({
                "type": "start_session", "cmd": cmd, "env": env,
            }))
            .unwrap()
            .len()
        };
        let (mut cmd, mut env) = (Vec::new(), Vec::new());
        loop {
            cmd.push(word.clone());
            env.push(format!("K={word}"));
            if frame(&cmd, &env) > MAX_MESSAGE_LEN {
                cmd.pop();
                env.pop();
                break;
            }
        }
        assert!(frame(&cmd, &env) > MAX_MESSAGE_LEN - 4 * 1024);
        env.push("CDE_LOGIN_READY_FD=3".into());
        let r = spawn_req(cmd, env);
        let len = r.encode().len();
        assert!(len <= MAX_MSG, "{len} > {MAX_MSG}");
    }

    #[test]
    fn datagram_at_the_budget_passes_with_its_fd_and_one_over_is_refused() {
        let (a, b) = pair();
        let base = spawn_req(vec![String::new()], vec![]).encode().len();
        let r = spawn_req(vec!["x".repeat(MAX_MSG - base)], vec![]);
        let data = r.encode();
        assert_eq!(data.len(), MAX_MSG);
        let (pr, _pw) = crate::sys::pipe_cloexec(0).unwrap();
        send(a.as_raw_fd(), &data, Some(pr.as_raw_fd())).unwrap();
        let (got, fd) = recv(b.as_raw_fd()).unwrap().unwrap();
        assert_eq!(Req::decode(&got).unwrap(), r);
        assert!(fd.is_some());

        let over = spawn_req(vec!["x".repeat(MAX_MSG - base + 1)], vec![]).encode();
        assert_eq!(over.len(), MAX_MSG + 1);
        let e = send(a.as_raw_fd(), &over, None).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::InvalidInput);
    }

    #[test]
    fn oversized_datagram_is_an_error_not_a_truncation() {
        let (a, b) = pair();
        let big = vec![b'x'; MAX_MSG + 2];
        // Bypass `send`'s check to model a peer that ignores the budget.
        // SAFETY: plain send(2) of a live buffer.
        let n = unsafe { libc::send(a.as_raw_fd(), big.as_ptr().cast(), big.len(), 0) };
        assert_eq!(n as usize, big.len());
        assert_eq!(
            recv(b.as_raw_fd()).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
    }
}
