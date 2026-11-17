//! Virtual terminal control. The daemon only ever puts a VT into graphics mode, switches VTs and
//! reads the active VT: it never sets `KD_TEXT`, never clears and never writes to a VT.

use std::fs::{File, OpenOptions};
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;

/// VT operations the daemon needs; faked in tests.
pub trait Vt {
    /// Puts `vt` into `KD_GRAPHICS` so the kernel never draws its console there.
    fn set_graphics(&mut self, vt: u32) -> io::Result<()>;
    /// Switches to `vt` and waits until it is active.
    fn activate(&mut self, vt: u32) -> io::Result<()>;
    /// The active VT number.
    fn active(&mut self) -> io::Result<u32>;
}

// ioctl numbers and values, from the Linux UAPI headers:
// linux/kd.h:  #define KDSETMODE 0x4B3A ; #define KD_GRAPHICS 0x01
// linux/vt.h:  #define VT_GETSTATE 0x5603 ; #define VT_ACTIVATE 0x5606 ;
//              #define VT_WAITACTIVE 0x5607
//              struct vt_stat { unsigned short v_active, v_signal, v_state; };
const KDSETMODE: libc::c_ulong = 0x4B3A;
const KD_GRAPHICS: libc::c_ulong = 0x01;
const VT_GETSTATE: libc::c_ulong = 0x5603;
const VT_ACTIVATE: libc::c_ulong = 0x5606;
const VT_WAITACTIVE: libc::c_ulong = 0x5607;

#[repr(C)]
#[derive(Default)]
struct VtStat {
    v_active: libc::c_ushort,
    v_signal: libc::c_ushort,
    v_state: libc::c_ushort,
}

/// The real VT implementation (root only; never exercised by the tests).
#[derive(Debug, Default)]
pub struct RealVt;

fn open_tty(vt: u32) -> io::Result<File> {
    OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_NOCTTY | libc::O_CLOEXEC)
        .open(format!("/dev/tty{vt}"))
}

fn check(rc: libc::c_int) -> io::Result<()> {
    if rc < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[allow(unsafe_code)]
impl Vt for RealVt {
    fn set_graphics(&mut self, vt: u32) -> io::Result<()> {
        let tty = open_tty(vt)?;
        // SAFETY: KDSETMODE takes an integer argument; the fd is open for the call.
        check(unsafe { libc::ioctl(tty.as_raw_fd(), KDSETMODE as _, KD_GRAPHICS) })
    }

    fn activate(&mut self, vt: u32) -> io::Result<()> {
        let tty = open_tty(vt)?;
        let n = libc::c_ulong::from(vt);
        // SAFETY: VT_ACTIVATE / VT_WAITACTIVE take an integer VT number.
        check(unsafe { libc::ioctl(tty.as_raw_fd(), VT_ACTIVATE as _, n) })?;
        loop {
            // SAFETY: as above.
            match check(unsafe { libc::ioctl(tty.as_raw_fd(), VT_WAITACTIVE as _, n) }) {
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                other => return other,
            }
        }
    }

    fn active(&mut self) -> io::Result<u32> {
        let tty = open_tty(0)?;
        let mut st = VtStat::default();
        // SAFETY: VT_GETSTATE writes one `struct vt_stat` through the pointer.
        check(unsafe { libc::ioctl(tty.as_raw_fd(), VT_GETSTATE as _, &mut st as *mut VtStat) })?;
        let _ = (st.v_signal, st.v_state);
        Ok(u32::from(st.v_active))
    }
}
