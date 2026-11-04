//! Pointer constraints (`zwp_pointer_constraints_v1`) and relative pointer
//! (`zwp_relative_pointer_manager_v1`): the globals, CDE's activation policy, and the pure motion
//! decision.
//!
//! Xwayland emulates X11 pointer grabs and `XWarpPointer` (Wine/Proton mouselook) with these two
//! protocols. Smithay tracks the constraints per surface but leaves *activation* to the
//! compositor; CDE activates a constraint only when
//! - the backend delivers relative motion ([`Backend::HAS_RELATIVE_MOTION`]; the absolute winit
//!   path never honours a constraint, so none is ever activated there),
//! - no CDE modal (overlay, session lock) takes input,
//! - the constrained surface has pointer focus, the pointer lies in its region, and
//! - the surface's client window also has keyboard focus.
//!
//! CDE's keyboard focus changes never move pointer focus, so [`CrucibleState::reconcile_pointer_constraint`]
//! runs on every keyboard focus change and pointer motion, deactivating a constraint the user left
//! by keyboard (Super chords, workspace switch, overlay, lock screen). On every deactivation CDE
//! performs, and on `remove_constraint`, the client's last cursor-position hint is applied with a
//! silent location update (no synthetic motion).
//!
//! Every constraint lifecycle step (created, activated, deactivated, refused, forgotten, removed)
//! is logged at `debug` under this module's target, deduplicated so steady motion does not repeat
//! a line; per-motion clamping is logged at `trace`.

use std::collections::HashMap;

use smithay::desktop::utils::bbox_from_surface_tree;
use smithay::input::pointer::PointerHandle;
use smithay::reexports::wayland_server::DisplayHandle;
use smithay::reexports::wayland_server::Resource;
use smithay::reexports::wayland_server::backend::ObjectId;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::utils::{Logical, Point, Rectangle};
use smithay::wayland::compositor::{RegionAttributes, get_parent};
use smithay::wayland::pointer_constraints::{
    PointerConstraint, PointerConstraintsHandler, PointerConstraintsState, with_pointer_constraint,
};
use smithay::wayland::relative_pointer::RelativePointerManagerState;
use smithay::wayland::seat::WaylandFocus;

use crate::state::{Backend, CrucibleState};
use tracing::debug;

/// The kind of an active constraint, as seen by [`constrained_motion`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConstraintKind {
    /// `zwp_locked_pointer_v1`: the pointer does not move; only relative motion is delivered.
    Locked,
    /// `zwp_confined_pointer_v1`: the pointer moves, but stays inside the region.
    Confined,
}

/// What a relative motion does to the pointer location under a constraint.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum MotionOutcome {
    /// Move to this global location (and deliver absolute motion as usual).
    Move(Point<f64, Logical>),
    /// Locked: deliver relative motion only; the location stays put.
    Suppress,
}

/// Decides where a relative motion takes the pointer.
///
/// `kind` is the *active* constraint on the pointer-focused surface (`None`: unconstrained).
/// `region` is the constraint region in surface-local coordinates (`None`: the whole surface);
/// `surface` is the surface's rectangle in global coordinates (its origin maps the region).
/// `proposed` is the already output-clamped `current + delta`.
pub fn constrained_motion(
    kind: Option<ConstraintKind>,
    region: Option<&RegionAttributes>,
    surface: Rectangle<i32, Logical>,
    current: Point<f64, Logical>,
    proposed: Point<f64, Logical>,
) -> MotionOutcome {
    match kind {
        None => MotionOutcome::Move(proposed),
        Some(ConstraintKind::Locked) => MotionOutcome::Suppress,
        Some(ConstraintKind::Confined) => {
            let origin = surface.loc.to_f64();
            let local = proposed - origin;
            let whole = Rectangle::from_size(surface.size);
            let inside = |p: Point<f64, Logical>| {
                whole.contains(p.to_i32_floor())
                    && region.is_none_or(|r| r.contains(p.to_i32_floor()))
            };
            if inside(local) {
                return MotionOutcome::Move(proposed);
            }
            // Clamp into each candidate rectangle; keep the nearest point that is inside.
            let candidates: Vec<Rectangle<i32, Logical>> = match region {
                None => vec![whole],
                Some(r) => r
                    .rects
                    .iter()
                    .filter(|(k, _)| matches!(k, smithay::wayland::compositor::RectangleKind::Add))
                    .filter_map(|(_, rect)| rect.intersection(whole))
                    .collect(),
            };
            candidates
                .iter()
                .map(|rect| clamp_into(local, *rect))
                .filter(|p| inside(*p))
                .min_by(|a, b| dist2(*a, local).total_cmp(&dist2(*b, local)))
                .map(|p| MotionOutcome::Move(p + origin))
                .unwrap_or(MotionOutcome::Move(current))
        }
    }
}

