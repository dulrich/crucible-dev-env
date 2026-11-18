//! Built-in screenshots: the capture core (plan 34 WP-1).
//!
//! [`render_capture`] renders one output's [`frame_elements`] (built with the cursor and drag icon
//! suppressed) into a fresh offscreen buffer at the output's physical size, in the output's
//! logical orientation, with the same renderer the output renders with, and reads it back as
//! top-down RGBA. Each backend wires it through [`Backend::capture_output`]; it is refused while
//! the session is locked or the output is powered off ([`CrucibleState::capture_output`]).
//!
//! [`encode_png`] turns a [`Capture`] into PNG bytes (fast compression); callers run it off the
//! main thread.

use std::path::{Path, PathBuf};

use smithay::{
    backend::{
        allocator::Fourcc,
        renderer::{
            Bind, ExportMem, ImportAll, ImportMem, Offscreen, Renderer, TextureMapping,
            damage::OutputDamageTracker,
        },
    },
    output::Output,
    reexports::calloop::channel::{Channel, Sender, channel},
    utils::{Rectangle, Size, Transform},
};

use crate::{
    cursor::Cursor,
    ipc::Response,
    render::{FrameView, frame_elements},
    state::{Backend, CrucibleState},
    text::HeaderFonts,
};

/// One captured output frame: top-down, tightly packed RGBA rows (`stride == width * 4`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Capture {
    /// Width in physical pixels.
    pub width: u32,
    /// Height in physical pixels.
    pub height: u32,
    /// Bytes per row.
    pub stride: u32,
    /// The pixels, `stride * height` bytes, `[r, g, b, a]` per pixel (alpha forced opaque).
    pub rgba: Vec<u8>,
}

/// Why a capture failed.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum CaptureError {
    /// The session is locked (or locking): nothing is captured.
    #[error("session locked")]
    Locked,
    /// The output is powered off by idle blanking.
    #[error("output powered off")]
    PoweredOff,
    /// There is no output to capture (none is focused, or it has no mode).
    #[error("no output to capture")]
    NoOutput,
    /// This backend cannot capture.
    #[error("capture unsupported by this backend")]
    Unsupported,
    /// The offscreen render or the readback failed.
    #[error("capture render failed: {0}")]
    Render(String),
}

