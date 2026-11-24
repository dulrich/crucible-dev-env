//! The `zwlr_output_manager_v1` server (wlr-output-management-unstable-v1, plan 53 WP-2), so
//! `wlr-randr` and `kanshi` can move and rescale outputs live.
//!
//! Three layers:
//! - **Effective settings** ([`OutputOverrides`] plus the `effective_*` methods): each connector's
//!   position and scale come from its live override, else config, else none (position) or 1
//!   (scale). Adoption, reflow and reload read these, never the config directly. Overrides last
//!   for the session, survive replugs, are never written to config, and are cleared by a reload
//!   that changes an `output` block.
//! - **Pure decision** ([`HeadState`], [`HeadRequest`], [`decide`], [`SnapshotTracker`]): plain
//!   data, unit-tested. `test` and `apply` share [`decide`], which validates fully before anything
//!   is mutated. Only an effective change of position and/or scale on enabled heads is accepted;
//!   any effective mode, custom-mode, transform, adaptive-sync or enable/disable change fails.
//! - **Protocol glue** (the `GlobalDispatch`/`Dispatch` impls): per-client head and mode objects,
//!   the change notification [`CrucibleState::output_layout_changed`], and apply.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use smithay::output::{Output, Scale};
use smithay::reexports::wayland_protocols_wlr::output_management::v1::server::{
    zwlr_output_configuration_head_v1::{self as cfg_head, ZwlrOutputConfigurationHeadV1},
    zwlr_output_configuration_v1::{self as cfg, ZwlrOutputConfigurationV1},
    zwlr_output_head_v1::{self as head, ZwlrOutputHeadV1},
    zwlr_output_manager_v1::{self as manager, ZwlrOutputManagerV1},
    zwlr_output_mode_v1::{self as mode, ZwlrOutputModeV1},
};
use smithay::reexports::wayland_server::protocol::wl_output::Transform as WlTransform;
use smithay::reexports::wayland_server::{
    Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource, WEnum,
    backend::ClientId,
};
use smithay::utils::{Physical, Size, Transform};
use tracing::{info, warn};

use crucible_config::{
    OUTPUT_POSITION_MAX, OUTPUT_POSITION_MIN, OUTPUT_SCALE_MAX, OUTPUT_SCALE_MIN,
    OUTPUT_SCALE_STEPS,
};

use crate::state::{Backend, CrucibleState};

/// The `zwlr_output_manager_v1` version advertised: the highest the bindings carry.
pub const MANAGER_VERSION: u32 = 4;

// ---- effective settings ----------------------------------------------------------------------

/// The live per-connector overrides set by a successful `apply`, by connector name.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct OutputOverrides {
    /// Live origins in logical pixels.
    pub position: BTreeMap<String, (i32, i32)>,
    /// Live scales in 1/120 steps.
    pub scale_120: BTreeMap<String, u32>,
}

impl OutputOverrides {
    /// Whether no override is set.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.position.is_empty() && self.scale_120.is_empty()
    }
}

/// The modes a backend knows for an output (udev: the DRM connector's), stored in the
/// [`Output`]'s user data at adoption. Without it the current mode is the only one advertised.
#[derive(Debug, Default)]
pub struct OutputModes(pub Mutex<Vec<ModeInfo>>);

/// Records `modes` as `output`'s advertised mode list (replacing an earlier list).
pub fn set_output_modes(output: &Output, modes: Vec<ModeInfo>) {
    output
        .user_data()
        .insert_if_missing_threadsafe(OutputModes::default);
    let Some(slot) = output.user_data().get::<OutputModes>() else {
        return;
    };
    *slot
        .0
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = modes;
}

/// A scale from 1/120 steps, `Integer(1)` at exactly 1 (the default output state).
#[must_use]
pub fn scale_from_120(scale_120: u32) -> Scale {
    if scale_120 == OUTPUT_SCALE_STEPS {
        Scale::Integer(1)
    } else {
        Scale::Fractional(f64::from(scale_120) / f64::from(OUTPUT_SCALE_STEPS))
    }
}

/// `scale` in 1/120 steps when it is finite, positive and representable, else `None`.
#[must_use]
pub fn quantize_scale(scale: f64) -> Option<u32> {
    let steps = (scale * f64::from(OUTPUT_SCALE_STEPS)).round();
    if !steps.is_finite() || steps < 1.0 || steps > f64::from(u32::MAX) {
        return None;
    }
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    Some(steps as u32)
}

/// Whether `scale_120` lies in the config's accepted scale range.
fn scale_in_range(scale_120: u32) -> bool {
    let s = f64::from(scale_120) / f64::from(OUTPUT_SCALE_STEPS);
    (OUTPUT_SCALE_MIN..=OUTPUT_SCALE_MAX).contains(&s)
}

/// Whether `(x, y)` lies in the config's accepted position range.
fn position_in_range((x, y): (i32, i32)) -> bool {
    let range = OUTPUT_POSITION_MIN..=OUTPUT_POSITION_MAX;
    range.contains(&x) && range.contains(&y)
}

/// The logical size Smithay's `Space` reports for a mode of `mode` hardware pixels under
/// `transform` and `scale`: transformed, divided by the scale, rounded up.
#[must_use]
pub fn logical_size(mode: (i32, i32), transform: Transform, scale: f64) -> (i32, i32) {
    let size = transform
        .transform_size(Size::<i32, Physical>::from(mode))
        .to_f64()
        .to_logical(scale)
        .to_i32_ceil::<i32>();
    (size.w, size.h)
}