/// Clamps `p` into the half-open `rect` (the far edges are exclusive).
fn clamp_into(p: Point<f64, Logical>, rect: Rectangle<i32, Logical>) -> Point<f64, Logical> {
    const INSET: f64 = 1.0 / 256.0;
    let lo = rect.loc.to_f64();
    let hi = (rect.loc + rect.size).to_f64();
    Point::from((
        p.x.clamp(lo.x, (hi.x - INSET).max(lo.x)),
        p.y.clamp(lo.y, (hi.y - INSET).max(lo.y)),
    ))
}

fn dist2(a: Point<f64, Logical>, b: Point<f64, Logical>) -> f64 {
    let d = a - b;
    d.x * d.x + d.y * d.y
}

/// Whether a constraint may be active (pure eligibility, see the module docs).
pub fn constraint_eligible(
    relative_motion: bool,
    modal: bool,
    keyboard_on_surface: bool,
    pointer_in_region: bool,
) -> bool {
    relative_motion && !modal && keyboard_on_surface && pointer_in_region
}

/// The rectangle a keyboard-focused fullscreen window confines the pointer to (pure decision).
///
/// `fullscreen_output` is the full global rect of the output showing the focused fullscreen
/// window, `None` when the keyboard focus is not a shown, mapped fullscreen window. Returns the
/// rect only on a relative-motion backend, with no modal, and the config toggle on.
pub fn fullscreen_confine_rect(
    relative_motion: bool,
    modal: bool,
    toggle: bool,
    fullscreen_output: Option<Rectangle<i32, Logical>>,
) -> Option<Rectangle<i32, Logical>> {
    if relative_motion && !modal && toggle {
        fullscreen_output
    } else {
        None
    }
}

/// Clamps `proposed` into `rect` (far edges exclusive) when the pre-motion `current` location is
/// inside it; otherwise returns `proposed` unchanged (a pointer outside is never moved).
pub fn clamp_to_fullscreen(
    rect: Rectangle<i32, Logical>,
    current: Point<f64, Logical>,
    proposed: Point<f64, Logical>,
) -> Point<f64, Logical> {
    let (lo, hi) = (rect.loc.to_f64(), (rect.loc + rect.size).to_f64());
    let inside = current.x >= lo.x && current.x < hi.x && current.y >= lo.y && current.y < hi.y;
    if inside {
        clamp_into(proposed, rect)
    } else {
        proposed
    }
}

/// The eligibility conditions that fail, named for the diagnostic log (empty: eligible).
pub fn failed_gates(
    relative_motion: bool,
    modal: bool,
    keyboard_on_surface: bool,
    pointer_in_region: bool,
) -> Vec<&'static str> {
    [
        (!relative_motion, "no-relative-motion"),
        (modal, "modal"),
        (!keyboard_on_surface, "keyboard-elsewhere"),
        (!pointer_in_region, "pointer-outside-region"),
    ]
    .into_iter()
    .filter_map(|(failed, name)| failed.then_some(name))
    .collect()
}

