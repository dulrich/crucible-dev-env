//! Hand-written FFI to the system libpam (no dev package: links `libpam.so.0` verbatim) and a
//! small owning wrapper. Only the per-session worker process ([`crate::worker`]) uses it; PAM
//! never runs in the daemon process.
//!
//! Secret answers: the [`Prompter`] hands each answer over as a [`Secret`] (wiped on drop), and
//! the conversation callback copies its bytes straight into the `malloc`ed `pam_response.resp`
//! that PAM owns from then on. That copy is the end of this program's responsibility: the module
//! stack owns, uses and frees the response. Not covered: `serde_json`'s internal buffers while
//! decoding a request, the greetd socket read buffer in `crucible-greetd`, and kernel socket
//! buffers.
#![allow(unsafe_code)]

use std::ffi::{CStr, CString, c_char, c_int, c_void};
use std::ptr;

use crucible_greetd::{AuthMessageType, Secret};

/// `PAM_SUCCESS`.
pub const PAM_SUCCESS: c_int = 0;
/// `PAM_CONV_ERR`.
pub const PAM_CONV_ERR: c_int = 19;
/// `PAM_BUF_ERR`.
pub const PAM_BUF_ERR: c_int = 5;
/// Item type `PAM_USER`.
pub const PAM_USER: c_int = 2;
/// Item type `PAM_TTY`.
pub const PAM_TTY: c_int = 3;
/// `pam_setcred` flag: establish credentials.
pub const PAM_ESTABLISH_CRED: c_int = 0x2;
/// `pam_setcred` flag: delete credentials.
pub const PAM_DELETE_CRED: c_int = 0x4;
/// Message style: prompt, answer not echoed.
pub const PAM_PROMPT_ECHO_OFF: c_int = 1;
/// Message style: prompt, answer echoed.
pub const PAM_PROMPT_ECHO_ON: c_int = 2;
/// Message style: error line.
pub const PAM_ERROR_MSG: c_int = 3;
/// Message style: info line.
pub const PAM_TEXT_INFO: c_int = 4;

/// `struct pam_message`.
#[repr(C)]
pub struct PamMessage {
    /// The style (`PAM_PROMPT_ECHO_OFF`, …).
    pub msg_style: c_int,
    /// The text.
    pub msg: *const c_char,
}

/// `struct pam_response`.
#[repr(C)]
pub struct PamResponse {
    /// `malloc`-allocated answer or null.
    pub resp: *mut c_char,
    /// Unused, 0.
    pub resp_retcode: c_int,
}

type ConvFn =
    extern "C" fn(c_int, *mut *const PamMessage, *mut *mut PamResponse, *mut c_void) -> c_int;

#[repr(C)]
struct PamConv {
    conv: ConvFn,
    appdata_ptr: *mut c_void,
}

#[repr(C)]
struct PamHandleT {
    _p: [u8; 0],
}

#[link(name = "libpam.so.0", kind = "dylib", modifiers = "+verbatim")]
unsafe extern "C" {
    fn pam_start(
        service: *const c_char,
        user: *const c_char,
        conv: *const PamConv,
        pamh: *mut *mut PamHandleT,
    ) -> c_int;
    fn pam_end(pamh: *mut PamHandleT, status: c_int) -> c_int;
    fn pam_authenticate(pamh: *mut PamHandleT, flags: c_int) -> c_int;
    fn pam_acct_mgmt(pamh: *mut PamHandleT, flags: c_int) -> c_int;
    fn pam_setcred(pamh: *mut PamHandleT, flags: c_int) -> c_int;
    fn pam_open_session(pamh: *mut PamHandleT, flags: c_int) -> c_int;
    fn pam_close_session(pamh: *mut PamHandleT, flags: c_int) -> c_int;
    fn pam_putenv(pamh: *mut PamHandleT, name_value: *const c_char) -> c_int;
    fn pam_getenvlist(pamh: *mut PamHandleT) -> *mut *mut c_char;
    fn pam_get_item(pamh: *const PamHandleT, item_type: c_int, item: *mut *const c_void) -> c_int;
    fn pam_set_item(pamh: *mut PamHandleT, item_type: c_int, item: *const c_void) -> c_int;
    fn pam_strerror(pamh: *mut PamHandleT, errnum: c_int) -> *const c_char;
}