impl<B: Backend + 'static> CrucibleState<B> {
    /// Whether another mapped output earlier in the Space shares `output`'s connector name (the
    /// multi-GPU duplicate-name limitation): such an output gets no live override.
    fn is_duplicate_name(&self, output: &Output) -> bool {
        let name = output.name();
        self.space
            .outputs()
            .find(|o| o.name() == name)
            .is_some_and(|first| first != output)
    }

    /// The effective origin of the connector `name`: its live override, else its configured
    /// `position`, else `None` (the strip).
    #[must_use]
    pub fn effective_output_position(&self, name: &str) -> Option<(i32, i32)> {
        self.output_management
            .overrides
            .position
            .get(name)
            .copied()
            .or_else(|| self.config.output_position(name))
    }

    /// The effective scale of `output`: its live override, else its configured scale, else 1. A
    /// duplicate-named output reads the config only.
    #[must_use]
    pub fn effective_output_scale(&self, output: &Output) -> Scale {
        let name = output.name();
        match self.output_management.overrides.scale_120.get(&name) {
            Some(&s) if !self.is_duplicate_name(output) => scale_from_120(s),
            _ => crate::config::configured_scale(&self.config, &name),
        }
    }

    /// The desired origin of each connector by name (live overrides over config positions): the
    /// planner's only position feed.
    #[must_use]
    pub fn desired_output_positions(&self) -> BTreeMap<String, (i32, i32)> {
        let mut desired: BTreeMap<String, (i32, i32)> = self
            .config
            .outputs
            .iter()
            .filter_map(|(name, output)| Some((name.clone(), output.position?)))
            .collect();
        for (name, position) in &self.output_management.overrides.position {
            desired.insert(name.clone(), *position);
        }
        desired
    }
}

// ---- pure head state and decision -------------------------------------------------------------

/// One advertised mode.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ModeInfo {
    /// Width in hardware pixels.
    pub width: i32,
    /// Height in hardware pixels.
    pub height: i32,
    /// Refresh in mHz (0: unknown, not sent).
    pub refresh: i32,
    /// The backend's preferred mode.
    pub preferred: bool,
}

/// Everything a head advertises, as plain data.
#[derive(Clone, Debug, PartialEq)]
pub struct HeadState {
    /// The output's stable id (see [`output_head_id`]).
    pub id: u64,
    /// The `wl_output` name.
    pub name: String,
    /// The `wl_output` description.
    pub description: String,
    /// Manufacturer, when known.
    pub make: Option<String>,
    /// Model, when known.
    pub model: Option<String>,
    /// Serial number, when known.
    pub serial_number: Option<String>,
    /// Physical size in mm, for a physical output only.
    pub physical_size: Option<(i32, i32)>,
    /// The mode list.
    pub modes: Vec<ModeInfo>,
    /// Index of the current mode in `modes`.
    pub current_mode: Option<usize>,
    /// Origin in logical pixels.
    pub position: (i32, i32),
    /// The output transform.
    pub transform: Transform,
    /// The fractional scale.
    pub scale: f64,
}

impl HeadState {
    /// The current mode's hardware size, `(0, 0)` without one.
    fn mode_size(&self) -> (i32, i32) {
        self.current_mode
            .and_then(|i| self.modes.get(i))
            .map_or((0, 0), |m| (m.width, m.height))
    }

    /// Whether the geometry (mode, position, transform, scale) differs from `other`.
    fn geometry_differs(&self, other: &Self) -> bool {
        self.mode_size() != other.mode_size()
            || self.position != other.position
            || self.transform != other.transform
            || self.scale != other.scale
    }
}

/// A requested mode for one head.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ModeRequest {
    /// One of the head's advertised modes, by index.
    Listed(usize),
    /// A custom mode: hardware size and refresh in mHz (0: unspecified).
    Custom {
        /// Width.
        width: i32,
        /// Height.
        height: i32,
        /// Refresh in mHz, 0 for any.
        refresh: i32,
    },
}

/// The protocol error a configuration-head setter raises.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SetError {
    /// The property was set twice (also: both a mode and a custom mode).
    AlreadySet,
    /// The mode belongs to another head.
    InvalidMode,
    /// The custom mode is not positive.
    InvalidCustomMode,
    /// The transform is outside the enum.
    InvalidTransform,
    /// The scale is negative or zero.
    InvalidScale,
    /// The adaptive-sync state is outside the enum.
    InvalidAdaptiveSync,
}

impl From<SetError> for cfg_head::Error {
    fn from(e: SetError) -> Self {
        match e {
            SetError::AlreadySet => Self::AlreadySet,
            SetError::InvalidMode => Self::InvalidMode,
            SetError::InvalidCustomMode => Self::InvalidCustomMode,
            SetError::InvalidTransform => Self::InvalidTransform,
            SetError::InvalidScale => Self::InvalidScale,
            SetError::InvalidAdaptiveSync => Self::InvalidAdaptiveSyncState,
        }
    }
}

/// The properties a client set on one enabled head (`zwlr_output_configuration_head_v1`).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct HeadRequest {
    /// The head this configures.
    pub head: u64,
    /// A requested mode.
    pub mode: Option<ModeRequest>,
    /// A requested origin.
    pub position: Option<(i32, i32)>,
    /// A requested transform.
    pub transform: Option<Transform>,
    /// A requested scale.
    pub scale: Option<f64>,
    /// A requested adaptive-sync state.
    pub adaptive_sync: Option<bool>,
}

impl HeadRequest {
    /// `set_mode` with a mode of head `mode_head` at `index`.
    ///
    /// # Errors
    /// [`SetError`] per the XML.
    pub fn set_mode(&mut self, mode_head: u64, index: usize) -> Result<(), SetError> {
        if self.mode.is_some() {
            return Err(SetError::AlreadySet);
        }
        if mode_head != self.head {
            return Err(SetError::InvalidMode);
        }
        self.mode = Some(ModeRequest::Listed(index));
        Ok(())
    }

    /// `set_custom_mode`.
    ///
    /// # Errors
    /// [`SetError`] per the XML.
    pub fn set_custom_mode(
        &mut self,
        width: i32,
        height: i32,
        refresh: i32,
    ) -> Result<(), SetError> {
        if self.mode.is_some() {
            return Err(SetError::AlreadySet);
        }
        if width <= 0 || height <= 0 || refresh < 0 {
            return Err(SetError::InvalidCustomMode);
        }
        self.mode = Some(ModeRequest::Custom {
            width,
            height,
            refresh,
        });
        Ok(())
    }

