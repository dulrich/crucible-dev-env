//! The bridge between the pure [`crucible_layout`] model and the Smithay [`Space`].
//!
//! `crucible-layout` is the single source of truth for geometry, focus, stacking, and workspace
//! membership (decision #1 in `plans/02-tiling-model.md`). This module owns:
//!
//! - the [`World`] (the model) plus the payload bindings — [`LayoutState`], which holds the stable
//!   [`WindowId`] ↔ element map for **managed** windows and the flat set of **unmanaged** elements
//!   (X11 override-redirect surfaces, which deliberately live outside the `World`);
//! - the **lifecycle controller**: [`CrucibleState::map_managed_window`],
//!   [`CrucibleState::unmap_matching`], [`CrucibleState::map_unmanaged_element`] and the output
//!   registration methods. Both shell backends (xdg-shell and XWayland) go through these; no backend
//!   sequences model/map/`Space` transitions by hand;
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
//! existing `World::new()` output; each subsequent connector calls [`World::add_output`]. The
//! projection asks the model for *each* output's own active workspace, so each monitor shows an
//! independent workspace stack.

use std::collections::{HashMap, HashSet};

use crucible_config::TilingConfig;
use crucible_layout::{
    FocusChange, GapConfig, LayoutChange, LayoutError, OutputId, PaneId, Projection, Rect,
    WindowId, World,
};
use smithay::output::Output;
use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel::State as XdgState;
use smithay::utils::{Logical, Point, Rectangle, Size};
use tracing::{debug, info, warn};

use crate::element::WindowElement;
use crate::focus::KeyboardFocusTarget;
use crate::state::{Backend, CrucibleState};

/// Wrapper stored in a Smithay [`Output`]'s `user_data()` binding it to a model [`OutputId`].
///
/// Using the `UserDataMap` (rather than a parallel `HashMap` in state) keeps the Smithay↔model
/// mapping co-located with the output it describes, so there is nothing to keep in sync on add/remove.
#[derive(Debug, Clone, Copy)]
struct ModelOutputId(OutputId);

/// Returns the model [`OutputId`] bound to `output`, if one has been set.
#[must_use]
pub fn model_output_id(output: &Output) -> Option<OutputId> {
    output.user_data().get::<ModelOutputId>().map(|m| m.0)
}

/// Binds `id` to `output` in its `user_data()` (idempotent: the first binding wins).
pub fn set_model_output_id(output: &Output, id: OutputId) {
    output.user_data().insert_if_missing(|| ModelOutputId(id));
}

/// The compositor-side payload a model window is bound to.
///
/// Abstracting it (rather than hard-coding [`WindowElement`]) lets the lifecycle sequencing in
/// [`LayoutState`] be unit-tested with a stand-in payload — no Smithay surface, no running
/// compositor.
pub trait Payload: Clone + PartialEq {
    /// Stamps the model id onto the payload. The first binding wins.
    fn set_window_id(&self, id: WindowId);
    /// The model id previously stamped onto this payload, if any.
    fn window_id(&self) -> Option<WindowId>;
}

impl Payload for WindowElement {
    fn set_window_id(&self, id: WindowId) {
        WindowElement::set_window_id(self, id);
    }

    fn window_id(&self) -> Option<WindowId> {
        WindowElement::window_id(self)
    }
}

/// Which lifecycle owner holds the element a request refers to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Binding {
    /// A managed window, bound to this model [`WindowId`].
    Managed(WindowId),
    /// An unmanaged element (X11 override-redirect), tracked outside the [`World`].
    Unmanaged,
    /// Nothing matches: the element was never bound, or is already torn down.
    None,
}

/// What an unmap/destroy request actually tore down.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Teardown {
    /// A managed window: closed in the `World`, unbound, and unmapped from the `Space`.
    Managed(WindowId),
    /// An unmanaged element: untracked and unmapped from the `Space`.
    Unmanaged,
    /// Nothing matched — a repeated unmap/destroy. Harmless by construction.
    Absent,
}

/// The model plus the compositor payload bindings.
///
/// Lives in [`CrucibleState`]. The [`World`] is the authority for everything it knows about;
/// `by_id` is the only place a payload for a **managed** window is retained while that window is
/// hidden (and therefore absent from the `Space`). `unmanaged` holds X11 override-redirect elements,
/// which are intentionally *not* modelled in the `World` (they place themselves) but still need a
/// symmetric owner so their unmap/destroy is complete and repeatable.
///
/// The steps here are the fallible/model half of the commit order documented at module level; the
/// `Space` half lives on [`CrucibleState`], which calls these first.
#[derive(Debug)]
pub struct LayoutState<E = WindowElement> {
    /// The pure layout model — single source of truth for geometry/focus/stacking/membership.
    pub world: World,
    /// Managed windows: model id → payload (visible or hidden).
    by_id: HashMap<WindowId, E>,
    /// Unmanaged (override-redirect) payloads, outside the model.
    unmanaged: Vec<E>,
}