/// Maps a PAM message style to the greetd message type (`None`: unknown style).
pub fn map_style(style: c_int) -> Option<AuthMessageType> {
    match style {
        PAM_PROMPT_ECHO_OFF => Some(AuthMessageType::Secret),
        PAM_PROMPT_ECHO_ON => Some(AuthMessageType::Visible),
        PAM_TEXT_INFO => Some(AuthMessageType::Info),
        PAM_ERROR_MSG => Some(AuthMessageType::Error),
        _ => None,
    }
}

/// Builds the `malloc`-allocated response array PAM takes ownership of (`None`: null `resp`).
/// Returns null on allocation failure or an answer containing NUL. Each answer is copied
/// directly into its `malloc`ed slot, with no intermediate buffer.
pub fn marshal_responses(answers: &[Option<Secret>]) -> *mut PamResponse {
    let n = answers.len().max(1);
    // SAFETY: calloc returns zeroed memory for n responses or null.
    let arr = unsafe { libc::calloc(n, std::mem::size_of::<PamResponse>()) }.cast::<PamResponse>();
    if arr.is_null() {
        return arr;
    }
    for (i, a) in answers.iter().enumerate() {
        let Some(a) = a else { continue };
        let b = a.expose().as_bytes();
        // SAFETY: malloc returns len + 1 bytes or null; arr has n slots and is the live array
        // built here.
        unsafe {
            let p = if b.contains(&0) {
                ptr::null_mut()
            } else {
                libc::malloc(b.len() + 1).cast::<c_char>()
            };
            if p.is_null() {
                free_responses(arr, answers.len());
                return ptr::null_mut();
            }
            ptr::copy_nonoverlapping(b.as_ptr().cast::<c_char>(), p, b.len());
            *p.add(b.len()) = 0;
            (*arr.add(i)).resp = p;
        }
    }
    arr
}

/// Frees a response array from [`marshal_responses`] (scrubbing answers first).
///
/// # Safety
/// `arr` is null or an array from [`marshal_responses`] with `n` entries, not yet freed.
pub unsafe fn free_responses(arr: *mut PamResponse, n: usize) {
    if arr.is_null() {
        return;
    }
    for i in 0..n {
        // SAFETY: arr holds n responses; each resp is null or a malloc'd C string.
        unsafe {
            let p = (*arr.add(i)).resp;
            if !p.is_null() {
                let len = libc::strlen(p);
                ptr::write_bytes(p, 0, len);
                libc::free(p.cast());
            }
        }
    }
    // SAFETY: arr came from calloc.
    unsafe { libc::free(arr.cast()) };
}

/// Answers one conversation message: `(type, text)` in, the answer out (`Err`: abort).
pub type Prompter = Box<dyn FnMut(AuthMessageType, String) -> Result<Option<Secret>, ()>>;

struct ConvData {
    prompter: Prompter,
}

extern "C" fn conversation(
    num_msg: c_int,
    msg: *mut *const PamMessage,
    resp: *mut *mut PamResponse,
    appdata: *mut c_void,
) -> c_int {
    if num_msg <= 0 || msg.is_null() || resp.is_null() || appdata.is_null() {
        return PAM_CONV_ERR;
    }
    // SAFETY: appdata is the ConvData registered in Pam::start, alive for the handle's life.
    let data = unsafe { &mut *appdata.cast::<ConvData>() };
    // Collected as Secrets, so every answer is wiped when this returns.
    let mut answers: Vec<Option<Secret>> = Vec::with_capacity(num_msg as usize);
    for i in 0..num_msg as usize {
        // SAFETY: Linux-PAM passes an array of num_msg message pointers.
        let m = unsafe { &**msg.add(i) };
        let text = if m.msg.is_null() {
            String::new()
        } else {
            // SAFETY: PAM message text is a NUL-terminated string.
            unsafe { CStr::from_ptr(m.msg) }
                .to_string_lossy()
                .into_owned()
        };
        let Some(kind) = map_style(m.msg_style) else {
            return PAM_CONV_ERR;
        };
        match (data.prompter)(kind, text) {
            Ok(a) => answers.push(a),
            Err(()) => return PAM_CONV_ERR,
        }
    }
    let arr = marshal_responses(&answers);
    if arr.is_null() {
        return PAM_BUF_ERR;
    }
    // SAFETY: resp is PAM's out pointer; PAM frees the array.
    unsafe { *resp = arr };
    PAM_SUCCESS
}