    /// `set_position`.
    ///
    /// # Errors
    /// [`SetError::AlreadySet`].
    pub fn set_position(&mut self, x: i32, y: i32) -> Result<(), SetError> {
        if self.position.is_some() {
            return Err(SetError::AlreadySet);
        }
        self.position = Some((x, y));
        Ok(())
    }

    /// `set_transform` (`None`: outside the enum).
    ///
    /// # Errors
    /// [`SetError`] per the XML.
    pub fn set_transform(&mut self, transform: Option<Transform>) -> Result<(), SetError> {
        if self.transform.is_some() {
            return Err(SetError::AlreadySet);
        }
        self.transform = Some(transform.ok_or(SetError::InvalidTransform)?);
        Ok(())
    }

    /// `set_scale`.
    ///
    /// # Errors
    /// [`SetError`] per the XML.
    pub fn set_scale(&mut self, scale: f64) -> Result<(), SetError> {
        if self.scale.is_some() {
            return Err(SetError::AlreadySet);
        }
        if !scale.is_finite() || scale <= 0.0 {
            return Err(SetError::InvalidScale);
        }
        self.scale = Some(scale);
        Ok(())
    }

    /// `set_adaptive_sync` (`None`: outside the enum).
    ///
    /// # Errors
    /// [`SetError`] per the XML.
    pub fn set_adaptive_sync(&mut self, enabled: Option<bool>) -> Result<(), SetError> {
        if self.adaptive_sync.is_some() {
            return Err(SetError::AlreadySet);
        }
        self.adaptive_sync = Some(enabled.ok_or(SetError::InvalidAdaptiveSync)?);
        Ok(())
    }
}

/// What a configuration asks of one head.
#[derive(Clone, Debug, PartialEq)]
pub enum Requested {
    /// `disable_head`.
    Disable(u64),
    /// `enable_head` plus its setters.
    Enable(HeadRequest),
}

impl Requested {
    fn head(&self) -> u64 {
        match self {
            Self::Disable(id) => *id,
            Self::Enable(r) => r.head,
        }
    }
}

/// One head's accepted change.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HeadChange {
    /// The connector name (the override key).
    pub name: String,
    /// A new origin.
    pub position: Option<(i32, i32)>,
    /// A new scale in 1/120 steps.
    pub scale_120: Option<u32>,
}

/// The decision for one `test`/`apply`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// The serial is stale or a configured head is gone.
    Cancelled,
    /// A current head was left out (the `unconfigured_head` protocol error).
    UnconfiguredHead,
    /// Rejected; nothing changes.
    Failed(&'static str),
    /// Accepted; the changes to apply (empty: nothing changes).
    Succeeded(Vec<HeadChange>),
}

/// Decides a configuration created at `serial` against the `current` heads (advertised at
/// `current_serial`). Shared by `test` and `apply`; it mutates nothing.
#[must_use]
pub fn decide(
    current_serial: u32,
    current: &[HeadState],
    serial: u32,
    requests: &[Requested],
) -> Outcome {
    if serial != current_serial
        || requests
            .iter()
            .any(|r| !current.iter().any(|h| h.id == r.head()))
    {
        return Outcome::Cancelled;
    }
    let mut changes = Vec::new();
    let mut rects = Vec::with_capacity(current.len());
    for h in current {
        let Some(req) = requests.iter().find(|r| r.head() == h.id) else {
            return Outcome::UnconfiguredHead;
        };
        let Requested::Enable(r) = req else {
            return Outcome::Failed("disabling an output is not supported");
        };
        let mode = h.current_mode.and_then(|i| h.modes.get(i));
        match r.mode {
            Some(ModeRequest::Listed(i)) if Some(i) != h.current_mode => {
                if h.modes.get(i) != mode {
                    return Outcome::Failed("changing the mode is not supported");
                }
            }
            Some(ModeRequest::Custom {
                width,
                height,
                refresh,
            }) => {
                let same = mode.is_some_and(|m| {
                    m.width == width && m.height == height && (refresh == 0 || refresh == m.refresh)
                });
                if !same {
                    return Outcome::Failed("changing the mode is not supported");
                }
            }
            _ => {}
        }
        if r.transform.is_some_and(|t| t != h.transform) {
            return Outcome::Failed("changing the transform is not supported");
        }
        if r.adaptive_sync == Some(true) {
            return Outcome::Failed("adaptive sync is not supported");
        }
        let position = r.position.unwrap_or(h.position);
        let position_change = position != h.position;
        if position_change && !position_in_range(position) {
            return Outcome::Failed("position out of range");
        }
        let current_120 = quantize_scale(h.scale);
        let scale_120 = match r.scale {
            Some(s) => match quantize_scale(s) {
                Some(q) if Some(q) == current_120 => current_120,
                Some(q) if scale_in_range(q) => Some(q),
                _ => return Outcome::Failed("scale out of range"),
            },
            None => current_120,
        };
        let scale_change = scale_120 != current_120;
        let scale = scale_120.map_or(h.scale, |q| f64::from(q) / f64::from(OUTPUT_SCALE_STEPS));
        if position_change || scale_change {
            if current.iter().filter(|o| o.name == h.name).count() > 1 {
                return Outcome::Failed("duplicate output name");
            }
            changes.push(HeadChange {
                name: h.name.clone(),
                position: position_change.then_some(position),
                scale_120: if scale_change { scale_120 } else { None },
            });
        }
        let (w, hgt) = logical_size(h.mode_size(), h.transform, scale);
        let (Some(right), Some(bottom)) = (position.0.checked_add(w), position.1.checked_add(hgt))
        else {
            return Outcome::Failed("position out of range");
        };
        rects.push((position.0, position.1, right, bottom));
    }
    if changes.is_empty() {
        return Outcome::Succeeded(changes);
    }
    for (i, a) in rects.iter().enumerate() {
        for b in &rects[i + 1..] {
            let empty = |r: &(i32, i32, i32, i32)| r.0 >= r.2 || r.1 >= r.3;
            if !empty(a) && !empty(b) && a.0 < b.2 && b.0 < a.2 && a.1 < b.3 && b.1 < a.3 {
                return Outcome::Failed("outputs would overlap");
            }
        }
    }
    // A live move pins every head at its validated candidate origin, so the applied layout is
    // exactly the candidate (the planner would otherwise re-strip unpositioned heads).
    if changes.iter().any(|c| c.position.is_some()) {
        for (h, r) in current.iter().zip(&rects) {
            let position = Some((r.0, r.1));
            match changes.iter_mut().find(|c| c.name == h.name) {
                Some(c) => c.position = position,
                None => changes.push(HeadChange {
                    name: h.name.clone(),
                    position,
                    scale_120: None,
                }),
            }
        }
    }
    Outcome::Succeeded(changes)
}