impl<E: Payload> Default for LayoutState<E> {
    fn default() -> Self {
        Self::new()
    }
}

impl<E: Payload> LayoutState<E> {
    /// Creates a fresh layout state with the default one-output/one-workspace/one-pane `World`.
    #[must_use]
    pub fn new() -> Self {
        Self {
            world: World::new(),
            by_id: HashMap::new(),
            unmanaged: Vec::new(),
        }
    }

    /// Resolves a model `id` to its payload.
    #[must_use]
    pub fn element(&self, id: WindowId) -> Option<&E> {
        self.by_id.get(&id)
    }

    /// Number of managed windows currently bound (visible *and* hidden).
    #[must_use]
    pub fn managed_count(&self) -> usize {
        self.by_id.len()
    }

    /// The unmanaged (override-redirect) payloads currently tracked.
    #[must_use]
    pub fn unmanaged(&self) -> &[E] {
        &self.unmanaged
    }

    /// Which owner — if any — holds the payload matching `pred`.
    ///
    /// Managed bindings are checked first: a payload can only ever be in one of the two sets.
    pub fn resolve(&self, pred: impl Fn(&E) -> bool) -> Binding {
        if let Some((id, _)) = self.by_id.iter().find(|(_, e)| pred(e)) {
            return Binding::Managed(*id);
        }
        if self.unmanaged.iter().any(&pred) {
            return Binding::Unmanaged;
        }
        Binding::None
    }

    /// Step 1+2 of the commit order for a **map**: open a window in the model, then bind `element`.
    ///
    /// `World::open_window` is infallible, so this cannot leave the pair inconsistent. Returns the
    /// new [`WindowId`]; the caller applies the projection.
    pub fn open_managed(&mut self, element: E) -> WindowId {
        let id = self.world.open_window();
        element.set_window_id(id);
        self.by_id.insert(id, element);
        id
    }

    /// Like [`Self::open_managed`] but opens the window at stack `index` of `pane` (restore
    /// placement). On a model error nothing is bound and the error is returned.
    pub fn open_managed_at(
        &mut self,
        element: E,
        pane: PaneId,
        index: usize,
    ) -> Result<WindowId, LayoutError> {
        let id = self.world.open_window_at(pane, index)?;
        element.set_window_id(id);
        self.by_id.insert(id, element);
        Ok(id)
    }

    /// Step 1+2 of the commit order for an **unmap**: close `id` in the model, then release its
    /// binding.
    ///
    /// The model mutation happens **first and alone**: if it fails, `Err` is returned and neither
    /// the binding map nor (by extension) the `Space` has been touched. On success the released
    /// payload is returned so the caller can unmap it from the `Space`; `Ok(None)` means the model
    /// knew the window but nothing was ever bound to it.
    pub fn close_managed(&mut self, id: WindowId) -> Result<Option<E>, LayoutError> {
        self.world.close_window(id)?;
        Ok(self.by_id.remove(&id))
    }

    /// Drops the binding of a window the model has **already** forgotten (e.g. after
    /// `World::remove_workspace`), returning its payload so the caller can unmap and end it.
    /// `None` when nothing was bound. After this, a later unmap/destroy of the payload resolves to
    /// [`Binding::None`] and is ignored.
    pub fn release_binding(&mut self, id: WindowId) -> Option<E> {
        self.by_id.remove(&id)
    }

    /// Tracks an unmanaged payload. Returns `false` if it was already tracked (idempotent).
    pub fn track_unmanaged(&mut self, element: E) -> bool {
        if self.unmanaged.contains(&element) {
            return false;
        }
        self.unmanaged.push(element);
        true
    }

    /// Untracks the first unmanaged payload matching `pred`, returning it so the caller can unmap
    /// it. `None` when nothing matches — a repeated unmap/destroy is not an error.
    pub fn untrack_unmanaged(&mut self, pred: impl Fn(&E) -> bool) -> Option<E> {
        let idx = self.unmanaged.iter().position(pred)?;
        Some(self.unmanaged.remove(idx))
    }

    // ---- output binding (the model half of output registration) -----------------------------