/// A PAM failure: the call's return code and `pam_strerror` text.
#[derive(Debug, Clone)]
pub struct PamError {
    /// The return code.
    pub code: c_int,
    /// The message.
    pub message: String,
}

/// An open PAM handle; `pam_end` runs on drop.
pub struct Pam {
    h: *mut PamHandleT,
    // Boxed so the pointer handed to PAM stays put.
    _conv: Box<PamConv>,
    _data: Box<ConvData>,
    last: c_int,
}

impl Pam {
    /// `pam_start(service, user)` with `prompter` answering the conversation.
    pub fn start(service: &str, user: &str, prompter: Prompter) -> Result<Self, PamError> {
        let err = |m: &str| PamError {
            code: -1,
            message: m.to_owned(),
        };
        let svc = CString::new(service).map_err(|_| err("bad service name"))?;
        let usr = CString::new(user).map_err(|_| err("bad user name"))?;
        let mut data = Box::new(ConvData { prompter });
        let conv = Box::new(PamConv {
            conv: conversation,
            appdata_ptr: (&mut *data as *mut ConvData).cast(),
        });
        let mut h = ptr::null_mut();
        // SAFETY: valid C strings; conv lives as long as the returned handle.
        let rc = unsafe { pam_start(svc.as_ptr(), usr.as_ptr(), &*conv, &mut h) };
        if rc != PAM_SUCCESS || h.is_null() {
            return Err(PamError {
                code: rc,
                message: format!("pam_start failed ({rc})"),
            });
        }
        Ok(Self {
            h,
            _conv: conv,
            _data: data,
            last: PAM_SUCCESS,
        })
    }

    fn check(&mut self, rc: c_int) -> Result<(), PamError> {
        self.last = rc;
        if rc == PAM_SUCCESS {
            return Ok(());
        }
        // SAFETY: pam_strerror returns a static string for a live handle.
        let s = unsafe { pam_strerror(self.h, rc) };
        let message = if s.is_null() {
            format!("PAM error {rc}")
        } else {
            // SAFETY: non-null NUL-terminated string.
            unsafe { CStr::from_ptr(s) }.to_string_lossy().into_owned()
        };
        Err(PamError { code: rc, message })
    }

    /// `pam_authenticate`.
    pub fn authenticate(&mut self) -> Result<(), PamError> {
        // SAFETY: live handle.
        let rc = unsafe { pam_authenticate(self.h, 0) };
        self.check(rc)
    }

    /// `pam_acct_mgmt`.
    pub fn acct_mgmt(&mut self) -> Result<(), PamError> {
        // SAFETY: live handle.
        let rc = unsafe { pam_acct_mgmt(self.h, 0) };
        self.check(rc)
    }

    /// `pam_setcred(flags)`.
    pub fn setcred(&mut self, flags: c_int) -> Result<(), PamError> {
        // SAFETY: live handle.
        let rc = unsafe { pam_setcred(self.h, flags) };
        self.check(rc)
    }

    /// `pam_open_session`.
    pub fn open_session(&mut self) -> Result<(), PamError> {
        // SAFETY: live handle.
        let rc = unsafe { pam_open_session(self.h, 0) };
        self.check(rc)
    }