/// The last advertised head snapshot and its `done` serial.
#[derive(Debug, Default)]
pub struct SnapshotTracker {
    /// The serial of the last `done`.
    pub serial: u32,
    /// The heads as last advertised.
    pub heads: Vec<HeadState>,
}

impl SnapshotTracker {
    /// Records `heads`; on a change bumps the serial and returns the previous snapshot.
    pub fn update(&mut self, heads: Vec<HeadState>) -> Option<Vec<HeadState>> {
        if heads == self.heads {
            return None;
        }
        self.serial = self.serial.wrapping_add(1);
        Some(std::mem::replace(&mut self.heads, heads))
    }
}

/// `wl_output.transform` to Smithay's, `None` outside the enum.
fn transform_from_wl(t: WEnum<WlTransform>) -> Option<Transform> {
    Some(match t {
        WEnum::Value(WlTransform::Normal) => Transform::Normal,
        WEnum::Value(WlTransform::_90) => Transform::_90,
        WEnum::Value(WlTransform::_180) => Transform::_180,
        WEnum::Value(WlTransform::_270) => Transform::_270,
        WEnum::Value(WlTransform::Flipped) => Transform::Flipped,
        WEnum::Value(WlTransform::Flipped90) => Transform::Flipped90,
        WEnum::Value(WlTransform::Flipped180) => Transform::Flipped180,
        WEnum::Value(WlTransform::Flipped270) => Transform::Flipped270,
        _ => return None,
    })
}

// ---- snapshot from Smithay outputs -------------------------------------------------------------

/// A stable per-output id stored in the output's user data.
#[derive(Debug)]
struct HeadId(u64);

/// The stable id of `output` (assigned on first use).
fn output_head_id(output: &Output) -> u64 {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    output
        .user_data()
        .insert_if_missing_threadsafe(|| HeadId(NEXT.fetch_add(1, Ordering::Relaxed)));
    output.user_data().get::<HeadId>().map_or(0, |h| h.0)
}

/// A known identity string, `None` for empty or `Unknown`.
fn known(s: String) -> Option<String> {
    (!s.is_empty() && s != "Unknown").then_some(s)
}

/// The advertised state of `output`.
fn head_state(output: &Output) -> HeadState {
    let props = output.physical_properties();
    let mut modes = output
        .user_data()
        .get::<OutputModes>()
        .map(|m| {
            m.0.lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone()
        })
        .unwrap_or_default();
    let current = output.current_mode();
    let current_mode = current.map(|c| {
        modes
            .iter()
            .position(|m| m.width == c.size.w && m.height == c.size.h && m.refresh == c.refresh)
            .unwrap_or_else(|| {
                modes.push(ModeInfo {
                    width: c.size.w,
                    height: c.size.h,
                    refresh: c.refresh,
                    preferred: output.preferred_mode() == Some(c),
                });
                modes.len() - 1
            })
    });
    let location = output.current_location();
    HeadState {
        id: output_head_id(output),
        name: output.name(),
        description: output.description(),
        make: known(props.make),
        model: known(props.model),
        serial_number: known(props.serial_number),
        physical_size: (props.size.w > 0 && props.size.h > 0)
            .then_some((props.size.w, props.size.h)),
        modes,
        current_mode,
        position: (location.x, location.y),
        transform: output.current_transform(),
        scale: output.current_scale().fractional_scale(),
    }
}

// ---- protocol state ----------------------------------------------------------------------------

/// One client's head object and its mode objects.
#[derive(Debug)]
struct ClientHead {
    id: u64,
    head: ZwlrOutputHeadV1,
    modes: Vec<ZwlrOutputModeV1>,
}

/// One bound manager.
#[derive(Debug)]
struct ManagerEntry {
    manager: ZwlrOutputManagerV1,
    heads: Vec<ClientHead>,
}

/// The server state on [`CrucibleState`].
#[derive(Debug, Default)]
pub struct OutputManagement {
    /// The live overrides.
    pub overrides: OutputOverrides,
    /// The advertised snapshot and serial.
    tracker: SnapshotTracker,
    /// Bound, not stopped, managers.
    managers: Vec<ManagerEntry>,
}

/// Head user data: the output's id.
#[derive(Debug)]
pub struct HeadData(u64);

/// Mode user data: the head id and the mode's index.
#[derive(Debug)]
pub struct ModeData {
    head: u64,
    index: usize,
}

/// A configuration in progress.
#[derive(Debug)]
pub struct PendingConfig {
    serial: u32,
    used: bool,
    heads: Vec<PendingHead>,
}

/// One configured head.
#[derive(Debug)]
enum PendingHead {
    Disable(u64),
    Enable(Arc<Mutex<HeadRequest>>),
}

impl PendingHead {
    fn head(&self) -> u64 {
        match self {
            Self::Disable(id) => *id,
            Self::Enable(r) => lock(r).head,
        }
    }
}