/// Whether `hint` (surface-local) lies inside a surface of `size`, i.e. may be applied on unlock.
pub fn hint_applies(hint: Point<f64, Logical>, size: smithay::utils::Size<i32, Logical>) -> bool {
    Rectangle::from_size(size).contains(hint.to_i32_floor())
}

/// The globals plus CDE's constraint bookkeeping.
#[derive(Debug)]
pub struct PointerLockState {
    /// `zwp_relative_pointer_manager_v1` global.
    pub relative_pointer: RelativePointerManagerState,
    /// `zwp_pointer_constraints_v1` global.
    pub constraints: PointerConstraintsState,
    /// The latest committed cursor-position hint per locked surface (surface-local).
    hints: HashMap<ObjectId, (WlSurface, Point<f64, Logical>)>,
    /// The surface whose constraint CDE activated, with its last known global origin.
    active: Option<(WlSurface, Point<f64, Logical>)>,
    /// The last reconcile outcome logged, so repeated identical outcomes log once.
    last_logged: Option<String>,
    /// The fullscreen confinement rectangle last logged (`None`: off), so only changes log.
    last_fullscreen_confine: Option<Rectangle<i32, Logical>>,
}

impl PointerLockState {
    /// Creates both globals.
    pub fn new<B: Backend + 'static>(dh: &DisplayHandle) -> Self {
        Self {
            relative_pointer: RelativePointerManagerState::new::<CrucibleState<B>>(dh),
            constraints: PointerConstraintsState::new::<CrucibleState<B>>(dh),
            hints: HashMap::new(),
            active: None,
            last_logged: None,
            last_fullscreen_confine: None,
        }
    }
}

/// The topmost parent of `surface` (itself when it is not a subsurface).
fn root_surface(surface: &WlSurface) -> WlSurface {
    let mut root = surface.clone();
    while let Some(parent) = get_parent(&root) {
        root = parent;
    }
    root
}

fn kind_of(constraint: &PointerConstraint) -> ConstraintKind {
    match constraint {
        PointerConstraint::Locked(_) => ConstraintKind::Locked,
        PointerConstraint::Confined(_) => ConstraintKind::Confined,
    }
}

