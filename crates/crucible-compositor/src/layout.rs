//! The bridge between the pure [`crucible_layout`] model and the Smithay [`Space`].
//!
//! `crucible-layout` is the single source of truth for geometry, focus, stacking, and workspace
//! membership. This module owns:
//!
//! - the [`World`] (the model) plus the payload bindings — [`LayoutState`], which holds the stable
//!   [`WindowId`] ↔ element map for **managed** windows and the flat set of **unmanaged** elements
//!   (X11 override-redirect surfaces, which deliberately live outside the `World`);
//! - the **lifecycle controller**: [`CrucibleState::map_managed_window`],
//!   [`CrucibleState::unmap_matching`], [`CrucibleState::end_workspace`],
//!   [`CrucibleState::map_unmanaged_element`] and the output registration methods. Per-window
//!   metadata ([`WindowMeta`]: pid and class) is bound with the element at open and released with
//!   it, so it is known before the first placement and never outlives the window. Both shell
//!   backends (xdg-shell and XWayland) go through these; no backend sequences model/map/`Space`
//!   transitions by hand;
//! - [`CrucibleState::apply_layout`], which derives the [`Projection`] from the model, diffs it
//!   against the last applied one, and applies exactly that difference to the `Space`.
//!
//! ## One projection, no hints
//! Every mutating `World` op returns only the identity it created (or `Result<(), LayoutError>`).
//! What the compositor must show is *derived*: [`World::project`] produces the visible
//! `(WindowId, Rect)` set plus the focused identity, and [`Projection::diff`] turns
//! (previous, next) into the minimal [`LayoutChange`] list plus a [`FocusChange`]. Applying an
//! unchanged projection is a no-op — no configures, no focus churn.
//!
//! ## Lifecycle commit order (the one order every transition follows)
//! 1. **Model first.** Validate and mutate the [`World`]. This is the only step that may fail; on
//!    failure the transition aborts and **no** payload state (element bindings, `Space`) has been
//!    touched.
//! 2. **Payload second.** Insert/remove the element binding and map/unmap it in the `Space`. These
//!    steps cannot fail.
//! 3. **Project last.** Re-derive the projection and apply the diff (geometry, then focus).
//!
//! Protocol I/O (an X11 `set_mapped`, a `configure`) is *recoverable*: it is logged and the
//! transition continues or aborts cleanly. That is distinct from an invariant violation — a
//! disagreement between the `World` and the bindings — which is logged as a warning and never
//! silently repaired by mutating payload state behind the model's back.
//!
//! ## Visibility (decision #4)
//! Only the active workspace's panes, and only each pane's focused window, are in the projection and
//! therefore mapped into the `Space`. Non-visible windows are *unmapped* (`Space::unmap_elem`) but
//! **kept alive** in the binding map so re-showing them (stack cycle / workspace switch) is just a
//! re-`map_element` + configure — no surface teardown.
//!
//! ## Multi-output: independent per-monitor workspaces
//! Each physical Smithay [`Output`](smithay::output::Output) is mapped 1:1 to a model
//! [`crucible_layout::OutputId`], stashed in the Smithay output's `user_data()` (see
//! [`model_output_id`] / [`set_model_output_id`]). The **first** compositor output adopts the
//! existing `World::new` output (seeded at startup off the saved names); each subsequent connector
//! calls [`World::add_output`] with a seed named the same way. The
//! projection asks the model for *each* output's own active workspace, so each monitor shows an
//! independent workspace stack.

mod bindings;
mod outputs;
mod placement;

pub use bindings::{Binding, LayoutState, Payload, Teardown, WindowMeta};
pub use outputs::{model_output_id, set_model_output_id};
pub use placement::{NO_GAPS, WindowClass, clamped_gap, window_rect};
use placement::{configure_placement, suspend_toplevel};

use crucible_layout::{
    FocusChange, LayoutChange, LayoutError, PaneId, Projection, Rect, WindowId, WorkspaceId, World,
};
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::utils::{Logical, Point};
use tracing::{debug, warn};

use crate::element::WindowElement;
use crate::focus::KeyboardFocusTarget;
use crate::state::{Backend, CrucibleState};

/// What keyboard focus must do to match the model after a pointer-driven model focus change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyboardSync {
    /// Keyboard focus already matches the model.
    Keep,
    /// The model focuses an empty pane: clear keyboard focus.
    Clear,
    /// Move keyboard focus to this managed window.
    Focus(WindowId),
}