/// Configuration user data.
pub type ConfigData = Mutex<PendingConfig>;
/// Configuration-head user data.
pub type ConfigHeadData = Arc<Mutex<HeadRequest>>;

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Advertises `zwlr_output_manager_v1` to every client.
pub fn create_global<B: Backend + 'static>(dh: &DisplayHandle) {
    let _ = dh.create_global::<CrucibleState<B>, ZwlrOutputManagerV1, ()>(MANAGER_VERSION, ());
}

/// Sends `h`'s modes on `head`, returning the mode objects.
fn send_modes<B: Backend + 'static>(
    dh: &DisplayHandle,
    client: &Client,
    head: &ZwlrOutputHeadV1,
    h: &HeadState,
) -> Vec<ZwlrOutputModeV1> {
    let mut out = Vec::with_capacity(h.modes.len());
    for (index, m) in h.modes.iter().enumerate() {
        let Ok(mode) = client.create_resource::<ZwlrOutputModeV1, ModeData, CrucibleState<B>>(
            dh,
            head.version(),
            ModeData { head: h.id, index },
        ) else {
            continue;
        };
        head.mode(&mode);
        mode.size(m.width, m.height);
        if m.refresh > 0 {
            mode.refresh(m.refresh);
        }
        if m.preferred {
            mode.preferred();
        }
        out.push(mode);
    }
    out
}

/// Sends the current-mode event for `h`, if it has one.
fn send_current_mode(ch: &ClientHead, h: &HeadState) {
    if let Some(mode) = h.current_mode.and_then(|i| ch.modes.get(i)) {
        ch.head.current_mode(mode);
    }
}

/// Introduces `h` to one manager.
fn send_new_head<B: Backend + 'static>(
    dh: &DisplayHandle,
    manager: &ZwlrOutputManagerV1,
    h: &HeadState,
) -> Option<ClientHead> {
    let client = manager.client()?;
    let version = manager.version();
    let head = client
        .create_resource::<ZwlrOutputHeadV1, HeadData, CrucibleState<B>>(
            dh,
            version,
            HeadData(h.id),
        )
        .ok()?;
    manager.head(&head);
    head.name(h.name.clone());
    head.description(h.description.clone());
    if let Some((w, hh)) = h.physical_size {
        head.physical_size(w, hh);
    }
    let modes = send_modes::<B>(dh, &client, &head, h);
    let ch = ClientHead {
        id: h.id,
        head,
        modes,
    };
    ch.head.enabled(1);
    send_current_mode(&ch, h);
    ch.head.position(h.position.0, h.position.1);
    ch.head.transform(h.transform.into());
    ch.head.scale(h.scale);
    if version >= 2 {
        for (value, send) in [
            (
                &h.make,
                ZwlrOutputHeadV1::make as fn(&ZwlrOutputHeadV1, String),
            ),
            (&h.model, ZwlrOutputHeadV1::model),
            (&h.serial_number, ZwlrOutputHeadV1::serial_number),
        ] {
            if let Some(v) = value {
                send(&ch.head, v.clone());
            }
        }
    }
    if version >= 4 {
        ch.head.adaptive_sync(head::AdaptiveSyncState::Disabled);
    }
    Some(ch)
}

/// Sends what changed between `old` and `new` on one client head.
fn send_head_delta<B: Backend + 'static>(
    dh: &DisplayHandle,
    ch: &mut ClientHead,
    old: &HeadState,
    new: &HeadState,
) {
    if old.modes != new.modes {
        for mode in ch.modes.drain(..) {
            mode.finished();
        }
        if let Some(client) = ch.head.client() {
            ch.modes = send_modes::<B>(dh, &client, &ch.head, new);
        }
        send_current_mode(ch, new);
    } else if old.current_mode != new.current_mode {
        send_current_mode(ch, new);
    }
    if old.position != new.position {
        ch.head.position(new.position.0, new.position.1);
    }
    if old.transform != new.transform {
        ch.head.transform(new.transform.into());
    }
    if old.scale != new.scale {
        ch.head.scale(new.scale);
    }
}

/// Retires one client head and its modes.
fn finish_head(ch: ClientHead) {
    for mode in ch.modes {
        mode.finished();
    }
    ch.head.finished();
}