    /// Chooses the model output that backs a newly registered compositor output and stores `rect`.
    ///
    /// `bound` is the set of model outputs already claimed by live compositor outputs. The first
    /// compositor output adopts the unbound output `World::new()` created; every later one
    /// allocates a fresh model output. Either way the chosen output carries `rect` on return.
    pub fn bind_output(&mut self, bound: &HashSet<OutputId>, rect: Rect) -> OutputId {
        let unbound = self.world.output_ids().find(|id| !bound.contains(id));
        let id = unbound.unwrap_or_else(|| self.world.add_output(rect));
        self.world
            .set_output_rect(id, rect)
            .expect("output was just found or allocated in this model");
        id
    }

    /// Writes `rect` onto model output `id`.
    ///
    /// A compositor output can outlive its model output (the connector is gone but a frame is still
    /// in flight); that surfaces as [`LayoutError::UnknownOutput`] and leaves every surviving
    /// output's rect untouched. The caller decides whether that is worth a warning.
    pub fn update_output_rect(&mut self, id: OutputId, rect: Rect) -> Result<(), LayoutError> {
        self.world.set_output_rect(id, rect)
    }

    /// Releases model output `id`, migrating its workspaces (and their windows) onto a survivor.
    ///
    /// The model always keeps one output: releasing the last one returns
    /// [`LayoutError::LastOutput`] and changes nothing. See [`World::remove_output`].
    pub fn release_output(&mut self, id: OutputId) -> Result<(), LayoutError> {
        self.world.remove_output(id)
    }
}

/// Converts a model [`Rect`] (global Space coordinates) into a Smithay logical rectangle.
#[must_use]
pub(crate) fn to_logical(rect: Rect) -> Rectangle<i32, Logical> {
    Rectangle::new(
        Point::from((rect.x, rect.y)),
        Size::from((rect.width.max(0), rect.height.max(0))),
    )
}

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

/// Layout geometry is edge to edge: tiles abut and cover the output. Per-class gaps are applied
/// to the *window* inside its tile ([`window_rect`]), never to the tile.
pub const NO_GAPS: GapConfig = GapConfig::new(0, 0);

/// Whether a managed window is a CDE-spawned terminal or an app (everything else).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WindowClass {
    /// A CDE-spawned terminal: fills its tile and draws the gap as its own padding.
    Terminal,
    /// Any other managed window: inset inside its tile by the app gap.
    App,
}

/// `gap` clamped so that insetting `tile` by it on every side never yields a negative size.
#[must_use]
pub fn clamped_gap(tile: Rect, gap: u32) -> i32 {
    i32::try_from(gap)
        .unwrap_or(i32::MAX)
        .min(tile.width / 2)
        .min(tile.height / 2)
        .max(0)
}