impl<B: Backend + 'static> CrucibleState<B> {
    /// The full global rect of the output showing the keyboard-focused fullscreen window, or
    /// `None` when the focus is not a mapped window that is its output's shown fullscreen window.
    /// Deliberately the output's rect, never the element's client-backed geometry.
    fn focused_fullscreen_output_rect(&self) -> Option<Rectangle<i32, Logical>> {
        let focus = self.seat.get_keyboard()?.current_focus()?;
        let crate::focus::KeyboardFocusTarget::Window(element) = focus else {
            return None;
        };
        let id = element.window_id()?;
        if !self.layout.world.is_fullscreen(id) {
            return None;
        }
        self.space.element_location(&element)?;
        self.raw_output_rects()
            .into_iter()
            .find(|(o, _)| self.layout.world.output_fullscreen(*o) == Some(id))
            .map(|(_, r)| Rectangle::new((r.x, r.y).into(), (r.width, r.height).into()))
    }

    /// Where a relative motion is confined by a focused fullscreen window (see
    /// [`fullscreen_confine_rect`]); logs (debug, deduplicated) when confinement switches.
    pub(crate) fn fullscreen_confine(&mut self) -> Option<Rectangle<i32, Logical>> {
        let mode = self.input_mode();
        let modal = mode.is_locked() || mode.has_overlay();
        let rect = fullscreen_confine_rect(
            B::HAS_RELATIVE_MOTION,
            modal,
            self.config.confine_pointer_to_fullscreen,
            self.focused_fullscreen_output_rect(),
        );
        if rect != self.pointer_lock.last_fullscreen_confine {
            match rect {
                Some(r) => debug!(rect = ?r, "fullscreen pointer confinement on"),
                None => debug!("fullscreen pointer confinement off"),
            }
            self.pointer_lock.last_fullscreen_confine = rect;
        }
        rect
    }

    /// The active constraint on the pointer surface at `under`: kind, region and surface rectangle.
    pub(crate) fn active_constraint(
        &self,
        surface: &WlSurface,
        origin: Point<f64, Logical>,
    ) -> Option<(
        ConstraintKind,
        Option<RegionAttributes>,
        Rectangle<i32, Logical>,
    )> {
        if !B::HAS_RELATIVE_MOTION {
            return None;
        }
        let pointer = self.pointer.clone();
        let found = with_pointer_constraint(surface, &pointer, |c| {
            c.filter(|c| c.is_active())
                .map(|c| (kind_of(&c), c.region().cloned()))
        })?;
        let rect = bbox_from_surface_tree(surface, origin.to_i32_round());
        let rect = Rectangle::new(origin.to_i32_round(), rect.size);
        Some((found.0, found.1, rect))
    }

    /// Activates or deactivates the constraint on the pointer-focused surface per the module-level
    /// policy. `keyboard` is the current keyboard focus surface (passed in because this runs from
    /// `SeatHandler::focus_changed`, where the keyboard handle is busy).
    pub(crate) fn reconcile_pointer_constraint(&mut self, keyboard: Option<WlSurface>) {
        let pointer = self.pointer.clone();
        let mode = self.input_mode();
        let modal = mode.is_locked() || mode.has_overlay();
        let location = pointer.current_location();

        // A constraint CDE activated on a surface that lost pointer focus was deactivated by
        // Smithay on leave; forget it (no hint: the pointer already moved on).
        let focus = pointer
            .current_focus()
            .and_then(|f| f.wl_surface().map(|s| s.into_owned()));
        if let Some((active, _)) = &self.pointer_lock.active
            && (!active.is_alive() || focus.as_ref() != Some(active))
        {
            debug!(
                surface = ?active.id(),
                alive = active.is_alive(),
                focus = ?focus.as_ref().map(Resource::id),
                location = ?location,
                "pointer constraint forgotten: pointer focus left the constrained surface"
            );
            self.pointer_lock.active = None;
            self.pointer_lock.last_logged = None;
        }
        let Some(surface) = focus else { return };

        // The surface's global origin: from the space when the pointer resolves to it, else the
        // last one recorded at activation.
        let origin = self
            .surface_origin_under(location)
            .filter(|(s, _)| *s == surface)
            .map(|(_, o)| o)
            .or_else(|| {
                self.pointer_lock
                    .active
                    .as_ref()
                    .filter(|(s, _)| *s == surface)
                    .map(|(_, o)| *o)
            });

        let keyboard_on_surface = keyboard
            .as_ref()
            .is_some_and(|k| root_surface(k) == root_surface(&surface));
        let in_region = |region: Option<&RegionAttributes>| match origin {
            Some(o) => region.is_none_or(|r| r.contains((location - o).to_i32_floor())),
            None => false,
        };

        let mut deactivated = false;
        let mut activated = false;
        let mut seen: Option<(ConstraintKind, Option<RegionAttributes>, Vec<&'static str>)> = None;
        with_pointer_constraint(&surface, &pointer, |c| {
            let Some(c) = c else { return };
            let pointer_in_region = c.is_active() || in_region(c.region());
            let gates = (
                B::HAS_RELATIVE_MOTION,
                modal,
                keyboard_on_surface,
                pointer_in_region,
            );
            let eligible = constraint_eligible(gates.0, gates.1, gates.2, gates.3);
            seen = Some((
                kind_of(&c),
                c.region().cloned(),
                failed_gates(gates.0, gates.1, gates.2, gates.3),
            ));
            if eligible {
                c.activate();
                activated = true;
            } else if c.is_active() {
                c.deactivate();
                deactivated = true;
            }
        });
        if let Some((kind, region, failed)) = seen
            && tracing::enabled!(tracing::Level::DEBUG)
        {
            let id = surface.id();
            let line = if activated {
                format!(
                    "pointer constraint active: {kind:?} on {id:?}, origin {origin:?}, region {region:?}"
                )
            } else {
                let verb = if deactivated {
                    "deactivated"
                } else {
                    "refused"
                };
                format!(
                    "pointer constraint {verb}: {kind:?} on {id:?}, failed {failed:?}, origin {origin:?}, keyboard {:?}",
                    keyboard.as_ref().map(Resource::id)
                )
            };
            self.log_constraint_once(line);
        }
        if activated {
            if let Some(o) = origin {
                self.pointer_lock.active = Some((surface, o));
            }
        } else if deactivated {
            self.apply_unlock_hint(&surface);
            self.pointer_lock.active = None;
        }
    }

    /// Logs a reconcile outcome at `debug` unless it repeats the last one logged.
    fn log_constraint_once(&mut self, line: String) {
        if self.pointer_lock.last_logged.as_deref() != Some(line.as_str()) {
            debug!("{line}");
            self.pointer_lock.last_logged = Some(line);
        }
    }

    /// The pointer-focusable surface under `location` and its global origin (ignoring modals).
    fn surface_origin_under(
        &self,
        location: Point<f64, Logical>,
    ) -> Option<(WlSurface, Point<f64, Logical>)> {
        let (window, win_loc) = self.space.element_under(location)?;
        let (target, offset) = window.surface_under(
            location - win_loc.to_f64(),
            smithay::desktop::WindowSurfaceType::ALL,
        )?;
        let surface = target.wl_surface()?.into_owned();
        Some((surface, (offset + win_loc).to_f64()))
    }

    /// Moves the pointer to `surface`'s latest cursor-position hint (if it lies inside the
    /// surface), silently: no motion event, no focus change.
    fn apply_unlock_hint(&mut self, surface: &WlSurface) {
        let Some(hint) = self.pointer_lock.hints.get(&surface.id()).map(|(_, h)| *h) else {
            return;
        };
        let Some(origin) = self
            .pointer_lock
            .active
            .as_ref()
            .filter(|(s, _)| s == surface)
            .map(|(_, o)| *o)
        else {
            return;
        };
        let size = bbox_from_surface_tree(surface, (0, 0)).size;
        if hint_applies(hint, size) {
            self.pointer.set_location(hint + origin);
        }
    }

    /// Records the origin of the surface CDE keeps a constraint active on (motion keeps it fresh).
    pub(crate) fn note_constraint_origin(
        &mut self,
        surface: &WlSurface,
        origin: Point<f64, Logical>,
    ) {
        if let Some((s, o)) = &mut self.pointer_lock.active
            && s == surface
        {
            *o = origin;
        }
    }
}