impl<B: Backend + 'static> CrucibleState<B> {
    /// The change notification: recomputes every head from the mapped outputs and, if anything
    /// changed, sends the deltas and a new `done` serial to every manager. A geometry change of an
    /// existing output cancels an active snip. Call after every path that changes outputs.
    pub fn output_layout_changed(&mut self) {
        let heads: Vec<HeadState> = self.space.outputs().map(head_state).collect();
        let Some(old) = self.output_management.tracker.update(heads) else {
            return;
        };
        let new = &self.output_management.tracker.heads;
        let moved = new
            .iter()
            .any(|n| old.iter().any(|o| o.id == n.id && o.geometry_differs(n)));
        let serial = self.output_management.tracker.serial;
        let dh = self.display_handle.clone();
        self.output_management
            .managers
            .retain(|e| e.manager.is_alive());
        for entry in &mut self.output_management.managers {
            let mut kept = Vec::with_capacity(entry.heads.len());
            for ch in entry.heads.drain(..) {
                if new.iter().any(|h| h.id == ch.id) {
                    kept.push(ch);
                } else {
                    finish_head(ch);
                }
            }
            entry.heads = kept;
            for h in new {
                if let Some(ch) = entry.heads.iter_mut().find(|c| c.id == h.id) {
                    if let Some(o) = old.iter().find(|o| o.id == h.id) {
                        send_head_delta::<B>(&dh, ch, o, h);
                    }
                } else if let Some(ch) = send_new_head::<B>(&dh, &entry.manager, h) {
                    entry.heads.push(ch);
                }
            }
            entry.manager.done(serial);
        }
        if moved {
            self.cancel_snip_for_geometry();
        }
    }

    /// Writes an accepted configuration's overrides and reflows through the scale path.
    fn apply_output_changes(&mut self, changes: Vec<HeadChange>) {
        if changes.is_empty() {
            return;
        }
        for change in changes {
            info!(output = %change.name, position = ?change.position, scale_120 = ?change.scale_120, "output management: applied");
            if let Some(p) = change.position {
                self.output_management
                    .overrides
                    .position
                    .insert(change.name.clone(), p);
            }
            if let Some(s) = change.scale_120 {
                self.output_management
                    .overrides
                    .scale_120
                    .insert(change.name, s);
            }
        }
        self.cancel_snip_for_geometry();
        self.apply_output_scales(true);
    }

    /// `test`/`apply` of `config`.
    fn finish_configuration(&mut self, config: &ZwlrOutputConfigurationV1, apply: bool) {
        let (serial, requests) = {
            let Some(data) = config.data::<ConfigData>() else {
                return;
            };
            let mut pending = lock(data);
            if pending.used {
                config.post_error(cfg::Error::AlreadyUsed, "configuration already used");
                return;
            }
            pending.used = true;
            let requests: Vec<Requested> = pending
                .heads
                .iter()
                .map(|p| match p {
                    PendingHead::Disable(id) => Requested::Disable(*id),
                    PendingHead::Enable(r) => Requested::Enable(lock(r).clone()),
                })
                .collect();
            (pending.serial, requests)
        };
        // Bring the advertisement up to date first, so a missed change reads as stale.
        self.output_layout_changed();
        let tracker = &self.output_management.tracker;
        match decide(tracker.serial, &tracker.heads, serial, &requests) {
            Outcome::Cancelled => config.cancelled(),
            Outcome::UnconfiguredHead => {
                config.post_error(cfg::Error::UnconfiguredHead, "a head was not configured");
            }
            Outcome::Failed(reason) => {
                warn!(reason, apply, "output management: configuration failed");
                config.failed();
            }
            Outcome::Succeeded(changes) => {
                config.succeeded();
                if apply {
                    self.apply_output_changes(changes);
                }
            }
        }
    }
}

impl<B: Backend + 'static> GlobalDispatch<ZwlrOutputManagerV1, ()> for CrucibleState<B> {
    fn bind(
        state: &mut Self,
        dh: &DisplayHandle,
        _client: &Client,
        resource: New<ZwlrOutputManagerV1>,
        _data: &(),
        data_init: &mut DataInit<'_, Self>,
    ) {
        let manager = data_init.init(resource, ());
        state.output_layout_changed();
        let mut heads = Vec::new();
        for h in &state.output_management.tracker.heads {
            if let Some(ch) = send_new_head::<B>(dh, &manager, h) {
                heads.push(ch);
            }
        }
        manager.done(state.output_management.tracker.serial);
        state
            .output_management
            .managers
            .push(ManagerEntry { manager, heads });
    }
}

impl<B: Backend + 'static> Dispatch<ZwlrOutputManagerV1, ()> for CrucibleState<B> {
    fn request(
        state: &mut Self,
        _client: &Client,
        resource: &ZwlrOutputManagerV1,
        request: manager::Request,
        _data: &(),
        _dh: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        match request {
            manager::Request::CreateConfiguration { id, serial } => {
                data_init.init(
                    id,
                    Mutex::new(PendingConfig {
                        serial,
                        used: false,
                        heads: Vec::new(),
                    }),
                );
            }
            manager::Request::Stop => {
                state
                    .output_management
                    .managers
                    .retain(|e| &e.manager != resource);
                resource.finished();
            }
            _ => {}
        }
    }

    fn destroyed(state: &mut Self, _client: ClientId, resource: &ZwlrOutputManagerV1, _data: &()) {
        state
            .output_management
            .managers
            .retain(|e| &e.manager != resource);
    }
}

impl<B: Backend + 'static> Dispatch<ZwlrOutputHeadV1, HeadData> for CrucibleState<B> {
    fn request(
        state: &mut Self,
        _client: &Client,
        resource: &ZwlrOutputHeadV1,
        request: head::Request,
        _data: &HeadData,
        _dh: &DisplayHandle,
        _data_init: &mut DataInit<'_, Self>,
    ) {
        if let head::Request::Release = request {
            for entry in &mut state.output_management.managers {
                entry.heads.retain(|c| &c.head != resource);
            }
        }
    }
}

impl<B: Backend + 'static> Dispatch<ZwlrOutputModeV1, ModeData> for CrucibleState<B> {
    fn request(
        _state: &mut Self,
        _client: &Client,
        _resource: &ZwlrOutputModeV1,
        _request: mode::Request,
        _data: &ModeData,
        _dh: &DisplayHandle,
        _data_init: &mut DataInit<'_, Self>,
    ) {
    }
}

impl<B: Backend + 'static> Dispatch<ZwlrOutputConfigurationV1, ConfigData> for CrucibleState<B> {
    fn request(
        state: &mut Self,
        _client: &Client,
        resource: &ZwlrOutputConfigurationV1,
        request: cfg::Request,
        data: &ConfigData,
        _dh: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        match request {
            cfg::Request::EnableHead { id, head } => {
                let head_id = head.data::<HeadData>().map_or(0, |d| d.0);
                let request = Arc::new(Mutex::new(HeadRequest {
                    head: head_id,
                    ..HeadRequest::default()
                }));
                data_init.init(id, Arc::clone(&request));
                add_head(resource, data, head_id, PendingHead::Enable(request));
            }
            cfg::Request::DisableHead { head } => {
                let head_id = head.data::<HeadData>().map_or(0, |d| d.0);
                add_head(resource, data, head_id, PendingHead::Disable(head_id));
            }
            cfg::Request::Apply => state.finish_configuration(resource, true),
            cfg::Request::Test => state.finish_configuration(resource, false),
            _ => {}
        }
    }
}