/// The rect a window of `class` occupies inside `tile`: a terminal fills the tile, an app is
/// inset by `tiling.app.gap` on every side (clamped so the size is never negative). **Pure.**
#[must_use]
pub fn window_rect(tile: Rect, class: WindowClass, tiling: &TilingConfig) -> Rect {
    match class {
        WindowClass::Terminal => tile,
        WindowClass::App => tile.inset(clamped_gap(tile, tiling.app.gap)),
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
            return;
        }

        for change in &diff.changes {
            match change {
                LayoutChange::Place { id, rect, resized } => {
                    let Some(element) = self.layout.element(*id).cloned() else {
                        continue;
                    };
                    // The projection holds the tile; the window sits inside it per its class.
                    let class = self.window_class(*id);
                    let logical = to_logical(window_rect(*rect, class, &self.config.tiling));
                    // Configure first, then position via map_element so the element is mapped (or
                    // re-mapped, for a window returning from hidden) at the right place.
                    configure_placement(&element, logical, *resized);
                    self.space.map_element(element, logical.loc, false);
                }
                LayoutChange::Hide { id } => {
                    if let Some(element) = self.layout.element(*id).cloned() {
                        self.space.unmap_elem(&element);
                    }
                }
            }
        }

        if let FocusChange::To(target) = diff.focus {
            self.apply_focus(target);
        }

        self.projection = next;
    }

    /// Snapshots every mapped compositor output as `(model id, global rect)`.
    ///
    /// Outputs not yet bound to a model id, or with no geometry in the `Space`, are skipped — they
    /// contribute nothing to the projection.
    ///
    /// While the header strip is shown (`always`, or `toggle` while visible) each rect has its height removed from its top, so the
    /// projection, `pane_rect`, the empty-pane ring and X11 placement all agree.
    pub(crate) fn output_bindings(&self) -> Vec<(OutputId, Rect)> {
        let reserved = crate::header::reserved_height(&self.config.header, self.header.visible);
        self.raw_output_rects()
            .into_iter()
            .map(|(id, rect)| (id, crate::header::shrink_top(rect, reserved)))
            .collect()
    }

    /// Every bound output's full global rect, before any header reservation.
    pub(crate) fn raw_output_rects(&self) -> Vec<(OutputId, Rect)> {
        self.space
            .outputs()
            .filter_map(|output| {
                let id = model_output_id(output)?;
                let geo = self.space.output_geometry(output)?;
                Some((id, Rect::new(geo.loc.x, geo.loc.y, geo.size.w, geo.size.h)))
            })
            .collect()
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

    // ---- window lifecycle controller ---------------------------------------------------------

    /// Maps a **managed** window: model open, bind the element, apply the projection.
    ///
    /// The projection — not the caller — decides where and at what size the window lands, and
    /// whether it takes focus. `on_open` runs with the new id **before** the first
    /// [`Self::apply_layout`], so per-window state the placement reads (its class) is registered
    /// in time. Returns the model [`WindowId`] now bound to `element`.
    pub fn map_managed_window(
        &mut self,
        element: WindowElement,
        on_open: impl FnOnce(&mut Self, WindowId),
    ) -> WindowId {
        let id = self.layout.open_managed(element);
        on_open(self, id);
        self.apply_layout();
        id
    }

    /// Maps a **managed** window into `pane` at stack `index` (a restored window matched to its
    /// pending placement). Same commit order as [`Self::map_managed_window`]; on a model error
    /// nothing is bound or mapped and `on_open` is not called.
    pub fn map_managed_window_at(
        &mut self,
        element: WindowElement,
        pane: PaneId,
        index: usize,
        on_open: impl FnOnce(&mut Self, WindowId),
    ) -> Result<WindowId, LayoutError> {
        let id = self.layout.open_managed_at(element, pane, index)?;
        on_open(self, id);
        self.apply_layout();
        Ok(id)
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
    pub fn unmap_matching(&mut self, pred: impl Fn(&WindowElement) -> bool) -> Teardown {
        match self.layout.resolve(&pred) {
            Binding::Managed(id) => match self.layout.close_managed(id) {
                Ok(element) => {
                    if let Some(element) = element {
                        self.space.unmap_elem(&element);
                    }
                    self.persist.forget_window(id);
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

    // ---- output lifecycle --------------------------------------------------------------------

    /// Registers a Smithay `output` with the model: adopt-or-add, then bind + size it.
    ///
    /// The **first** compositor output adopts the model's pre-existing `World::new()` output (which
    /// is otherwise unbound); every subsequent output calls [`World::add_output`] to get its own
    /// independent workspace stack. Either way the chosen model [`OutputId`] is stashed in the
    /// Smithay output's `user_data()` and its stored rect is set to `rect` (the live pixel geometry
    /// in global Space coordinates). Returns the model [`OutputId`]. Idempotent: re-registering an
    /// already-bound output only refreshes its rect.
    pub fn register_output(&mut self, output: &Output, rect: Rect) -> OutputId {
        if let Some(existing) = model_output_id(output) {
            self.set_model_output_rect(existing, rect);
            return existing;
        }

        // The binding decision (adopt the initial model output vs. allocate) is the model half of
        // registration and lives on `LayoutState` so it is testable without a `Space`.
        let bound: HashSet<OutputId> = self.space.outputs().filter_map(model_output_id).collect();
        let fresh = self.layout.world.output_ids().all(|id| bound.contains(&id));
        let model_id = self.layout.bind_output(&bound, rect);
        set_model_output_id(output, model_id);
        self.apply_layout();
        if fresh {
            // A freshly allocated output's seed workspace must not shadow a saved one.
            if let Some(seed) = self
                .layout
                .world
                .output(model_id)
                .map(crucible_layout::Output::active_workspace)
            {
                self.rename_seed_off_saved(seed);
            }
        }
        model_id
    }

    /// Updates the stored model rect for `output` from its current Space geometry.
    ///
    /// Called after an output is (re)mapped/resized so the projection sees the live pixel size.
    /// No-op for an output not yet bound to a model id.
    pub fn sync_output_rect(&mut self, output: &Output) {
        let Some(model_id) = model_output_id(output) else {
            return;
        };
        let Some(geo) = self.space.output_geometry(output) else {
            return;
        };
        let rect = Rect::new(geo.loc.x, geo.loc.y, geo.size.w, geo.size.h);
        self.set_model_output_rect(model_id, rect);
    }

    /// Writes `rect` onto model output `id`, reporting (and skipping) a stale binding.
    ///
    /// A Smithay output can outlive its model output (the connector is gone but a frame is still in
    /// flight). That is recoverable: the geometry update is dropped and the surviving outputs keep
    /// their own rects.
    fn set_model_output_rect(&mut self, id: OutputId, rect: Rect) {
        match self.with_model(|world| world.set_output_rect(id, rect)) {
            Ok(()) => {}
            Err(err) => warn!(%err, ?id, "stale output binding; dropping geometry update"),
        }
    }

    /// Removes the Smithay `output`'s model output, migrating its workspaces to a survivor.
    ///
    /// Calls [`World::remove_output`], which preserves every window by appending the removed
    /// output's workspaces onto a surviving output, then applies the projection. The **last** model
    /// output is never removed ([`LayoutError::LastOutput`]) — the model always keeps one — and an
    /// unbound output is nothing to remove.
    pub fn unregister_output(&mut self, output: &Output) {
        let Some(model_id) = model_output_id(output) else {
            debug!("unregister_output for an unbound output; ignoring");
            return;
        };
        match self.with_model(|world| world.remove_output(model_id)) {
            Ok(()) => {}
            Err(LayoutError::LastOutput) => {
                debug!(
                    ?model_id,
                    "keeping the last model output (last-output policy)"
                );
            }
            Err(err) => warn!(%err, ?model_id, "remove_output failed; keeping model output"),
        }
    }

    /// Routes the focused model output to the one backing `output`, if bound.
    ///
    /// Call when input/pointer focus moves onto a monitor (or a window maps there) so new windows
    /// and workspace verbs act on that monitor. No-op if unbound or already focused.
    pub fn focus_output(&mut self, output: &Output) {
        let Some(model_id) = model_output_id(output) else {
            return;
        };
        if self.layout.world.focused_output() == model_id {
            return;
        }
        match self.with_model(|world| world.set_focused_output(model_id)) {
            Ok(()) => info!(
                output = %output.name(),
                focus = %self.layout.world.focus_path(),
                "output focus"
            ),
            Err(err) => warn!(%err, ?model_id, "stale output binding; focus stays where it is"),
        }
    }
}

/// Sends the `configure` a projected placement needs to whichever surface kind backs `element`.
///
/// Wayland toplevels only learn their size (position is purely compositor-side), so they are
/// configured only when `resized`. X11 windows also carry a position in the X server, which
/// XWayland uses for input and for placing the client's own override-redirect menus, so they are
/// configured with the full `rect` whenever it differs from their X geometry — a move alone counts.
/// Kept free of `self` so it does not borrow the whole state.
fn configure_placement(element: &WindowElement, rect: Rectangle<i32, Logical>, resized: bool) {
    if let Some(toplevel) = element.toplevel() {
        // Every managed toplevel is tiled on all four edges so the client fills its tile instead
        // of drawing floating-window chrome (shadow margins, own sizing). Never `Maximized`.
        let had_tiled = toplevel.with_pending_state(|state| {
            let had = has_tiled_states(&state.states);
            for tiled in TILED_STATES {
                state.states.set(tiled);
            }
            if resized {
                state.size = Some(rect.size);
            }
            had
        });
        if toplevel_needs_configure(resized, had_tiled) {
            toplevel.send_configure();
        }
    } else if let Some(surface) = element.x11_surface()
        && x11_needs_configure(surface.geometry(), rect)
    {
        let _ = surface.configure(Some(rect));
    }
}

/// The xdg states every managed toplevel carries: tiled on all four edges.
const TILED_STATES: [XdgState; 4] = [
    XdgState::TiledLeft,
    XdgState::TiledRight,
    XdgState::TiledTop,
    XdgState::TiledBottom,
];

/// Whether `states` already carries all of [`TILED_STATES`]. **Pure** (no surface needed).
fn has_tiled_states(states: &smithay::wayland::shell::xdg::ToplevelStateSet) -> bool {
    TILED_STATES.iter().all(|s| states.contains(*s))
}

/// Whether a placement must (re)send a toplevel configure: a size change, or the tiled states
/// being newly applied (a states-only change at the same size still needs one). **Pure**.
fn toplevel_needs_configure(resized: bool, had_tiled: bool) -> bool {
    resized || !had_tiled
}

/// Whether an X11 window whose X geometry is `current` must be configured to reach `target`.
/// **Pure** so the rule is unit-testable without an X server.
fn x11_needs_configure(current: Rectangle<i32, Logical>, target: Rectangle<i32, Logical>) -> bool {
    current != target
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    #[test]
    fn x11_placement_configures_on_a_move_alone() {
        let at = |x, y, w, h| Rectangle::<i32, Logical>::new((x, y).into(), (w, h).into());
        // The regression: same size, new position (e.g. a pane on the second output) must still
        // reach the X server, or XWayland echoes the stale position back.
        assert!(x11_needs_configure(
            at(0, 0, 1920, 2160),
            at(3840, 0, 1920, 2160)
        ));
        assert!(x11_needs_configure(
            at(0, 0, 1920, 2160),
            at(0, 0, 3840, 2160)
        ));
        assert!(!x11_needs_configure(
            at(3840, 0, 1920, 2160),
            at(3840, 0, 1920, 2160)
        ));
    }

    #[test]
    fn terminals_fill_the_tile_and_apps_inset_by_their_gap_clamped() {
        let tile = Rect::new(10, 20, 400, 300);
        let mut tiling = TilingConfig::default();
        assert_eq!(window_rect(tile, WindowClass::Terminal, &tiling), tile);
        // Default app gap is 0: the app fills the tile too.
        assert_eq!(window_rect(tile, WindowClass::App, &tiling), tile);
        tiling.app.gap = 8;
        assert_eq!(
            window_rect(tile, WindowClass::App, &tiling),
            Rect::new(18, 28, 384, 284)
        );
        // Terminal gap never insets the window (it is the terminal's own padding).
        tiling.terminal.gap = 50;
        assert_eq!(window_rect(tile, WindowClass::Terminal, &tiling), tile);
        // An oversized gap clamps instead of going negative.
        tiling.app.gap = 1000;
        let w = window_rect(Rect::new(0, 0, 40, 10), WindowClass::App, &tiling);
        assert_eq!(w, Rect::new(5, 5, 30, 0));
    }

    #[test]
    fn toplevel_configure_resent_when_only_states_change() {
        assert!(toplevel_needs_configure(true, true), "resize");
        assert!(toplevel_needs_configure(false, false), "states-only change");
        assert!(
            !toplevel_needs_configure(false, true),
            "pure move: nothing new"
        );
    }

    #[test]
    fn tiled_states_cover_all_edges_and_never_maximize() {
        use smithay::wayland::shell::xdg::ToplevelStateSet;
        let mut set = ToplevelStateSet::default();
        assert!(!has_tiled_states(&set));
        for s in TILED_STATES {
            set.set(s);
        }
        assert!(has_tiled_states(&set));
        assert!(!set.contains(XdgState::Maximized));
        assert!(!TILED_STATES.contains(&XdgState::Maximized));
    }

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
    use std::rc::Rc;

    /// A stand-in payload: carries the same id-stamping contract as [`WindowElement`] with no
    /// Smithay surface behind it, so the lifecycle sequencing is testable without a compositor.
    #[derive(Debug, Clone, PartialEq)]
    struct TestElement {
        tag: u32,
        id: Rc<Cell<Option<WindowId>>>,
    }

    impl TestElement {
        fn new(tag: u32) -> Self {
            Self {
                tag,
                id: Rc::new(Cell::new(None)),
            }
        }
    }

    impl Payload for TestElement {
        fn set_window_id(&self, id: WindowId) {
            if self.id.get().is_none() {
                self.id.set(Some(id));
            }
        }

        fn window_id(&self) -> Option<WindowId> {
            self.id.get()
        }
    }

    fn tagged(tag: u32) -> impl Fn(&TestElement) -> bool {
        move |e: &TestElement| e.tag == tag
    }

    #[test]
    fn open_managed_at_places_by_index_and_binds_nothing_on_error() {
        let mut layout = state();
        let pane = layout.world.focused_pane_id();
        let b = layout
            .open_managed_at(TestElement::new(2), pane, 0)
            .unwrap();
        let a = layout
            .open_managed_at(TestElement::new(1), pane, 0)
            .unwrap();
        assert_eq!(layout.world.pane(pane).unwrap().windows, vec![a, b]);
        assert_eq!(layout.element(a).unwrap().window_id(), Some(a));
        let bogus = PaneId::from_raw(999);
        assert!(
            layout
                .open_managed_at(TestElement::new(3), bogus, 0)
                .is_err()
        );
        assert_eq!(layout.managed_count(), 2);
    }

    fn state() -> LayoutState<TestElement> {
        LayoutState::new()
    }

    #[test]
    fn opening_a_managed_window_binds_the_payload_both_ways() {
        let mut layout = state();
        let element = TestElement::new(1);
        let id = layout.open_managed(element.clone());

        assert_eq!(element.window_id(), Some(id));
        assert_eq!(layout.element(id), Some(&element));
        assert_eq!(layout.managed_count(), 1);
        assert_eq!(layout.world.window_count(), 1);
        assert_eq!(layout.resolve(tagged(1)), Binding::Managed(id));
    }

    #[test]
    fn closing_a_managed_window_releases_model_and_binding_together() {
        let mut layout = state();
        let id = layout.open_managed(TestElement::new(1));

        let released = layout.close_managed(id).expect("close succeeds");
        assert_eq!(released.map(|e| e.tag), Some(1));
        assert_eq!(layout.managed_count(), 0);
        assert_eq!(layout.world.window_count(), 0);
        // The window is gone from both halves, so a repeat resolves to nothing at all.
        assert_eq!(layout.resolve(tagged(1)), Binding::None);
    }

    #[test]
    fn release_binding_drops_only_the_binding_of_a_forgotten_window() {
        let mut layout = state();
        let id = layout.open_managed(TestElement::new(1));
        let keep = layout.open_managed(TestElement::new(2));
        layout.world.close_window(id).unwrap();

        assert_eq!(layout.release_binding(id).map(|e| e.tag), Some(1));
        assert_eq!(layout.managed_count(), 1);
        assert_eq!(layout.resolve(tagged(1)), Binding::None);
        assert_eq!(layout.resolve(tagged(2)), Binding::Managed(keep));
        // Idempotent.
        assert!(layout.release_binding(id).is_none());
    }

    #[test]
    fn repeated_close_is_an_error_the_resolver_never_reaches() {
        let mut layout = state();
        let id = layout.open_managed(TestElement::new(1));
        layout.close_managed(id).expect("first close succeeds");

        // Directly re-closing reports the model's rejection...
        assert_eq!(
            layout.close_managed(id),
            Err(LayoutError::UnknownWindow(id))
        );
        // ...but the controller resolves first, so a repeated unmap is simply a no-op.
        assert_eq!(layout.resolve(tagged(1)), Binding::None);
    }

    #[test]
    fn a_failed_model_close_leaves_every_binding_untouched() {
        let mut layout = state();
        let kept = layout.open_managed(TestElement::new(1));
        let other = layout.open_managed(TestElement::new(2));
        let unknown = WindowId::from_raw(9_999);

        let before_world = layout.world.clone();
        assert_eq!(
            layout.close_managed(unknown),
            Err(LayoutError::UnknownWindow(unknown))
        );

        // Model and bindings are byte-identical to before the failed mutation, so the caller never
        // reaches the `Space` half of the commit order.
        assert_eq!(layout.world, before_world);
        assert_eq!(layout.managed_count(), 2);
        assert!(layout.element(kept).is_some());
        assert!(layout.element(other).is_some());
    }

    #[test]
    fn unmanaged_elements_are_tracked_outside_the_world() {
        let mut layout = state();
        let element = TestElement::new(7);

        assert!(layout.track_unmanaged(element.clone()));
        assert_eq!(layout.resolve(tagged(7)), Binding::Unmanaged);
        // Outside the model entirely: no window, no binding.
        assert_eq!(layout.world.window_count(), 0);
        assert_eq!(layout.managed_count(), 0);
        assert_eq!(layout.unmanaged().len(), 1);

        // Re-mapping the same element does not duplicate the tracking entry.
        assert!(!layout.track_unmanaged(element));
        assert_eq!(layout.unmanaged().len(), 1);
    }

    #[test]
    fn unmanaged_unmap_then_destroy_is_symmetric_and_idempotent() {
        let mut layout = state();
        layout.track_unmanaged(TestElement::new(7));

        // First unmap tears it down...
        assert_eq!(layout.untrack_unmanaged(tagged(7)).map(|e| e.tag), Some(7));
        assert!(layout.unmanaged().is_empty());
        // ...and the following destroy (or any repeat) finds nothing and is harmless.
        assert_eq!(layout.resolve(tagged(7)), Binding::None);
        assert!(layout.untrack_unmanaged(tagged(7)).is_none());
    }

    #[test]
    fn managed_and_unmanaged_sets_stay_distinct() {
        let mut layout = state();
        let managed = layout.open_managed(TestElement::new(1));
        layout.track_unmanaged(TestElement::new(2));

        assert_eq!(layout.resolve(tagged(1)), Binding::Managed(managed));
        assert_eq!(layout.resolve(tagged(2)), Binding::Unmanaged);
        assert_eq!(layout.resolve(tagged(3)), Binding::None);
        // Tearing down the unmanaged one leaves the managed window alone.
        assert!(layout.untrack_unmanaged(tagged(2)).is_some());
        assert_eq!(layout.managed_count(), 1);
        assert_eq!(layout.world.window_count(), 1);
    }

    #[test]
    fn the_projection_reflects_managed_windows_only() {
        let mut layout = state();
        let output = layout.world.focused_output();
        layout
            .world
            .set_output_rect(output, Rect::new(0, 0, 800, 600))
            .expect("output exists");

        let managed = layout.open_managed(TestElement::new(1));
        layout.track_unmanaged(TestElement::new(2));

        let projection = layout
            .world
            .project_stored(crucible_layout::GapConfig::default());
        assert_eq!(projection.windows().len(), 1);
        assert!(projection.is_visible(managed));
        assert_eq!(projection.focus(), Some(managed));

        // Re-deriving without touching the model yields an empty diff: applying it is a no-op.
        assert!(
            projection
                .diff(
                    &layout
                        .world
                        .project_stored(crucible_layout::GapConfig::default())
                )
                .is_empty()
        );
    }

    // ---- output binding ----------------------------------------------------------------------

    const R1: Rect = Rect::new(0, 0, 1920, 1080);
    const R2: Rect = Rect::new(1920, 0, 1280, 1024);

    #[test]
    fn the_first_compositor_output_adopts_the_initial_model_output() {
        let mut layout = state();
        let initial = layout.world.focused_output();

        let id = layout.bind_output(&HashSet::new(), R1);

        assert_eq!(id, initial);
        assert_eq!(layout.world.output_ids().count(), 1);
        assert_eq!(layout.world.output(id).unwrap().rect, R1);
    }

    #[test]
    fn a_second_compositor_output_allocates_a_fresh_model_output() {
        let mut layout = state();
        let first = layout.bind_output(&HashSet::new(), R1);

        let second = layout.bind_output(&HashSet::from([first]), R2);

        assert_ne!(second, first);
        assert_eq!(layout.world.output_ids().count(), 2);
        assert_eq!(layout.world.output(first).unwrap().rect, R1);
        assert_eq!(layout.world.output(second).unwrap().rect, R2);
    }

    #[test]
    fn binding_never_steals_an_output_another_compositor_output_holds() {
        let mut layout = state();
        let first = layout.bind_output(&HashSet::new(), R1);
        let second = layout.bind_output(&HashSet::from([first]), R2);

        let third = layout.bind_output(&HashSet::from([first, second]), R1);

        assert!(third != first && third != second);
        assert_eq!(layout.world.output_ids().count(), 3);
    }

    #[test]
    fn a_stale_rect_update_is_reported_and_leaves_survivors_untouched() {
        let mut layout = state();
        let live = layout.bind_output(&HashSet::new(), R1);
        let before = layout.world.clone();
        let stale = OutputId::from_raw(u64::MAX);

        let outcome = layout.update_output_rect(stale, R2);

        assert_eq!(outcome, Err(LayoutError::UnknownOutput(stale)));
        assert_eq!(layout.world, before);
        assert_eq!(layout.world.output(live).unwrap().rect, R1);
    }

    #[test]
    fn releasing_the_last_output_is_refused_and_changes_nothing() {
        let mut layout = state();
        let only = layout.bind_output(&HashSet::new(), R1);
        layout.open_managed(TestElement::new(1));
        let before = layout.world.clone();

        assert_eq!(layout.release_output(only), Err(LayoutError::LastOutput));
        assert_eq!(layout.world, before);
        assert_eq!(layout.managed_count(), 1);
    }

    #[test]
    fn releasing_a_bound_output_keeps_its_windows_on_a_survivor() {
        let mut layout = state();
        let first = layout.bind_output(&HashSet::new(), R1);
        let second = layout.bind_output(&HashSet::from([first]), R2);
        layout.world.set_focused_output(second).unwrap();
        let id = layout.open_managed(TestElement::new(7));

        layout.release_output(second).unwrap();

        assert_eq!(layout.world.output_ids().collect::<Vec<_>>(), vec![first]);
        assert_eq!(layout.world.focused_output(), first);
        assert!(layout.world.window(id).is_some(), "migrated, not closed");
        assert_eq!(layout.managed_count(), 1);
    }
}