/// Why PNG encoding failed.
#[derive(Debug, thiserror::Error)]
pub enum EncodeError {
    /// The capture's buffer does not match its size.
    #[error("capture buffer is {got} bytes, expected {want}")]
    Size {
        /// Actual length.
        got: usize,
        /// `stride * height`.
        want: usize,
    },
    /// The PNG encoder failed.
    #[error("png: {0}")]
    Png(#[from] png::EncodingError),
}

fn render_err(e: impl std::fmt::Display) -> CaptureError {
    CaptureError::Render(e.to_string())
}

/// Renders `view`'s output into an offscreen `T` buffer with `renderer` and reads it back.
///
/// The caller suppresses the cursor (`view.inputs.cursor == None`) and checks the lock; the lock
/// branch of [`frame_elements`] would otherwise still apply.
///
/// # Errors
/// [`CaptureError::NoOutput`] when the output has no mode; [`CaptureError::Render`] when the
/// buffer, the render or the readback fails.
pub fn render_capture<R, T>(
    renderer: &mut R,
    view: FrameView<'_>,
    header_text: &mut HeaderFonts,
    cursor: &mut Cursor,
) -> Result<Capture, CaptureError>
where
    R: Renderer + ImportAll + ImportMem + Offscreen<T> + Bind<T> + ExportMem,
    R::TextureId: Send + Clone + 'static,
{
    let output = view.output;
    let mode = output.current_mode().ok_or(CaptureError::NoOutput)?;
    // Render in the logical orientation (what the user sees), at physical pixels.
    let size = output.current_transform().transform_size(mode.size);
    let scale = view.inputs.scale;
    if size.w <= 0 || size.h <= 0 {
        return Err(CaptureError::NoOutput);
    }
    let (elements, clear_color) = frame_elements(view, header_text, cursor, renderer);
    let mut buffer: T = renderer
        .create_buffer(Fourcc::Abgr8888, Size::from((size.w, size.h)))
        .map_err(render_err)?;
    let mut fb = renderer.bind(&mut buffer).map_err(render_err)?;
    let mut tracker = OutputDamageTracker::new(size, scale, Transform::Normal);
    tracker
        .render_output(renderer, &mut fb, 0, &elements, clear_color)
        .map_err(|e| render_err(format!("{e:?}")))?;
    let mapping = renderer
        .copy_framebuffer(
            &fb,
            Rectangle::from_size(Size::from((size.w, size.h))),
            Fourcc::Abgr8888,
        )
        .map_err(render_err)?;
    // `flipped()` means top-left origin (rows already top-down); otherwise GL rows are bottom-up.
    // Verified under nested winit: an unflipped GLES mapping reads back upside down.
    let bottom_up = !mapping.flipped();
    let bytes = renderer.map_texture(&mapping).map_err(render_err)?;
    #[allow(clippy::cast_sign_loss)]
    let (width, height) = (size.w as u32, size.h as u32);
    Ok(pack_rows(bytes, width, height, bottom_up))
}

/// Copies `bytes` (bottom-up rows when `bottom_up`) into a top-down [`Capture`] with opaque alpha.
#[must_use]
pub fn pack_rows(bytes: &[u8], width: u32, height: u32, bottom_up: bool) -> Capture {
    let stride = width as usize * 4;
    let mut rgba = Vec::with_capacity(stride * height as usize);
    for row in 0..height as usize {
        let src = if bottom_up {
            height as usize - 1 - row
        } else {
            row
        };
        let line = bytes.get(src * stride..(src + 1) * stride).unwrap_or(&[]);
        rgba.extend_from_slice(line);
        rgba.resize((row + 1) * stride, 0);
    }
    for px in rgba.chunks_exact_mut(4) {
        px[3] = 255;
    }
    Capture {
        width,
        height,
        stride: width * 4,
        rgba,
    }
}

/// Encodes `capture` as an 8-bit RGBA PNG with fast compression.
///
/// # Errors
/// [`EncodeError::Size`] when the buffer does not match the size; [`EncodeError::Png`] on an
/// encoder failure.
pub fn encode_png(capture: &Capture) -> Result<Vec<u8>, EncodeError> {
    let want = capture.stride as usize * capture.height as usize;
    if capture.rgba.len() != want || capture.stride != capture.width * 4 {
        return Err(EncodeError::Size {
            got: capture.rgba.len(),
            want,
        });
    }
    let mut out = Vec::new();
    {
        let mut enc = png::Encoder::new(&mut out, capture.width, capture.height);
        enc.set_color(png::ColorType::Rgba);
        enc.set_depth(png::BitDepth::Eight);
        enc.set_compression(png::Compression::Fast);
        let mut writer = enc.write_header()?;
        writer.write_image_data(&capture.rgba)?;
        writer.finish()?;
    }
    Ok(out)
}

/// Encodes `capture` (timed, logged at info): the one PNG encode step of `capture` and of the
/// screenshot worker.
///
/// # Errors
/// The encoder's failure text.
pub fn encode_timed(capture: &Capture) -> Result<Vec<u8>, String> {
    let started = std::time::Instant::now();
    let bytes = encode_png(capture).map_err(|e| e.to_string())?;
    tracing::info!(
        bytes = bytes.len(),
        ms = started.elapsed().as_secs_f64() * 1000.0,
        "capture: png encoded"
    );
    Ok(bytes)
}

/// Encodes `capture` ([`encode_timed`]) and writes it to `path`.
///
/// # Errors
/// The encoder's failure text, or `cannot write <path>: <e>`.
pub fn encode_and_write(capture: &Capture, path: &Path) -> Result<(), String> {
    let bytes = encode_timed(capture)?;
    std::fs::write(path, &bytes).map_err(|e| format!("cannot write {}: {e}", path.display()))
}

/// An IPC `capture` of a held output (plan 38), answered on `tx` once the hold releases.
#[derive(Debug)]
pub struct PendingCapture {
    /// The (held) output to capture.
    pub output: Output,
    /// The absolute PNG path.
    pub path: PathBuf,
    /// The reply channel the connection is waiting on.
    pub tx: Sender<Response>,
}

/// Encodes and writes `capture` to `path` off the main thread, answering on `tx`.
///
/// # Errors
/// The thread could not be spawned.
pub fn spawn_encode_and_write_to(
    capture: Capture,
    path: PathBuf,
    tx: Sender<Response>,
) -> Result<(), String> {
    std::thread::Builder::new()
        .name("cde-capture-png".into())
        .spawn(move || {
            let response = match encode_and_write(&capture, &path) {
                Ok(()) => Response::Ok(format!("{}x{}", capture.width, capture.height)),
                Err(reason) => {
                    tracing::warn!(path = %path.display(), %reason, "capture: write failed");
                    Response::Err(reason)
                }
            };
            // A send error means the receiver (connection or compositor) is gone.
            let _ = tx.send(response);
        })
        .map(|_| ())
        .map_err(|err| {
            tracing::warn!(%err, "capture: cannot spawn the encoder thread");
            format!("cannot spawn the encoder thread: {err}")
        })
}

/// Lock and power admission for a capture of `output`.
///
/// # Errors
/// [`CaptureError::Locked`] while locked, [`CaptureError::PoweredOff`] when the output is off.
pub fn admit_capture(locked: bool, powered_off: bool) -> Result<(), CaptureError> {
    if locked {
        Err(CaptureError::Locked)
    } else if powered_off {
        Err(CaptureError::PoweredOff)
    } else {
        Ok(())
    }
}

/// Why a `capture` request is refused at request time.
#[derive(Debug, PartialEq, Eq)]
pub enum CaptureRefusal {
    /// The path is relative.
    NotAbsolute,
    /// No output is focused.
    NoOutput,
    /// The lock or power admission failed ([`admit_capture`]).
    Admission(CaptureError),
}

impl CaptureRefusal {
    /// The `err` reply text.
    fn reason(&self, path: &Path) -> String {
        match self {
            Self::NotAbsolute => format!("{} is not an absolute path", path.display()),
            Self::NoOutput => CaptureError::NoOutput.to_string(),
            Self::Admission(err) => err.to_string(),
        }
    }
}

/// What [`CrucibleState::request_capture`] does with a request.
#[derive(Debug, PartialEq, Eq)]
pub enum CaptureDecision {
    /// Refuse with an `err` reply.
    Refuse(CaptureRefusal),
    /// Capture the output now.
    Now,
    /// The output is reveal-held: park the reply until the hold releases.
    Park,
}

/// The request-time capture checks, in order: absolute path, a focused output, lock/power
/// admission, then whether the output is held. **Pure**.
#[must_use]
pub fn decide_capture(
    absolute: bool,
    has_output: bool,
    locked: bool,
    powered_off: bool,
    held: bool,
) -> CaptureDecision {
    if !absolute {
        return CaptureDecision::Refuse(CaptureRefusal::NotAbsolute);
    }
    if !has_output {
        return CaptureDecision::Refuse(CaptureRefusal::NoOutput);
    }
    if let Err(err) = admit_capture(locked, powered_off) {
        return CaptureDecision::Refuse(CaptureRefusal::Admission(err));
    }
    if held {
        CaptureDecision::Park
    } else {
        CaptureDecision::Now
    }
}

impl<B: Backend + 'static> CrucibleState<B> {
    /// Captures `output` with no cursor (see [`render_capture`]).
    ///
    /// # Errors
    /// Refused while locked or when the output is powered off; otherwise the backend's error.
    pub fn capture_output(&mut self, output: &Output) -> Result<Capture, CaptureError> {
        admit_capture(
            self.session_lock.is_locked(),
            self.backend_data.output_powered_off(output),
        )?;
        self.render_capture(output)
    }