/// Decides how to reconcile keyboard focus with the model's focused window.
///
/// `keyboard_has_focus` is whether anything (managed or unmanaged) holds keyboard focus;
/// `keyboard_window` is the managed window id it holds, if any. Needed because `apply_layout` only
/// re-points keyboard focus when the *projected* focus changes, which focusing an empty pane from
/// another empty pane (or from an unmanaged element) does not.
#[must_use]
pub fn keyboard_sync(
    model: Option<WindowId>,
    keyboard_has_focus: bool,
    keyboard_window: Option<WindowId>,
) -> KeyboardSync {
    match model {
        None if keyboard_has_focus => KeyboardSync::Clear,
        None => KeyboardSync::Keep,
        Some(id) if keyboard_window == Some(id) => KeyboardSync::Keep,
        Some(id) => KeyboardSync::Focus(id),
    }
}

impl<B: Backend + 'static> CrucibleState<B> {
    /// Derives the current [`Projection`], applies the difference from the last one, and stores it.
    ///
    /// This is the *only* path from model to `Space`. Step 3 of the commit order; safe to call at
    /// any time (the per-frame backends do) because an unchanged projection produces an empty diff
    /// and therefore no configures and no focus churn.
    pub fn apply_layout(&mut self) {
        let bindings = self.output_bindings();
        let next: Projection = self.layout.world.project(&bindings, NO_GAPS);
        // Last-viewed outputs follow what is shown, so note them even when the diff is empty.
        self.note_last_viewed_outputs();
        let diff = self.projection.diff(&next);
        if diff.is_empty() {
            self.hover_after_workspace_switch();
            return;
        }

        for change in &diff.changes {
            match change {
                LayoutChange::Place {
                    id,
                    rect,
                    resized,
                    fullscreen,
                } => {
                    let Some(element) = self.layout.element(*id).cloned() else {
                        continue;
                    };
                    // The projection holds the tile; the window sits inside it per its class.
                    let logical = self.placed_rect(*id, *rect, *fullscreen);
                    // Configure first, then position via map_element so the element is mapped (or
                    // re-mapped, for a window returning from hidden) at the right place.
                    let revealed = configure_placement(&element, logical, *resized, *fullscreen);
                    self.space.map_element(element.clone(), logical.loc, false);
                    if let Some(serial) = revealed {
                        self.begin_reveal_hold(&element, serial);
                    }
                }
                LayoutChange::Hide { id } => {
                    if let Some(element) = self.layout.element(*id).cloned() {
                        self.space.unmap_elem(&element);
                        // After the unmap: tell the client it is no longer visible.
                        suspend_toplevel(&element);
                        // Hidden again: nothing left to wait for.
                        if let Some(root) = element.wl_surface() {
                            self.forget_reveal(&root);
                        }
                    }
                }
                LayoutChange::ShellState { id, fullscreen } => {
                    // A hidden window's state flips without a placement (after its `Hide`).
                    if let Some(element) = self.layout.element(*id) {
                        placement::set_fullscreen_state(element, *fullscreen);
                    }
                }
            }
        }

        if let FocusChange::To(target) = diff.focus {
            self.apply_focus(target);
        }

        // Locked > Overlay > Normal: a (re)placed fullscreen window must not cover a mapped
        // overlay or take its keyboard focus. The lock needs nothing here: render and input
        // resolve it before the `Space`.
        // Fullscreen is involved when either side shows or holds one (a flip implies that).
        let fullscreen_involved = self.projection.has_fullscreen() || next.has_fullscreen();
        if fullscreen_involved && let Some(overlay) = self.mapped_overlay().cloned() {
            self.space.raise_element(&overlay, true);
            self.set_keyboard_focus(Some(KeyboardFocusTarget::Window(overlay)));
        }

        self.projection = next;
        // After the Space matches the model, so the pointer resolves against what is shown.
        self.hover_after_workspace_switch();
    }

    /// The pane under global pointer `location` on any bound output's active workspace, using the
    /// same bindings as the projection. `None` off every output.
    pub(crate) fn pane_under(&self, location: Point<f64, Logical>) -> Option<PaneId> {
        self.layout.world.pane_at(
            &self.output_bindings(),
            NO_GAPS,
            location.x.floor() as i32,
            location.y.floor() as i32,
        )
    }

    /// The global rect of the focused pane when it holds no window (for the empty-pane focus
    /// ring), from the same bindings as the projection.
    pub(crate) fn focused_empty_pane_rect(&self) -> Option<Rect> {
        let pane = self.layout.world.focused_empty_pane()?;
        self.layout
            .world
            .pane_rect(&self.output_bindings(), NO_GAPS, pane)
    }

    /// Forgets the applied projection so the next [`Self::apply_layout`] re-places (and
    /// re-configures) every window instead of diffing to nothing.
    pub(crate) fn invalidate_projection(&mut self) {
        self.projection = Projection::default();
    }

    /// Makes keyboard focus match the model's focused window (see [`keyboard_sync`]).
    pub(crate) fn reconcile_keyboard_focus(&mut self) {
        let current = self.keyboard_focus();
        let keyboard_window = match &current {
            Some(KeyboardFocusTarget::Window(w)) => w.window_id(),
            _ => None,
        };
        match keyboard_sync(
            self.layout.world.focused_window(),
            current.is_some(),
            keyboard_window,
        ) {
            KeyboardSync::Keep => {}
            KeyboardSync::Clear => self.apply_focus(None),
            KeyboardSync::Focus(id) => self.apply_focus(Some(id)),
        }
    }

    /// Re-points keyboard focus (and X11 focus) at the projection's focused window.
    fn apply_focus(&mut self, focused: Option<WindowId>) {
        let Some(focused) = focused else {
            // No focused window (e.g. the active workspace is empty): clear focus.
            self.set_keyboard_focus(None);
            return;
        };
        let Some(element) = self.layout.element(focused).cloned() else {
            return;
        };
        // Raise + X11-activate via the existing focus path, then assign keyboard focus.
        self.space.raise_element(&element, true);
        if let Some(surface) = element.x11_surface() {
            if let Some(xwm) = self.xwm.as_mut() {
                let _ = xwm.raise_window(surface);
            }
        }
        self.set_keyboard_focus(Some(KeyboardFocusTarget::Window(element)));
    }

    // ---- the single mutation entry point ----------------------------------------------------

    /// Runs a fallible model mutation and applies the resulting projection **only on success**.
    ///
    /// The single entry point for every model mutation (steps 1 and 3 of the commit order): an
    /// `Err` means the model rejected the change, nothing was mutated, and nothing is applied. The
    /// error is returned to the caller untouched so it can distinguish an expected no-op (e.g.
    /// [`LayoutError::SameWorkspace`]) from a real problem.
    pub fn with_model<F>(&mut self, op: F) -> Result<(), LayoutError>
    where
        F: FnOnce(&mut World) -> Result<(), LayoutError>,
    {
        op(&mut self.layout.world)?;
        self.apply_layout();
        Ok(())
    }

    /// Infallible-op sugar over [`CrucibleState::with_model`], for `World` verbs that cannot fail.
    pub fn update_model<F>(&mut self, op: F)
    where
        F: FnOnce(&mut World),
    {
        let outcome = self.with_model(|world| {
            op(world);
            Ok(())
        });
        debug_assert!(outcome.is_ok(), "infallible model op cannot fail");
    }

    /// Routes a client fullscreen request (`on`) or unset (`!on`) for the managed window matching
    /// `pred` into the model, then applies the layout. Returns whether the model state changed; a
    /// window CDE does not manage is ignored (`false`). The window fullscreens on its own
    /// workspace's output.
    pub(crate) fn request_fullscreen(
        &mut self,
        pred: impl Fn(&WindowElement) -> bool,
        on: bool,
    ) -> bool {
        let Binding::Managed(id) = self.layout.resolve(pred) else {
            debug!("fullscreen request from an unmanaged window; ignoring");
            return false;
        };
        let changed = self.layout.world.set_fullscreen(id, on).unwrap_or(false);
        self.apply_layout();
        changed
    }

    // ---- window lifecycle controller ---------------------------------------------------------

    /// Maps a **managed** window: model open, bind the element with its `meta`, apply the
    /// projection.
    ///
    /// The projection — not the caller — decides where and at what size the window lands, and
    /// whether it takes focus. `meta` is bound **before** the first [`Self::apply_layout`], so the
    /// class the placement reads is known in time. Returns the model [`WindowId`] now bound to
    /// `element`.
    pub fn map_managed_window(&mut self, element: WindowElement, meta: WindowMeta) -> WindowId {
        let id = self.layout.open_managed(element, meta);
        self.apply_layout();
        id
    }

    /// Maps a **managed** window into `pane` at stack `index` (a restored window matched to its
    /// pending placement). Same commit order as [`Self::map_managed_window`]; on a model error
    /// nothing is bound or mapped.
    pub fn map_managed_window_at(
        &mut self,
        element: WindowElement,
        meta: WindowMeta,
        pane: PaneId,
        index: usize,
    ) -> Result<WindowId, LayoutError> {
        let id = self.layout.open_managed_at(element, meta, pane, index)?;
        self.apply_layout();
        Ok(id)
    }

    /// Removes workspace `ws` and tears down every window it held, in the commit order: the model
    /// drops the workspace and its bindings ([`LayoutState::release_workspace`]), each element is
    /// unmapped from the `Space`, then the projection is applied. Returns the ended elements with
    /// their metadata so the caller can decide how to end each client. On a model error nothing
    /// changes and the error is returned.
    pub fn end_workspace(
        &mut self,
        ws: WorkspaceId,
    ) -> Result<Vec<(WindowElement, WindowMeta)>, LayoutError> {
        let released = self.layout.release_workspace(ws)?;
        let ended = released
            .into_iter()
            .map(|(_, element, meta)| {
                self.space.unmap_elem(&element);
                (element, meta)
            })
            .collect();
        self.apply_layout();
        Ok(ended)
    }

    /// Maps an **unmanaged** element (X11 override-redirect) at `location`.
    ///
    /// Unmanaged elements place themselves and are deliberately absent from the [`World`], so the
    /// projection never moves or hides them. They are tracked here so
    /// [`CrucibleState::unmap_matching`] can tear them down symmetrically. Re-mapping an already
    /// tracked element only refreshes its position.
    pub fn map_unmanaged_element(&mut self, element: WindowElement, location: Point<i32, Logical>) {
        self.layout.track_unmanaged(element.clone());
        self.space.map_element(element, location, true);
    }

    /// Tears down whichever element matches `pred`, managed or unmanaged, in the commit order.
    ///
    /// Resolves the owner first, so a repeated unmap/destroy resolves to [`Binding::None`] and
    /// returns [`Teardown::Absent`] — no error, no log storm. For a managed window the model close
    /// runs first; only if it succeeds are the binding and the `Space` mapping released.
    /// A managed close then runs [`Self::managed_closed`] (recovery record, ephemeral prune), so
    /// every close path — Wayland destroy, X11 unmap or destroy-without-unmap — gets it once.
    pub fn unmap_matching(&mut self, pred: impl Fn(&WindowElement) -> bool) -> Teardown {
        let teardown = self.teardown_matching(pred);
        if let Teardown::Managed(id) = teardown {
            self.managed_closed(id);
        }
        teardown
    }

    /// [`Self::unmap_matching`] without the managed-close hook: tears the element down but leaves
    /// the recovery record and ephemeral workspaces alone. Only the lost-window path uses it, since
    /// its placeholder is not yet pending and a prune would remove the workspace it stands in for.
    pub(crate) fn teardown_matching(&mut self, pred: impl Fn(&WindowElement) -> bool) -> Teardown {
        match self.layout.resolve(&pred) {
            Binding::Managed(id) => match self.layout.close_managed(id) {
                Ok(released) => {
                    if let Some((element, _)) = released {
                        self.space.unmap_elem(&element);
                    }
                    self.apply_layout();
                    Teardown::Managed(id)
                }
                Err(err) => {
                    // The binding map and the model disagree: an invariant violation, not protocol
                    // I/O. Report it and change nothing rather than repairing state behind the
                    // model's back.
                    warn!(%err, ?id, "bound window unknown to the model; leaving state untouched");
                    Teardown::Absent
                }
            },
            Binding::Unmanaged => {
                if let Some(element) = self.layout.untrack_unmanaged(&pred) {
                    self.space.unmap_elem(&element);
                    self.unmanaged_closed(&element);
                }
                Teardown::Unmanaged
            }
            Binding::None => {
                debug!("unmap request for an element that is already gone; ignoring");
                Teardown::Absent
            }
        }
    }
}