    /// `pam_close_session`.
    pub fn close_session(&mut self) -> Result<(), PamError> {
        // SAFETY: live handle.
        let rc = unsafe { pam_close_session(self.h, 0) };
        self.check(rc)
    }

    /// `pam_putenv("KEY=VALUE")`.
    pub fn putenv(&mut self, kv: &str) -> Result<(), PamError> {
        let c = CString::new(kv).map_err(|_| PamError {
            code: -1,
            message: "NUL in env entry".into(),
        })?;
        // SAFETY: live handle, valid C string (PAM copies it).
        let rc = unsafe { pam_putenv(self.h, c.as_ptr()) };
        self.check(rc)
    }

    /// `pam_set_item(PAM_TTY, tty)`.
    pub fn set_tty(&mut self, tty: &str) -> Result<(), PamError> {
        let c = CString::new(tty).map_err(|_| PamError {
            code: -1,
            message: "NUL in tty".into(),
        })?;
        // SAFETY: live handle; PAM copies string items.
        let rc = unsafe { pam_set_item(self.h, PAM_TTY, c.as_ptr().cast()) };
        self.check(rc)
    }

    /// `pam_get_item(PAM_USER)`.
    pub fn user(&mut self) -> Result<String, PamError> {
        let mut p: *const c_void = ptr::null();
        // SAFETY: live handle; p receives a PAM-owned string.
        let rc = unsafe { pam_get_item(self.h, PAM_USER, &mut p) };
        self.check(rc)?;
        if p.is_null() {
            return Err(PamError {
                code: -1,
                message: "PAM_USER unset".into(),
            });
        }
        // SAFETY: PAM_USER is a NUL-terminated string owned by the handle.
        Ok(unsafe { CStr::from_ptr(p.cast()) }
            .to_string_lossy()
            .into_owned())
    }

    /// `pam_getenvlist` as owned `KEY=VALUE` strings.
    pub fn envlist(&mut self) -> Vec<String> {
        // SAFETY: live handle; the list and its entries are malloc'd and ours to free.
        unsafe {
            let list = pam_getenvlist(self.h);
            let mut out = Vec::new();
            if list.is_null() {
                return out;
            }
            let mut i = 0;
            loop {
                let e = *list.add(i);
                if e.is_null() {
                    break;
                }
                out.push(CStr::from_ptr(e).to_string_lossy().into_owned());
                libc::free(e.cast());
                i += 1;
            }
            libc::free(list.cast());
            out
        }
    }
}

impl Drop for Pam {
    fn drop(&mut self) {
        // SAFETY: the handle is live and ended exactly once here.
        unsafe { pam_end(self.h, self.last) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn styles_map_to_greetd_types() {
        assert_eq!(
            map_style(PAM_PROMPT_ECHO_OFF),
            Some(AuthMessageType::Secret)
        );
        assert_eq!(
            map_style(PAM_PROMPT_ECHO_ON),
            Some(AuthMessageType::Visible)
        );
        assert_eq!(map_style(PAM_TEXT_INFO), Some(AuthMessageType::Info));
        assert_eq!(map_style(PAM_ERROR_MSG), Some(AuthMessageType::Error));
        assert_eq!(map_style(99), None);
    }

    #[test]
    fn responses_are_malloced_copies_or_null() {
        let answers = vec![Some(Secret::new("hunter 2")), None, Some(Secret::new(""))];
        let arr = marshal_responses(&answers);
        assert!(!arr.is_null());
        // SAFETY: arr holds three responses built above.
        unsafe {
            assert_eq!(CStr::from_ptr((*arr).resp).to_str().unwrap(), "hunter 2");
            assert!((*arr.add(1)).resp.is_null());
            assert_eq!(CStr::from_ptr((*arr.add(2)).resp).to_str().unwrap(), "");
            assert_eq!((*arr.add(1)).resp_retcode, 0);
        }
        // SAFETY: arr is the live array built here.
        unsafe { free_responses(arr, answers.len()) };
        assert!(marshal_responses(&[Some(Secret::new("a\0b"))]).is_null());
    }
}
