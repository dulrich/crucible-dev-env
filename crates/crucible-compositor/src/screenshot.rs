//! Screenshot delivery (plan 34 WP-2): a [`Capture`] goes to the clipboard as an `image/png`
//! selection, to a `cde-<YYYYmmdd-HHMMSS>.png` file, or both, per `screenshot { destination }`.
//!
//! The PNG encode and the file write run on a worker thread; the result comes back through a
//! calloop channel because `set_data_device_selection` must run on the main thread. The channel's
//! sender lives only in the worker, so the source removes itself when the worker ends (calloop
//! answers a disconnected channel with `PostAction::Remove`): no source leaks per screenshot.
//! Every failure logs at `warn` and does nothing else.

use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

use crucible_config::ScreenshotDestination;
use smithay::{
    reexports::calloop::channel::{Event, channel},
    wayland::selection::{SelectionTarget, data_device::set_data_device_selection},
};
use tracing::{info, warn};

use crate::{
    capture::{Capture, encode_png},
    selection::{IMAGE_MIME, SelectionData, claim_x11_selection},
    state::{Backend, CrucibleState},
};

/// A broken-down local time, the fields the file name uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Stamp {
    pub year: i32,
    pub month: u32,
    pub day: u32,
    pub hour: u32,
    pub minute: u32,
    pub second: u32,
}

impl Stamp {
    /// The current local time (`libc::localtime_r`), or `None` if the C library refuses.
    pub(crate) fn now_local() -> Option<Self> {
        // SAFETY: `time` accepts a null out-pointer; `localtime_r` writes only into `tm`, which
        // is a valid zeroed `libc::tm` owned by this frame.
        #[allow(unsafe_code)]
        let tm = unsafe {
            let secs = libc::time(std::ptr::null_mut());
            let mut tm: libc::tm = std::mem::zeroed();
            if libc::localtime_r(&secs, &mut tm).is_null() {
                return None;
            }
            tm
        };
        Some(Self {
            year: tm.tm_year + 1900,
            month: u32::try_from(tm.tm_mon + 1).ok()?,
            day: u32::try_from(tm.tm_mday).ok()?,
            hour: u32::try_from(tm.tm_hour).ok()?,
            minute: u32::try_from(tm.tm_min).ok()?,
            second: u32::try_from(tm.tm_sec).ok()?,
        })
    }
}

/// `cde-YYYYmmdd-HHMMSS` (no extension).
#[must_use]
pub(crate) fn file_stem(t: Stamp) -> String {
    format!(
        "cde-{:04}{:02}{:02}-{:02}{:02}{:02}",
        t.year, t.month, t.day, t.hour, t.minute, t.second
    )
}

/// The first free `<stem>.png`, `<stem>-1.png`, `<stem>-2.png`, ... given `exists`.
#[must_use]
pub(crate) fn unique_file_name(stem: &str, exists: impl Fn(&str) -> bool) -> String {
    let first = format!("{stem}.png");
    if !exists(&first) {
        return first;
    }
    (1u32..)
        .map(|n| format!("{stem}-{n}.png"))
        .find(|name| !exists(name))
        .expect("an unbounded counter finds a free name")
}

/// Expands a leading `~` (alone or `~/...`) to `home`; any other path is returned unchanged. With
/// no `home` a `~` path is left as written (the write then fails and logs).
#[must_use]
pub(crate) fn expand_tilde(path: &Path, home: Option<&str>) -> PathBuf {
    match (path.strip_prefix("~"), home) {
        (Ok(rest), Some(home)) if !home.is_empty() => Path::new(home).join(rest),
        _ => path.to_path_buf(),
    }
}