/// Reveal-hold wiring (plan 38): the pure model is [`crate::reveal_hold`].
impl<B: Backend + 'static> CrucibleState<B> {
    /// Starts (or extends) a reveal hold for `element`, just re-shown by a configure `serial`
    /// that cleared `Suspended`, on the output it landed on. Only an xdg toplevel whose committed
    /// buffer is a placeholder holds ([`is_placeholder_buffer`]); a client that kept a full-size
    /// buffer (foot) never does. Lock and snip win: no new hold while either is active.
    pub(crate) fn begin_reveal_hold(
        &mut self,
        element: &WindowElement,
        serial: smithay::utils::Serial,
    ) {
        use crate::reveal_hold::{REVEAL_HOLD_TIMEOUT, is_placeholder_buffer};
        use smithay::reexports::calloop::timer::{TimeoutAction, Timer};

        if self.session_lock.is_locked() || self.snip.is_some() {
            return;
        }
        let Some(root) = element.toplevel().map(|t| t.wl_surface().clone()) else {
            return;
        };
        let Some(output) = self.space.outputs_for_element(element).first().cloned() else {
            return;
        };
        let (buffer, surface) =
            smithay::backend::renderer::utils::with_renderer_surface_state(&root, |s| {
                (
                    s.buffer_size().map(|size| (size.w, size.h)),
                    s.surface_size().map(|size| (size.w, size.h)),
                )
            })
            .unwrap_or((None, None));
        if !is_placeholder_buffer(buffer, surface) {
            return;
        }
        let deadline = std::time::Instant::now() + REVEAL_HOLD_TIMEOUT;
        let started = self
            .reveal_holds
            .begin(output.clone(), root, u32::from(serial), deadline);
        if started {
            debug!(output = %output.name(), "reveal hold: begun");
            let key = output;
            let armed =
                self.handle
                    .insert_source(Timer::from_deadline(deadline), move |_, (), data| {
                        data.expire_reveal_holds();
                        // A stale timer (its hold already released) finds nothing held and ends; a
                        // timer that fired a hair early re-arms for the hold's real deadline.
                        match data.reveal_holds.deadline(&key) {
                            Some(next) => TimeoutAction::ToInstant(next),
                            None => TimeoutAction::Drop,
                        }
                    });
            if let Err(err) = armed {
                // Without its timer the hold could never end: do not hold.
                warn!(%err, "reveal hold: cannot arm the deadline timer; not holding");
                self.clear_reveal_holds();
            }
        }
    }

    /// Releases every hold past its deadline.
    pub(crate) fn expire_reveal_holds(&mut self) {
        let released = self.reveal_holds.expire(std::time::Instant::now());
        self.release_reveal_holds(released);
    }

    /// A root surface committed with the committed configure `serial` (see the commit handler).
    pub(crate) fn reveal_commit(&mut self, root: &WlSurface, serial: u32) {
        let released = self.reveal_holds.commit(root, serial);
        self.release_reveal_holds(released);
    }

    /// Forgets `root` in every hold (destroyed, hidden again, closed).
    pub(crate) fn forget_reveal(&mut self, root: &WlSurface) {
        if self.reveal_holds.is_empty() {
            return;
        }
        let released = self.reveal_holds.forget(root);
        self.release_reveal_holds(released);
    }

    /// Lock and snip win: drops every hold.
    pub(crate) fn clear_reveal_holds(&mut self) {
        let released = self.reveal_holds.clear_all();
        self.release_reveal_holds(released);
    }

    /// Every release path ends here: enqueue a real render of each released output (backend hook)
    /// and answer the captures deferred on it. Run from the idle queue so a release inside a
    /// commit or destroy callback never renders or captures re-entrantly.
    fn release_reveal_holds(&mut self, released: Vec<smithay::output::Output>) {
        if released.is_empty() {
            return;
        }
        self.handle.insert_idle(move |data| {
            for output in &released {
                debug!(output = %output.name(), "reveal hold: released");
                B::schedule_render(data, output);
            }
            data.run_deferred_captures(&released);
        });
    }

    /// Whether the render pass should composite `output`: a held output is skipped (it keeps its
    /// previous frame and still gets frame callbacks). The lock screen is never held.
    pub(crate) fn render_admission(
        &self,
        output: &smithay::output::Output,
    ) -> crate::reveal_hold::RenderAdmission {
        if self.session_lock.is_locked() {
            return crate::reveal_hold::RenderAdmission::Render;
        }
        self.reveal_holds.admission(output)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keyboard_sync_clears_for_an_empty_pane_even_from_an_unmanaged_holder() {
        // An unmanaged element holds keyboard focus (no managed id), the model now focuses an
        // empty pane: keyboard focus must be cleared (E2's unmanaged detour).
        assert_eq!(keyboard_sync(None, true, None), KeyboardSync::Clear);
        // Nothing holds focus already: nothing to do.
        assert_eq!(keyboard_sync(None, false, None), KeyboardSync::Keep);
    }

    #[test]
    fn keyboard_sync_focuses_the_model_window_unless_already_held() {
        let a = WindowId::from_raw(1);
        let b = WindowId::from_raw(2);
        assert_eq!(keyboard_sync(Some(a), true, Some(a)), KeyboardSync::Keep);
        assert_eq!(
            keyboard_sync(Some(a), true, Some(b)),
            KeyboardSync::Focus(a)
        );
        assert_eq!(keyboard_sync(Some(a), true, None), KeyboardSync::Focus(a));
        assert_eq!(keyboard_sync(Some(a), false, None), KeyboardSync::Focus(a));
    }
}