impl<B: Backend + 'static> PointerConstraintsHandler for CrucibleState<B> {
    fn new_constraint(&mut self, surface: &WlSurface, pointer: &PointerHandle<Self>) {
        let detail = with_pointer_constraint(surface, pointer, |c| {
            c.map(|c| (kind_of(&c), c.region().cloned()))
        });
        let focus = pointer
            .current_focus()
            .and_then(|f| f.wl_surface().map(|s| s.id()));
        debug!(
            surface = ?surface.id(),
            constraint = ?detail,
            pointer_focus = ?focus,
            location = ?pointer.current_location(),
            "pointer constraint created"
        );
        self.pointer_lock.last_logged = None;
        let keyboard = self
            .seat
            .get_keyboard()
            .and_then(|k| k.current_focus())
            .and_then(|f| f.wl_surface().map(|s| s.into_owned()));
        self.reconcile_pointer_constraint(keyboard);
    }

    fn remove_constraint(&mut self, surface: &WlSurface, _pointer: &PointerHandle<Self>) {
        debug!(
            surface = ?surface.id(),
            was_active = self
                .pointer_lock
                .active
                .as_ref()
                .is_some_and(|(s, _)| s == surface),
            "pointer constraint removed"
        );
        self.pointer_lock.last_logged = None;
        // Smithay already dropped the constraint; CDE's own record says whether it was active.
        if self
            .pointer_lock
            .active
            .as_ref()
            .is_some_and(|(s, _)| s == surface)
        {
            self.apply_unlock_hint(surface);
            self.pointer_lock.active = None;
        }
        self.pointer_lock.hints.remove(&surface.id());
    }

    fn cursor_position_hint(
        &mut self,
        surface: &WlSurface,
        _pointer: &PointerHandle<Self>,
        location: Point<f64, Logical>,
    ) {
        let hints = &mut self.pointer_lock.hints;
        hints.retain(|_, (s, _)| s.is_alive());
        hints.insert(surface.id(), (surface.clone(), location));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use smithay::wayland::compositor::RectangleKind;

    fn surface() -> Rectangle<i32, Logical> {
        Rectangle::new((100, 50).into(), (200, 100).into())
    }

    fn p(x: f64, y: f64) -> Point<f64, Logical> {
        Point::from((x, y))
    }

    fn out0() -> Rectangle<i32, Logical> {
        Rectangle::new((0, 0).into(), (3840, 2160).into())
    }

    #[test]
    fn fullscreen_confine_eligible() {
        assert_eq!(
            fullscreen_confine_rect(true, false, true, Some(out0())),
            Some(out0())
        );
    }

    #[test]
    fn fullscreen_confine_each_failing_condition() {
        assert_eq!(
            fullscreen_confine_rect(false, false, true, Some(out0())),
            None
        );
        assert_eq!(
            fullscreen_confine_rect(true, true, true, Some(out0())),
            None
        );
        assert_eq!(
            fullscreen_confine_rect(true, false, false, Some(out0())),
            None
        );
        assert_eq!(fullscreen_confine_rect(true, false, true, None), None);
    }

    #[test]
    fn fullscreen_clamp_inside_stays_put() {
        assert_eq!(
            clamp_to_fullscreen(out0(), p(100.0, 100.0), p(150.5, 90.25)),
            p(150.5, 90.25)
        );
    }

    #[test]
    fn fullscreen_clamp_right_edge_is_exclusive() {
        let r = clamp_to_fullscreen(out0(), p(3800.0, 10.0), p(4000.0, 10.0));
        assert!(r.x < 3840.0 && r.x > 3839.0);
        assert_eq!(r.y, 10.0);
    }

    #[test]
    fn fullscreen_clamp_left_edge() {
        let r = clamp_to_fullscreen(out0(), p(5.0, 10.0), p(-40.0, 10.0));
        assert_eq!(r, p(0.0, 10.0));
    }

    #[test]
    fn fullscreen_clamp_from_outside_passes_through() {
        assert_eq!(
            clamp_to_fullscreen(out0(), p(4000.0, 100.0), p(-50.0, 100.0)),
            p(-50.0, 100.0)
        );
    }

    #[test]
    fn fullscreen_clamp_two_outputs_stays_on_first() {
        // Outputs at (0,0) and (3840,0), both 3840x2160; the fullscreen window is on the first.
        let r = clamp_to_fullscreen(out0(), p(3830.0, 100.0), p(3880.0, 100.0));
        assert!(r.x < 3840.0, "stayed on the first output: {r:?}");
    }

    #[test]
    fn fullscreen_clamp_second_output_rect_blocks_left_crossing() {
        let second = Rectangle::new((3840, 0).into(), (3840, 2160).into());
        let r = clamp_to_fullscreen(second, p(3845.0, 100.0), p(3800.0, 100.0));
        assert_eq!(r, p(3840.0, 100.0));
    }

    #[test]
    fn no_constraint_moves_to_proposed() {
        let out = constrained_motion(None, None, surface(), p(0.0, 0.0), p(5.0, 7.0));
        assert_eq!(out, MotionOutcome::Move(p(5.0, 7.0)));
    }

    #[test]
    fn lock_suppresses_motion() {
        let out = constrained_motion(
            Some(ConstraintKind::Locked),
            None,
            surface(),
            p(150.0, 80.0),
            p(400.0, 400.0),
        );
        assert_eq!(out, MotionOutcome::Suppress);
    }

    #[test]
    fn confine_inside_moves_freely() {
        let out = constrained_motion(
            Some(ConstraintKind::Confined),
            None,
            surface(),
            p(150.0, 80.0),
            p(160.0, 90.0),
        );
        assert_eq!(out, MotionOutcome::Move(p(160.0, 90.0)));
    }

    fn confined_to(proposed: Point<f64, Logical>) -> Point<f64, Logical> {
        match constrained_motion(
            Some(ConstraintKind::Confined),
            None,
            surface(),
            p(150.0, 80.0),
            proposed,
        ) {
            MotionOutcome::Move(loc) => loc,
            MotionOutcome::Suppress => panic!("confine never suppresses"),
        }
    }

    #[test]
    fn confine_clamps_left_edge() {
        assert_eq!(confined_to(p(10.0, 80.0)), p(100.0, 80.0));
    }

    #[test]
    fn confine_clamps_top_edge() {
        assert_eq!(confined_to(p(150.0, 0.0)), p(150.0, 50.0));
    }

    #[test]
    fn confine_clamps_right_edge() {
        let loc = confined_to(p(900.0, 80.0));
        assert!(loc.x < 300.0 && loc.x > 299.9, "{loc:?}");
        assert_eq!(loc.y, 80.0);
    }

    #[test]
    fn confine_clamps_bottom_edge() {
        let loc = confined_to(p(150.0, 900.0));
        assert!(loc.y < 150.0 && loc.y > 149.9, "{loc:?}");
        assert_eq!(loc.x, 150.0);
    }

    #[test]
    fn confine_clamps_into_region() {
        let region = RegionAttributes {
            rects: vec![(
                RectangleKind::Add,
                Rectangle::new((10, 10).into(), (20, 20).into()),
            )],
        };
        let out = constrained_motion(
            Some(ConstraintKind::Confined),
            Some(&region),
            surface(),
            p(115.0, 65.0),
            p(200.0, 65.0),
        );
        let MotionOutcome::Move(loc) = out else {
            panic!("confine never suppresses")
        };
        assert!(loc.x < 130.0 && loc.x > 129.9, "{loc:?}");
        assert_eq!(loc.y, 65.0);
    }

    #[test]
    fn confine_with_empty_region_stays_put() {
        let region = RegionAttributes { rects: vec![] };
        let out = constrained_motion(
            Some(ConstraintKind::Confined),
            Some(&region),
            surface(),
            p(150.0, 80.0),
            p(160.0, 80.0),
        );
        assert_eq!(out, MotionOutcome::Move(p(150.0, 80.0)));
    }

    #[test]
    fn eligibility_needs_every_condition() {
        assert!(constraint_eligible(true, false, true, true));
        assert!(
            !constraint_eligible(false, false, true, true),
            "winit: never"
        );
        assert!(!constraint_eligible(true, true, true, true), "modal");
        assert!(
            !constraint_eligible(true, false, false, true),
            "keyboard left"
        );
        assert!(
            !constraint_eligible(true, false, true, false),
            "outside region"
        );
    }

    #[test]
    fn failed_gates_names_each_failing_condition() {
        assert!(failed_gates(true, false, true, true).is_empty());
        assert_eq!(
            failed_gates(false, true, false, false),
            vec![
                "no-relative-motion",
                "modal",
                "keyboard-elsewhere",
                "pointer-outside-region"
            ]
        );
        assert_eq!(
            failed_gates(true, false, false, true),
            vec!["keyboard-elsewhere"]
        );
    }

    #[test]
    fn hint_applies_only_inside_surface() {
        let size = (200, 100).into();
        assert!(hint_applies(p(10.0, 10.0), size));
        assert!(!hint_applies(p(-1.0, 10.0), size));
        assert!(!hint_applies(p(200.0, 10.0), size));
    }
}