    /// Renders and reads back `output` with no admission check (the caller has admitted it).
    fn render_capture(&mut self, output: &Output) -> Result<Capture, CaptureError> {
        let started = std::time::Instant::now();
        let capture = B::capture_output(self, output)?;
        tracing::info!(
            output = %output.name(),
            width = capture.width,
            height = capture.height,
            ms = started.elapsed().as_secs_f64() * 1000.0,
            "capture: rendered and read back"
        );
        Ok(capture)
    }

    /// `capture <path>` (IPC): the one entry. Runs the request-time checks once (see
    /// [`decide_capture`]), then captures the focused output now and encodes it off-thread, or,
    /// when that output is reveal-held (plan 38), parks the reply until the hold releases
    /// ([`Self::run_deferred_captures`] admits it again, against the same output).
    ///
    /// # Errors
    /// The `err` reply: a relative path, no focused output, `session locked`, `output powered
    /// off`, or the capture's failure.
    pub fn request_capture(&mut self, path: PathBuf) -> Result<Channel<Response>, String> {
        let output = self.focused_smithay_output();
        let powered_off = output
            .as_ref()
            .is_some_and(|o| self.backend_data.output_powered_off(o));
        let held = output
            .as_ref()
            .is_some_and(|o| self.reveal_holds.is_held(o));
        let decision = decide_capture(
            path.is_absolute(),
            output.is_some(),
            self.session_lock.is_locked(),
            powered_off,
            held,
        );
        let (tx, rx) = channel::<Response>();
        match (decision, output) {
            (CaptureDecision::Refuse(refusal), _) => Err(refusal.reason(&path)),
            (CaptureDecision::Park, Some(output)) => {
                self.pending_captures
                    .push(PendingCapture { output, path, tx });
                Ok(rx)
            }
            (CaptureDecision::Now, Some(output)) => {
                let capture = self.render_capture(&output).map_err(|e| e.to_string())?;
                spawn_encode_and_write_to(capture, path, tx)?;
                Ok(rx)
            }
            // `decide_capture` refuses a missing output before it can park or capture.
            (CaptureDecision::Park | CaptureDecision::Now, None) => {
                Err(CaptureError::NoOutput.to_string())
            }
        }
    }

