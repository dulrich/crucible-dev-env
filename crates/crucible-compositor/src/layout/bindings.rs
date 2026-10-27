//! The model plus compositor payload bindings: [`LayoutState`], [`WindowMeta`], [`Payload`],
//! [`Binding`], [`Teardown`].

use std::collections::{HashMap, HashSet};

use crucible_layout::{LayoutError, OutputId, PaneId, Rect, WindowId, WorkspaceId, World};

use super::WindowClass;
use crate::element::WindowElement;

/// Per-window compositor metadata, bound to a managed window for exactly as long as its payload.
///
/// Supplied when the window is opened, so it is known before the first placement (the class
/// decides the window's first rect), and returned when the binding is released.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WindowMeta {
    /// The client PID, when known.
    pub pid: Option<u32>,
    /// The placement class: a CDE-spawned terminal or an app.
    pub class: WindowClass,
}

/// One managed binding: the payload plus its metadata.
#[derive(Debug)]
struct Bound<E> {
    element: E,
    meta: WindowMeta,
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
    /// Managed windows: model id → payload and metadata (visible or hidden).
    by_id: HashMap<WindowId, Bound<E>>,
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
        self.by_id.get(&id).map(|b| &b.element)
    }

    /// The metadata bound to model window `id`.
    #[must_use]
    pub fn meta(&self, id: WindowId) -> Option<&WindowMeta> {
        self.by_id.get(&id).map(|b| &b.meta)
    }

    /// Every bound managed window's metadata, in no particular order.
    pub fn metas(&self) -> impl Iterator<Item = (WindowId, &WindowMeta)> {
        self.by_id.iter().map(|(id, b)| (*id, &b.meta))
    }

    /// The placement class of window `id` — the single class lookup. An unbound id is an app.
    #[must_use]
    pub fn window_class(&self, id: WindowId) -> WindowClass {
        self.meta(id).map_or(WindowClass::App, |m| m.class)
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
        if let Some((id, _)) = self.by_id.iter().find(|(_, b)| pred(&b.element)) {
            return Binding::Managed(*id);
        }
        if self.unmanaged.iter().any(&pred) {
            return Binding::Unmanaged;
        }
        Binding::None
    }

    /// Step 1+2 of the commit order for a **map**: open a window in the model, then bind `element`
    /// with its `meta`.
    ///
    /// `World::open_window` is infallible, so this cannot leave the pair inconsistent. Returns the
    /// new [`WindowId`]; the caller applies the projection.
    pub fn open_managed(&mut self, element: E, meta: WindowMeta) -> WindowId {
        let id = self.world.open_window();
        self.bind(id, element, meta);
        id
    }

    fn bind(&mut self, id: WindowId, element: E, meta: WindowMeta) {
        element.set_window_id(id);
        self.by_id.insert(id, Bound { element, meta });
    }

    /// Like [`Self::open_managed`] but opens the window at stack `index` of `pane` (restore
    /// placement). On a model error nothing is bound and the error is returned.
    pub fn open_managed_at(
        &mut self,
        element: E,
        meta: WindowMeta,
        pane: PaneId,
        index: usize,
    ) -> Result<WindowId, LayoutError> {
        let id = self.world.open_window_at(pane, index)?;
        self.bind(id, element, meta);
        Ok(id)
    }

    /// Step 1+2 of the commit order for an **unmap**: close `id` in the model, then release its
    /// binding.
    ///
    /// The model mutation happens **first and alone**: if it fails, `Err` is returned and neither
    /// the binding map nor (by extension) the `Space` has been touched. On success the released
    /// payload and its metadata are returned so the caller can unmap it from the `Space`;
    /// `Ok(None)` means the model knew the window but nothing was ever bound to it.
    pub fn close_managed(&mut self, id: WindowId) -> Result<Option<(E, WindowMeta)>, LayoutError> {
        self.world.close_window(id)?;
        Ok(self.release_binding(id))
    }

    /// Drops the binding of a window the model has **already** forgotten (e.g. after
    /// `World::remove_workspace`), returning its payload and metadata so the caller can unmap and
    /// end it. `None` when nothing was bound. After this, a later unmap/destroy of the payload
    /// resolves to [`Binding::None`] and is ignored.
    pub fn release_binding(&mut self, id: WindowId) -> Option<(E, WindowMeta)> {
        self.by_id.remove(&id).map(|b| (b.element, b.meta))
    }

    /// The model half of closing a workspace: removes `ws` from the model, then releases the
    /// binding of every window it held, returning each `(id, payload, meta)` in pane/stack order so
    /// the caller can unmap and end them. On a model error nothing changes and the error is
    /// returned.
    pub fn release_workspace(
        &mut self,
        ws: WorkspaceId,
    ) -> Result<Vec<(WindowId, E, WindowMeta)>, LayoutError> {
        let ids = self.world.remove_workspace(ws)?;
        Ok(ids
            .into_iter()
            .filter_map(|id| self.release_binding(id).map(|(e, m)| (id, e, m)))
            .collect())
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
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
            .open_managed_at(TestElement::new(2), app(), pane, 0)
            .unwrap();
        let a = layout
            .open_managed_at(TestElement::new(1), app(), pane, 0)
            .unwrap();
        assert_eq!(layout.world.pane(pane).unwrap().windows(), vec![a, b]);
        assert_eq!(layout.element(a).unwrap().window_id(), Some(a));
        let bogus = PaneId::from_raw(999);
        assert!(
            layout
                .open_managed_at(TestElement::new(3), terminal(3), bogus, 0)
                .is_err()
        );
        assert_eq!(layout.managed_count(), 2);
        assert_eq!(layout.resolve(tagged(3)), Binding::None);
        assert!(layout.metas().all(|(_, m)| m.pid != Some(3)));
    }

    fn state() -> LayoutState<TestElement> {
        LayoutState::new()
    }

    fn app() -> WindowMeta {
        WindowMeta {
            pid: None,
            class: WindowClass::App,
        }
    }

    fn terminal(pid: u32) -> WindowMeta {
        WindowMeta {
            pid: Some(pid),
            class: WindowClass::Terminal,
        }
    }

    /// The class is bound at open, so the first placement (applied right after) already reads it;
    /// releasing the binding returns the meta and the class falls back to `App`.
    #[test]
    fn meta_is_bound_at_open_and_returned_at_release() {
        let mut layout = state();
        let pane = layout.world.focused_pane_id();
        let a = layout.open_managed(TestElement::new(1), terminal(10));
        let b = layout
            .open_managed_at(TestElement::new(2), terminal(20), pane, 0)
            .unwrap();
        assert_eq!(layout.window_class(a), WindowClass::Terminal);
        assert_eq!(layout.window_class(b), WindowClass::Terminal);
        assert_eq!(layout.meta(b), Some(&terminal(20)));

        let (_, meta) = layout.close_managed(a).unwrap().unwrap();
        assert_eq!(meta, terminal(10));
        assert_eq!(layout.window_class(a), WindowClass::App);
        assert!(layout.meta(a).is_none());

        layout.world.close_window(b).unwrap();
        let (_, meta) = layout.release_binding(b).unwrap();
        assert_eq!(meta, terminal(20));
        assert_eq!(layout.window_class(b), WindowClass::App);
    }

    #[test]
    fn release_workspace_drops_the_model_and_bindings_or_nothing() {
        let mut layout = state();
        let first = layout.world.active_workspace_id();
        let kept = layout.open_managed(TestElement::new(1), app());
        let ws = layout.world.new_workspace("two");
        let pane = layout.world.workspace(ws).unwrap().panes()[0];
        let a = layout
            .open_managed_at(TestElement::new(2), terminal(7), pane, 0)
            .unwrap();
        let b = layout
            .open_managed_at(TestElement::new(3), app(), pane, 1)
            .unwrap();
        let released = layout.release_workspace(ws).unwrap();
        let tags: Vec<(WindowId, u32, WindowMeta)> = released
            .into_iter()
            .map(|(id, e, m)| (id, e.tag, m))
            .collect();
        assert_eq!(tags.len(), 2);
        assert!(tags.contains(&(a, 2, terminal(7))));
        assert!(tags.contains(&(b, 3, app())));
        assert_eq!(layout.managed_count(), 1);
        assert!(layout.element(kept).is_some());
        assert_eq!(layout.resolve(tagged(2)), Binding::None);

        // The last workspace is refused by the model: nothing changes.
        let before = layout.world.clone();
        assert!(layout.release_workspace(first).is_err());
        assert_eq!(layout.world, before);
        assert_eq!(layout.managed_count(), 1);
    }

    #[test]
    fn opening_a_managed_window_binds_the_payload_both_ways() {
        let mut layout = state();
        let element = TestElement::new(1);
        let id = layout.open_managed(element.clone(), app());

        assert_eq!(element.window_id(), Some(id));
        assert_eq!(layout.element(id), Some(&element));
        assert_eq!(layout.managed_count(), 1);
        assert_eq!(layout.world.window_count(), 1);
        assert_eq!(layout.resolve(tagged(1)), Binding::Managed(id));
    }

    #[test]
    fn closing_a_managed_window_releases_model_and_binding_together() {
        let mut layout = state();
        let id = layout.open_managed(TestElement::new(1), app());

        let released = layout.close_managed(id).expect("close succeeds");
        assert_eq!(released.map(|(e, _)| e.tag), Some(1));
        assert_eq!(layout.managed_count(), 0);
        assert_eq!(layout.world.window_count(), 0);
        // The window is gone from both halves, so a repeat resolves to nothing at all.
        assert_eq!(layout.resolve(tagged(1)), Binding::None);
    }

    #[test]
    fn release_binding_drops_only_the_binding_of_a_forgotten_window() {
        let mut layout = state();
        let id = layout.open_managed(TestElement::new(1), app());
        let keep = layout.open_managed(TestElement::new(2), app());
        layout.world.close_window(id).unwrap();

        assert_eq!(layout.release_binding(id).map(|(e, _)| e.tag), Some(1));
        assert_eq!(layout.managed_count(), 1);
        assert_eq!(layout.resolve(tagged(1)), Binding::None);
        assert_eq!(layout.resolve(tagged(2)), Binding::Managed(keep));
        // Idempotent.
        assert!(layout.release_binding(id).is_none());
    }

    #[test]
    fn repeated_close_is_an_error_the_resolver_never_reaches() {
        let mut layout = state();
        let id = layout.open_managed(TestElement::new(1), app());
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
        let kept = layout.open_managed(TestElement::new(1), app());
        let other = layout.open_managed(TestElement::new(2), app());
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
        let managed = layout.open_managed(TestElement::new(1), app());
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

        let managed = layout.open_managed(TestElement::new(1), app());
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
        layout.open_managed(TestElement::new(1), app());
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
        let id = layout.open_managed(TestElement::new(7), app());

        layout.release_output(second).unwrap();

        assert_eq!(layout.world.output_ids().collect::<Vec<_>>(), vec![first]);
        assert_eq!(layout.world.focused_output(), first);
        assert!(layout.world.window(id).is_some(), "migrated, not closed");
        assert_eq!(layout.managed_count(), 1);
    }
}