/// Creates `dir` and writes `png` to the first free `cde-<stamp>.png` in it (never overwriting).
fn write_screenshot_file(dir: &Path, stamp: Stamp, png: &[u8]) -> Result<PathBuf, String> {
    use std::io::Write;
    std::fs::create_dir_all(dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    let stem = file_stem(stamp);
    // `create_new` makes the existence check and the create one atomic step; retry on a race.
    let mut name = unique_file_name(&stem, |n| dir.join(n).exists());
    for attempt in 1u32..=100 {
        let path = dir.join(&name);
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(mut file) => {
                return file
                    .write_all(png)
                    .map(|()| path.clone())
                    .map_err(|e| format!("cannot write {}: {e}", path.display()));
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                name = format!("{stem}-{attempt}.png");
            }
            Err(e) => return Err(format!("cannot create {}: {e}", path.display())),
        }
    }
    Err(format!("no free file name for {stem} in {}", dir.display()))
}

/// What the worker hands back to the main thread.
struct Delivery {
    png: Result<Arc<[u8]>, String>,
    file: Option<Result<PathBuf, String>>,
}

impl<B: Backend + 'static> CrucibleState<B> {
    /// Delivers `capture` per `config.screenshot.destination` (clipboard, file or both) with one
    /// encode. Independent of how the capture was produced (WP-4 passes a cropped one). Returns at
    /// once; the encode and file write run on a worker and failures only log.
    pub(crate) fn deliver_screenshot(&mut self, capture: Capture) {
        let destination = self.config.screenshot.destination;
        let want_clipboard = destination != ScreenshotDestination::File;
        let want_file = destination != ScreenshotDestination::Clipboard;
        let dir = expand_tilde(
            &self.config.screenshot.directory,
            std::env::var("HOME").ok().as_deref(),
        );
        let (tx, rx) = channel::<Delivery>();
        let started = std::time::Instant::now();
        let worker = std::thread::Builder::new()
            .name("screenshot".into())
            .spawn(move || {
                let png: Result<Arc<[u8]>, String> = encode_png(&capture)
                    .map(Arc::from)
                    .map_err(|e| e.to_string());
                let file = match (&png, want_file) {
                    (Ok(png), true) => Some(
                        Stamp::now_local()
                            .ok_or_else(|| "cannot read the local time".to_string())
                            .and_then(|stamp| write_screenshot_file(&dir, stamp, png)),
                    ),
                    _ => None,
                };
                // A send error means the compositor is shutting down.
                let _ = tx.send(Delivery { png, file });
            });
        if let Err(err) = worker {
            warn!(?err, "screenshot: cannot spawn the encoder thread");
            return;
        }
        let inserted = self.handle.insert_source(rx, move |event, (), state| {
            if let Event::Msg(delivery) = event {
                state.finish_screenshot(delivery, want_clipboard, started);
            }
            // `Event::Closed` follows the worker's exit; calloop removes the source then.
        });
        if let Err(err) = inserted {
            warn!(%err, "screenshot: cannot register the delivery channel");
        }
    }

    /// Main-thread half of [`Self::deliver_screenshot`]: logs the file result and publishes the
    /// clipboard image.
    fn finish_screenshot(
        &mut self,
        delivery: Delivery,
        want_clipboard: bool,
        started: std::time::Instant,
    ) {
        let png = match delivery.png {
            Ok(png) => png,
            Err(err) => {
                warn!(%err, "screenshot: PNG encode failed");
                return;
            }
        };
        let ms = started.elapsed().as_secs_f64() * 1000.0;
        match delivery.file {
            Some(Ok(path)) => info!(
                destination = "file",
                path = %path.display(),
                bytes = png.len(),
                ms,
                "screenshot saved"
            ),
            Some(Err(err)) => warn!(%err, "screenshot: file write failed"),
            None => {}
        }
        if want_clipboard {
            self.publish_image_selection(png, ms);
        }
    }

    /// Makes `png` the Wayland clipboard selection and claims the X11 clipboard for XWayland.
    fn publish_image_selection(&mut self, png: Arc<[u8]>, ms: f64) {
        if self.session_lock.is_locked() {
            warn!("screenshot: session locked; clipboard not set");
            return;
        }
        let bytes = png.len();
        set_data_device_selection(
            &self.display_handle,
            &self.seat,
            vec![IMAGE_MIME.to_string()],
            SelectionData::Image(png),
        );
        // `set_data_device_selection` does not fire `SelectionHandler::new_selection`.
        if let Some(xwm) = self.xwm.as_mut() {
            claim_x11_selection(
                xwm,
                SelectionTarget::Clipboard,
                Some(vec![IMAGE_MIME.into()]),
            );
        }
        info!(
            destination = "clipboard",
            mime = IMAGE_MIME,
            bytes,
            ms,
            "screenshot copied"
        );
    }

    /// The `screenshot` action: captures the focused output, then delivers it.
    ///
    /// # Errors
    /// The refusal reason (locked, no output, powered off, render failure).
    pub(crate) fn screenshot_focused_output(&mut self) -> Result<(), String> {
        let output = self
            .focused_smithay_output()
            .ok_or_else(|| crate::capture::CaptureError::NoOutput.to_string())?;
        let capture = self.capture_output(&output).map_err(|e| e.to_string())?;
        self.deliver_screenshot(capture);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const T: Stamp = Stamp {
        year: 2026,
        month: 11,
        day: 6,
        hour: 14,
        minute: 25,
        second: 30,
    };

    #[test]
    fn file_stem_is_zero_padded_local_time() {
        assert_eq!(file_stem(T), "cde-20261106-142530");
        let early = Stamp {
            month: 1,
            day: 2,
            hour: 3,
            minute: 4,
            second: 5,
            ..T
        };
        assert_eq!(file_stem(early), "cde-20260102-030405");
    }

    #[test]
    fn collisions_append_an_increasing_suffix() {
        let stem = file_stem(T);
        let taken = |set: &'static [&'static str]| move |n: &str| set.contains(&n);
        assert_eq!(
            unique_file_name(&stem, taken(&[])),
            "cde-20261106-142530.png"
        );
        assert_eq!(
            unique_file_name(&stem, taken(&["cde-20261106-142530.png"])),
            "cde-20261106-142530-1.png"
        );
        assert_eq!(
            unique_file_name(
                &stem,
                taken(&["cde-20261106-142530.png", "cde-20261106-142530-1.png"])
            ),
            "cde-20261106-142530-2.png"
        );
    }

    #[test]
    fn tilde_expands_only_as_a_leading_component() {
        let h = Some("/home/u");
        assert_eq!(
            expand_tilde(Path::new("~/Pictures/Screenshots"), h),
            Path::new("/home/u/Pictures/Screenshots")
        );
        assert_eq!(expand_tilde(Path::new("~"), h), Path::new("/home/u"));
        assert_eq!(
            expand_tilde(Path::new("/abs/~/x"), h),
            Path::new("/abs/~/x")
        );
        assert_eq!(
            expand_tilde(Path::new("~other/x"), h),
            Path::new("~other/x")
        );
        assert_eq!(expand_tilde(Path::new("~/x"), None), Path::new("~/x"));
    }

    #[test]
    fn writes_into_a_created_directory_and_never_overwrites() {
        let base = std::env::temp_dir().join(format!("cde-shot-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let dir = base.join("a/b");
        let first = write_screenshot_file(&dir, T, b"one").unwrap();
        let second = write_screenshot_file(&dir, T, b"two").unwrap();
        assert_eq!(first, dir.join("cde-20261106-142530.png"));
        assert_eq!(second, dir.join("cde-20261106-142530-1.png"));
        assert_eq!(std::fs::read(&first).unwrap(), b"one");
        assert_eq!(std::fs::read(&second).unwrap(), b"two");
        std::fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn local_time_is_available() {
        let now = Stamp::now_local().expect("localtime_r");
        assert!(now.year >= 2026 && (1..=12).contains(&now.month));
    }
}