/// Records one head of a configuration, raising `already_used` / `already_configured_head`.
fn add_head(config: &ZwlrOutputConfigurationV1, data: &ConfigData, id: u64, head: PendingHead) {
    let mut pending = lock(data);
    if pending.used {
        config.post_error(cfg::Error::AlreadyUsed, "configuration already used");
    } else if pending.heads.iter().any(|p| p.head() == id) {
        config.post_error(cfg::Error::AlreadyConfiguredHead, "head configured twice");
    } else {
        pending.heads.push(head);
    }
}

impl<B: Backend + 'static> Dispatch<ZwlrOutputConfigurationHeadV1, ConfigHeadData>
    for CrucibleState<B>
{
    fn request(
        _state: &mut Self,
        _client: &Client,
        resource: &ZwlrOutputConfigurationHeadV1,
        request: cfg_head::Request,
        data: &ConfigHeadData,
        _dh: &DisplayHandle,
        _data_init: &mut DataInit<'_, Self>,
    ) {
        let mut r = lock(data);
        let result = match request {
            cfg_head::Request::SetMode { mode } => match mode.data::<ModeData>() {
                Some(m) => r.set_mode(m.head, m.index),
                None => Err(SetError::InvalidMode),
            },
            cfg_head::Request::SetCustomMode {
                width,
                height,
                refresh,
            } => r.set_custom_mode(width, height, refresh),
            cfg_head::Request::SetPosition { x, y } => r.set_position(x, y),
            cfg_head::Request::SetTransform { transform } => {
                r.set_transform(transform_from_wl(transform))
            }
            cfg_head::Request::SetScale { scale } => r.set_scale(scale),
            cfg_head::Request::SetAdaptiveSync { state } => r.set_adaptive_sync(match state {
                WEnum::Value(head::AdaptiveSyncState::Enabled) => Some(true),
                WEnum::Value(head::AdaptiveSyncState::Disabled) => Some(false),
                _ => None,
            }),
            _ => Ok(()),
        };
        if let Err(e) = result {
            resource.post_error(cfg_head::Error::from(e), format!("{e:?}"));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mode(width: i32, height: i32) -> ModeInfo {
        ModeInfo {
            width,
            height,
            refresh: 60_000,
            preferred: true,
        }
    }

    fn head(id: u64, name: &str, x: i32) -> HeadState {
        HeadState {
            id,
            name: name.into(),
            description: String::new(),
            make: None,
            model: None,
            serial_number: None,
            physical_size: None,
            modes: vec![mode(1920, 1080), mode(1280, 720)],
            current_mode: Some(0),
            position: (x, 0),
            transform: Transform::Normal,
            scale: 1.0,
        }
    }

    fn two() -> Vec<HeadState> {
        vec![head(1, "DP-1", 0), head(2, "DP-3", 1920)]
    }

    fn enable(head: u64) -> HeadRequest {
        HeadRequest {
            head,
            ..HeadRequest::default()
        }
    }

    fn reqs(a: HeadRequest, b: HeadRequest) -> Vec<Requested> {
        vec![Requested::Enable(a), Requested::Enable(b)]
    }

    #[test]
    fn a_position_change_is_applied() {
        let mut a = enable(1);
        a.position = Some((-1920, 0));
        let out = decide(5, &two(), 5, &reqs(a, enable(2)));
        assert_eq!(
            out,
            Outcome::Succeeded(vec![
                HeadChange {
                    name: "DP-1".into(),
                    position: Some((-1920, 0)),
                    scale_120: None,
                },
                HeadChange {
                    name: "DP-3".into(),
                    position: Some((1920, 0)),
                    scale_120: None,
                }
            ])
        );
    }

    #[test]
    fn a_live_move_pins_every_head_at_its_candidate() {
        // DP-3 is a strip output at 0; DP-1 moves to 3840. Both are pinned where validated.
        let heads = vec![head(1, "DP-1", 1920), head(2, "DP-3", 0)];
        let mut a = enable(1);
        a.position = Some((3840, 0));
        let mut b = enable(2);
        b.position = Some((0, 0));
        assert_eq!(
            decide(1, &heads, 1, &reqs(a, b)),
            Outcome::Succeeded(vec![
                HeadChange {
                    name: "DP-1".into(),
                    position: Some((3840, 0)),
                    scale_120: None,
                },
                HeadChange {
                    name: "DP-3".into(),
                    position: Some((0, 0)),
                    scale_120: None,
                },
            ])
        );
    }

    #[test]
    fn a_scale_change_is_applied_quantized() {
        let mut b = enable(2);
        b.scale = Some(1.25);
        let mut a = enable(1);
        a.scale = Some(1.0);
        let out = decide(1, &two(), 1, &reqs(a, b));
        assert_eq!(
            out,
            Outcome::Succeeded(vec![HeadChange {
                name: "DP-3".into(),
                position: None,
                scale_120: Some(150),
            }])
        );
    }

    #[test]
    fn unchanged_setters_are_not_changes() {
        let mut a = enable(1);
        a.position = Some((0, 0));
        a.scale = Some(1.0);
        a.transform = Some(Transform::Normal);
        a.mode = Some(ModeRequest::Listed(0));
        a.adaptive_sync = Some(false);
        let mut b = enable(2);
        b.mode = Some(ModeRequest::Custom {
            width: 1920,
            height: 1080,
            refresh: 0,
        });
        assert_eq!(
            decide(1, &two(), 1, &reqs(a, b)),
            Outcome::Succeeded(vec![])
        );
    }

    #[test]
    fn a_stale_serial_or_gone_head_is_cancelled() {
        assert_eq!(
            decide(2, &two(), 1, &reqs(enable(1), enable(2))),
            Outcome::Cancelled
        );
        assert_eq!(
            decide(2, &two(), 2, &reqs(enable(1), enable(9))),
            Outcome::Cancelled
        );
    }

    #[test]
    fn an_omitted_head_is_unconfigured() {
        assert_eq!(
            decide(1, &two(), 1, &[Requested::Enable(enable(1))]),
            Outcome::UnconfiguredHead
        );
    }

    #[test]
    fn out_of_range_values_fail() {
        let mut a = enable(1);
        a.position = Some((OUTPUT_POSITION_MAX + 1, 0));
        assert!(matches!(
            decide(1, &two(), 1, &reqs(a, enable(2))),
            Outcome::Failed(_)
        ));
        let mut a = enable(1);
        a.scale = Some(OUTPUT_SCALE_MAX + 1.0);
        assert!(matches!(
            decide(1, &two(), 1, &reqs(a, enable(2))),
            Outcome::Failed(_)
        ));
    }

    #[test]
    fn overlap_fails() {
        let mut a = enable(1);
        a.position = Some((100, 0));
        assert_eq!(
            decide(1, &two(), 1, &reqs(a, enable(2))),
            Outcome::Failed("outputs would overlap")
        );
        // Scaling DP-1 down (larger logical size) overlaps DP-3 too.
        let mut a = enable(1);
        a.scale = Some(0.5);
        assert_eq!(
            decide(1, &two(), 1, &reqs(a, enable(2))),
            Outcome::Failed("outputs would overlap")
        );
    }

    #[test]
    fn mode_transform_sync_and_disable_changes_fail() {
        let mut a = enable(1);
        a.mode = Some(ModeRequest::Listed(1));
        assert!(matches!(
            decide(1, &two(), 1, &reqs(a, enable(2))),
            Outcome::Failed(_)
        ));
        let mut a = enable(1);
        a.mode = Some(ModeRequest::Custom {
            width: 800,
            height: 600,
            refresh: 0,
        });
        assert!(matches!(
            decide(1, &two(), 1, &reqs(a, enable(2))),
            Outcome::Failed(_)
        ));
        let mut a = enable(1);
        a.transform = Some(Transform::_90);
        assert!(matches!(
            decide(1, &two(), 1, &reqs(a, enable(2))),
            Outcome::Failed(_)
        ));
        let mut a = enable(1);
        a.adaptive_sync = Some(true);
        assert!(matches!(
            decide(1, &two(), 1, &reqs(a, enable(2))),
            Outcome::Failed(_)
        ));
        let reqs = vec![Requested::Disable(1), Requested::Enable(enable(2))];
        assert!(matches!(decide(1, &two(), 1, &reqs), Outcome::Failed(_)));
    }

    #[test]
    fn duplicate_names_get_no_override() {
        let mut heads = two();
        heads[1].name = "DP-1".into();
        let mut b = enable(2);
        b.position = Some((1920, 1080));
        assert!(matches!(
            decide(1, &heads, 1, &reqs(enable(1), b)),
            Outcome::Failed(_)
        ));
    }

    #[test]
    fn setters_raise_the_xml_errors() {
        let mut r = enable(1);
        assert_eq!(r.set_mode(2, 0), Err(SetError::InvalidMode));
        assert_eq!(r.set_mode(1, 0), Ok(()));
        assert_eq!(r.set_custom_mode(10, 10, 0), Err(SetError::AlreadySet));
        assert_eq!(r.set_position(1, 2), Ok(()));
        assert_eq!(r.set_position(1, 2), Err(SetError::AlreadySet));
        assert_eq!(r.set_scale(0.0), Err(SetError::InvalidScale));
        assert_eq!(r.set_transform(None), Err(SetError::InvalidTransform));
        assert_eq!(
            r.set_adaptive_sync(None),
            Err(SetError::InvalidAdaptiveSync)
        );
        let mut r = enable(1);
        assert_eq!(
            r.set_custom_mode(0, 10, 0),
            Err(SetError::InvalidCustomMode)
        );
    }

    #[test]
    fn lifecycle_snapshot_hotplug_mode_replacement_and_requests() {
        let mut t = SnapshotTracker::default();
        // Initial snapshot: the first done serial.
        assert!(t.update(vec![head(1, "DP-1", 0)]).is_some());
        assert_eq!(t.serial, 1);
        assert!(t.update(vec![head(1, "DP-1", 0)]).is_none());
        assert_eq!(t.serial, 1);
        // Hotplug bumps the serial; a configuration made before it is cancelled.
        assert!(t.update(two()).is_some());
        assert_eq!(t.serial, 2);
        assert_eq!(
            decide(t.serial, &t.heads, 1, &reqs(enable(1), enable(2))),
            Outcome::Cancelled
        );
        // A mode replacement bumps it again.
        let mut heads = two();
        heads[0].modes = vec![mode(2560, 1440)];
        let old = t.update(heads).expect("changed");
        assert_eq!(t.serial, 3);
        assert_ne!(old[0].modes, t.heads[0].modes);
        // At the current serial: invalid (overlap) and unsupported (transform) fail; valid applies.
        let mut a = enable(1);
        a.position = Some((10, 0));
        assert!(matches!(
            decide(3, &t.heads, 3, &reqs(a, enable(2))),
            Outcome::Failed(_)
        ));
        let mut a = enable(1);
        a.transform = Some(Transform::Flipped180);
        assert!(matches!(
            decide(3, &t.heads, 3, &reqs(a, enable(2))),
            Outcome::Failed(_)
        ));
        let mut b = enable(2);
        b.position = Some((2560, 0));
        assert!(
            matches!(decide(3, &t.heads, 3, &reqs(enable(1), b)), Outcome::Succeeded(c) if c.len() == 2)
        );
    }

    #[test]
    fn logical_size_matches_smithay_rounding() {
        assert_eq!(
            logical_size((1920, 1080), Transform::Normal, 1.25),
            (1536, 864)
        );
        assert_eq!(
            logical_size((1920, 1080), Transform::_90, 1.0),
            (1080, 1920)
        );
        assert_eq!(
            logical_size((1000, 1000), Transform::Normal, 1.5),
            (667, 667)
        );
    }
}