    /// Performs and answers the captures deferred on any of the `released` outputs, under
    /// today's admission rules (a lock or power-off since then answers `err`).
    pub(crate) fn run_deferred_captures(&mut self, released: &[Output]) {
        let (due, rest): (Vec<_>, Vec<_>) = std::mem::take(&mut self.pending_captures)
            .into_iter()
            .partition(|p| released.contains(&p.output));
        self.pending_captures = rest;
        for pending in due {
            match self.capture_output(&pending.output) {
                Ok(capture) => {
                    if let Err(reason) =
                        spawn_encode_and_write_to(capture, pending.path, pending.tx.clone())
                    {
                        let _ = pending.tx.send(Response::Err(reason));
                    }
                }
                Err(err) => {
                    let _ = pending.tx.send(Response::Err(err.to_string()));
                }
            }
        }
    }

    /// The frame view inputs for a capture of `output`: [`CrucibleState::begin_frame`]'s, with the
    /// cursor (and so the drag icon) suppressed.
    pub(crate) fn capture_inputs(&mut self, output: &Output) -> crate::render::FrameInputs {
        let mut inputs = self.begin_frame(output);
        inputs.cursor = None;
        inputs
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn synthetic(width: u32, height: u32) -> Capture {
        let mut rgba = Vec::new();
        for y in 0..height {
            for x in 0..width {
                #[allow(clippy::cast_possible_truncation)]
                rgba.extend_from_slice(&[(x * 40) as u8, (y * 60) as u8, 7, 255]);
            }
        }
        Capture {
            width,
            height,
            stride: width * 4,
            rgba,
        }
    }

    #[test]
    fn png_round_trips_the_pixels() {
        let cap = synthetic(5, 3);
        let bytes = encode_png(&cap).unwrap();
        let decoder = png::Decoder::new(std::io::Cursor::new(bytes));
        let mut reader = decoder.read_info().unwrap();
        let mut buf = vec![0; reader.output_buffer_size().unwrap()];
        let info = reader.next_frame(&mut buf).unwrap();
        assert_eq!((info.width, info.height), (5, 3));
        assert_eq!(info.color_type, png::ColorType::Rgba);
        assert_eq!(&buf[..info.buffer_size()], &cap.rgba[..]);
    }

    #[test]
    fn encode_rejects_a_short_buffer() {
        let mut cap = synthetic(4, 4);
        cap.rgba.pop();
        assert!(matches!(encode_png(&cap), Err(EncodeError::Size { .. })));
    }

    #[test]
    fn capture_is_refused_while_locked_or_powered_off() {
        assert_eq!(admit_capture(true, false), Err(CaptureError::Locked));
        assert_eq!(admit_capture(true, true), Err(CaptureError::Locked));
        assert_eq!(admit_capture(false, true), Err(CaptureError::PoweredOff));
        assert_eq!(admit_capture(false, false), Ok(()));
        assert_eq!(CaptureError::Locked.to_string(), "session locked");
    }

    #[test]
    fn capture_request_checks_run_in_order_and_park_only_when_admitted() {
        use CaptureDecision::{Now, Park, Refuse};
        use CaptureRefusal::{Admission, NoOutput, NotAbsolute};
        // Relative path first, then no output, then lock before power-off.
        assert_eq!(
            decide_capture(false, false, true, true, true),
            Refuse(NotAbsolute)
        );
        assert_eq!(
            decide_capture(true, false, true, true, true),
            Refuse(NoOutput)
        );
        assert_eq!(
            decide_capture(true, true, true, true, true),
            Refuse(Admission(CaptureError::Locked))
        );
        assert_eq!(
            decide_capture(true, true, false, true, true),
            Refuse(Admission(CaptureError::PoweredOff))
        );
        // Admitted: held parks, otherwise captures now.
        assert_eq!(decide_capture(true, true, false, false, true), Park);
        assert_eq!(decide_capture(true, true, false, false, false), Now);
        assert_eq!(
            NotAbsolute.reason(Path::new("x.png")),
            "x.png is not an absolute path"
        );
    }

    #[test]
    fn a_parked_capture_is_admitted_again_at_release() {
        // A request that parked was admitted; `run_deferred_captures` re-admits through
        // `capture_output` (= `admit_capture`), so a lock or power-off since then is refused with
        // the same text as at request time.
        assert_eq!(
            decide_capture(true, true, false, false, true),
            CaptureDecision::Park
        );
        assert_eq!(
            admit_capture(true, false).unwrap_err().to_string(),
            "session locked"
        );
        assert_eq!(
            admit_capture(false, true).unwrap_err().to_string(),
            "output powered off"
        );
    }

    #[test]
    fn pack_rows_flips_bottom_up_rows_and_forces_opaque() {
        // Two rows of one pixel: bottom-up input.
        let bytes = [1, 1, 1, 0, 2, 2, 2, 9];
        let cap = pack_rows(&bytes, 1, 2, true);
        assert_eq!(cap.rgba, vec![2, 2, 2, 255, 1, 1, 1, 255]);
        let cap = pack_rows(&bytes, 1, 2, false);
        assert_eq!(cap.rgba, vec![1, 1, 1, 255, 2, 2, 2, 255]);
    }
}
