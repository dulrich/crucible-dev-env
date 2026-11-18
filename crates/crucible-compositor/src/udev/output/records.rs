//! Output records and the pure helpers over them: ids, per-CRTC surface data, power marks, pending start-up adoptions and the mapped-output lookup.

use super::*;

/// Identifies the [`Output`] driven by a particular `(device, crtc)` pair.
///
/// Stored in each output's `user_data` so VBlank/render callbacks can find the Smithay [`Output`]
/// that corresponds to the CRTC the event fired for.
///
/// `Copy` so it can double as the planner's opaque output key (see [`super::plan`]).
#[derive(Clone, Copy, Debug, PartialEq)]
pub(in crate::udev) struct UdevOutputId {
    /// The DRM device (GPU) node the output lives on.
    pub device_id: DrmNode,
    /// The CRTC scanning out this output.
    pub crtc: crtc::Handle,
}

/// The dmabuf feedback a surface on one output is sent, depending on whether it was scanned out.
#[derive(Debug, Clone)]
pub(in crate::udev) struct SurfaceDmabufFeedback {
    /// Sent while the surface is composited: render-node tranche only.
    pub render_feedback: DmabufFeedback,
    /// Sent while the surface is (or nearly) scanned out: leads with the plane-format tranche.
    pub scanout_feedback: DmabufFeedback,
}

/// The formats of the scanout tranche: the plane formats that can also be rendered.
///
/// Limiting the tranche to renderable formats guarantees a composition fallback when a client
/// buffer cannot be scanned out directly.
fn scanout_tranche_formats(plane_formats: &FormatSet, render_formats: &FormatSet) -> FormatSet {
    plane_formats
        .intersection(render_formats)
        .copied()
        .collect()
}

/// Computes the render and scanout dmabuf feedback for a surface on `scanout_node`.
///
/// Mirrors anvil's `get_surface_dmabuf_feedback`; `None` when a renderer is unavailable or a
/// feedback cannot be built (the surface then keeps the global's default feedback).
pub(super) fn get_surface_dmabuf_feedback(
    primary_gpu: DrmNode,
    render_node: Option<DrmNode>,
    scanout_node: DrmNode,
    gpus: &mut GpuManager<GbmGlesBackend<GlesRenderer, DrmDeviceFd>>,
    surface: &DrmSurface,
) -> Option<SurfaceDmabufFeedback> {
    let primary_formats = gpus.single_renderer(&primary_gpu).ok()?.dmabuf_formats();
    let render_formats = match render_node {
        Some(render_node) => gpus.single_renderer(&render_node).ok()?.dmabuf_formats(),
        None => FormatSet::default(),
    };
    let all_render_formats: FormatSet = primary_formats
        .iter()
        .chain(render_formats.iter())
        .copied()
        .collect();

    let planes = surface.planes().clone();
    let plane_formats: FormatSet = surface
        .plane_info()
        .formats
        .iter()
        .copied()
        .chain(planes.overlay.into_iter().flat_map(|plane| plane.formats))
        .collect();
    let scanout_formats = scanout_tranche_formats(&plane_formats, &all_render_formats);

    let builder = DmabufFeedbackBuilder::new(primary_gpu.dev_id(), primary_formats);
    let render_feedback = match render_node {
        Some(render_node) => builder
            .clone()
            .add_preference_tranche(render_node.dev_id(), None, render_formats.clone())
            .build()
            .ok()?,
        None => builder.clone().build().ok()?,
    };
    let scanout_feedback = builder
        .add_preference_tranche(
            surface.device_fd().dev_id().ok()?,
            Some(TrancheFlags::Scanout),
            scanout_formats,
        )
        .add_preference_tranche(scanout_node.dev_id(), None, render_formats)
        .build()
        .ok()?;

    Some(SurfaceDmabufFeedback {
        render_feedback,
        scanout_feedback,
    })
}

/// One mapped output (connector/CRTC) on a DRM device.
///
/// Holds the Smithay [`Output`], its `wl_output` global, and the [`DrmOutput`] that owns the
/// per-output swapchain and page-flip scheduling.
pub(in crate::udev) struct SurfaceData {
    /// The output's `wl_output` global and leave-all cleanup (handed over from the
    /// [`PendingAdoption`]; declared first so it runs before the swapchain is dropped).
    pub(super) _global: OutputGlobal,
    /// The Smithay output mapped into the shared [`Space`].
    pub(super) output: Output,
    /// The DRM compositor for this CRTC: owns the GBM swapchain and frame submission.
    pub drm_output: DrmOutput<
        GbmAllocator<DrmDeviceFd>,
        GbmFramebufferExporter<DrmDeviceFd>,
        Option<OutputPresentationFeedback>,
        DrmDeviceFd,
    >,
    /// Per-output dmabuf feedback sent after each frame; `None` keeps the default feedback.
    pub dmabuf_feedback: Option<SurfaceDmabufFeedback>,
    /// Idle power state (DPMS off via `DrmCompositor::clear`).
    pub power: PowerMark,
    /// Once-flags for the `startup:` timing lines (first frame queued, first flip completed).
    pub first_frame: crate::startup::FirstOnly,
    pub first_flip: crate::startup::FirstOnly,
}

/// What [`CrucibleState::frame_finish`] does with a page-flip completion, given the power mark.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::udev) enum FlipAction {
    /// Normal: deliver feedback and schedule the next repaint.
    Schedule,
    /// Deliver nothing more and schedule nothing (a stale flip from before a
    /// power-off arriving after the wake render already runs its own chain).
    Drop,
}

/// Per-output idle power bookkeeping, kept Smithay-free so it is unit-testable.
///
/// `off` is set only when `clear()` succeeded. `stale_flip` covers the page-flip that may have
/// been pending at `clear()` time: its completion can still arrive after a wake, and must not
/// start a second render chain beside the wake render's.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(in crate::udev) struct PowerMark {
    off: bool,
    stale_flip: bool,
}

impl PowerMark {
    /// Whether the output is powered off (render nothing).
    pub(in crate::udev) fn is_off(self) -> bool {
        self.off
    }

    /// Records a successful `clear()` (DPMS off).
    pub(in crate::udev) fn powered_off(&mut self) {
        self.off = true;
        self.stale_flip = true;
    }

    /// Wakes the output; returns whether it was off (so the caller schedules exactly one render).
    pub(in crate::udev) fn wake(&mut self) -> bool {
        std::mem::take(&mut self.off)
    }

    /// Decides a page-flip completion of an output that is on. `had_frame` is whether the
    /// compositor still had a pending frame for it (`frame_submitted` returned `Some`).
    pub(in crate::udev) fn on_flip(&mut self, had_frame: bool) -> FlipAction {
        // The handler returns before `frame_submitted` while off, so `on_flip` only sees live flips.
        debug_assert!(
            !self.off,
            "on_flip while off: the handler drops the flip first"
        );
        if std::mem::take(&mut self.stale_flip) && !had_frame {
            return FlipAction::Drop;
        }
        FlipAction::Schedule
    }
}

/// The cleanup a mapped output owes on teardown: every surface leaves it and its `wl_output`
/// global is removed (plan 44 invariant 6).
///
/// Owned first by the output's [`PendingAdoption`] and then, from its adoption on, by its
/// [`SurfaceData`], so an output torn down before adoption still loses its global.
pub(in crate::udev) struct OutputGlobal {
    /// Display handle, used to remove the global on drop.
    pub(super) dh: DisplayHandle,
    /// The output every surface leaves on drop.
    pub(super) output: Output,
    /// The `wl_output` global advertised to clients.
    pub(super) global: Option<GlobalId>,
}

impl Drop for OutputGlobal {
    fn drop(&mut self) {
        self.output.leave_all();
        if let Some(global) = self.global.take() {
            self.dh.remove_global::<CrucibleState<UdevData>>(global);
        }
    }
}

/// A start-up output whose DRM adoption (its first commit) waits for its wallpaper (plan 44).
///
/// The output is already mapped, registered and advertised; this record holds what the deferred
/// `initialize_output` needs and owns the output's [`OutputGlobal`] until adoption.
pub(in crate::udev) struct PendingAdoption {
    pub(super) global: OutputGlobal,
    pub(super) output: Output,
    pub(super) connector: connector::Handle,
    pub(super) mode: drm_control::Mode,
    pub(super) planes: Planes,
}

/// The start-up adoptions due now (plan 44), taken from `pending`: the entry a wallpaper buffer
/// `landed` on (nothing if it is not pending, e.g. adopted already), or every one at the window's
/// close (`None`).
pub(super) fn due_adoptions<K: Eq + std::hash::Hash + Copy, P>(
    pending: &mut HashMap<K, P>,
    landed: Option<K>,
) -> Vec<(K, P)> {
    match landed {
        Some(key) => pending.remove(&key).map(|p| (key, p)).into_iter().collect(),
        None => pending.drain().collect(),
    }
}

/// One mapped output's backend record: adopted (it has a swapchain) or still pending adoption.
#[derive(Debug, PartialEq, Eq)]
pub(in crate::udev) enum Mapped<S, P> {
    Adopted(S),
    Pending(P),
}

/// Takes `key`'s record out of whichever map holds it (an output is in at most one of them).
pub(in crate::udev) fn take_mapped<K: Eq + std::hash::Hash, S, P>(
    surfaces: &mut HashMap<K, S>,
    pending: &mut HashMap<K, P>,
    key: &K,
) -> Option<Mapped<S, P>> {
    surfaces
        .remove(key)
        .map(Mapped::Adopted)
        .or_else(|| pending.remove(key).map(Mapped::Pending))
}

#[cfg(test)]
mod tests {
    use super::*;
    use smithay::backend::allocator::{Format, Fourcc, Modifier};

    #[test]
    fn a_wallpaper_adopts_only_its_own_pending_output() {
        let mut pending: HashMap<u8, &str> = HashMap::from([(1, "p1"), (2, "p2")]);
        assert_eq!(due_adoptions(&mut pending, Some(2)), vec![(2, "p2")]);
        assert_eq!(pending.len(), 1, "output 1 keeps waiting");
    }

    #[test]
    fn a_wallpaper_on_an_output_not_pending_adopts_nothing() {
        // Output 3 is adopted already (or not ours): pending 1 keeps waiting.
        let mut pending: HashMap<u8, &str> = HashMap::from([(1, "p1")]);
        assert!(due_adoptions(&mut pending, Some(3)).is_empty());
        assert_eq!(pending.len(), 1);
    }

    #[test]
    fn the_window_close_adopts_every_pending_output() {
        let mut pending: HashMap<u8, &str> = HashMap::from([(1, "p1"), (2, "p2")]);
        let mut due = due_adoptions(&mut pending, None);
        due.sort_unstable();
        assert_eq!(due, vec![(1, "p1"), (2, "p2")]);
        assert!(pending.is_empty());
        assert!(due_adoptions(&mut pending, None).is_empty());
    }

    #[test]
    fn disconnect_before_adoption_takes_the_pending_record() {
        let mut surfaces: HashMap<u8, &str> = HashMap::from([(1, "adopted")]);
        let mut pending: HashMap<u8, &str> = HashMap::from([(2, "pending")]);
        assert_eq!(
            take_mapped(&mut surfaces, &mut pending, &2),
            Some(Mapped::Pending("pending"))
        );
        assert!(pending.is_empty());
        assert_eq!(surfaces.len(), 1, "the adopted output is untouched");
        // A second disconnect event for the same CRTC finds nothing (no double cleanup).
        assert_eq!(take_mapped(&mut surfaces, &mut pending, &2), None);
    }

    #[test]
    fn device_removal_before_adoption_takes_every_record() {
        let mut surfaces: HashMap<u8, &str> = HashMap::from([(1, "adopted")]);
        let mut pending: HashMap<u8, &str> = HashMap::from([(2, "p2"), (3, "p3")]);
        let taken: Vec<_> = [1u8, 2, 3]
            .iter()
            .filter_map(|k| take_mapped(&mut surfaces, &mut pending, k))
            .collect();
        assert_eq!(
            taken,
            vec![
                Mapped::Adopted("adopted"),
                Mapped::Pending("p2"),
                Mapped::Pending("p3")
            ]
        );
        assert!(surfaces.is_empty() && pending.is_empty());
    }

    #[test]
    fn power_mark_off_drops_flips_and_wakes_once() {
        let mut mark = PowerMark::default();
        assert!(!mark.is_off());
        assert_eq!(mark.on_flip(true), FlipAction::Schedule);
        mark.powered_off();
        // A flip pending at clear() time completing while off: the handler drops it on `is_off`
        // before `frame_submitted`, so `on_flip` is never consulted.
        assert!(mark.is_off());
        assert!(mark.wake());
        assert!(!mark.is_off());
        // A second wake schedules nothing more (no double render).
        assert!(!mark.wake());
    }

    #[test]
    fn power_mark_stale_flip_after_wake_is_dropped_once() {
        let mut mark = PowerMark::default();
        mark.powered_off();
        assert!(mark.wake());
        // The stale flip arrives after the wake with no frame of its own: dropped.
        assert_eq!(mark.on_flip(false), FlipAction::Drop);
        // The wake frame's own flip then schedules normally.
        assert_eq!(mark.on_flip(true), FlipAction::Schedule);
        // Later frameless flips keep the pre-blanking behaviour (schedule).
        assert_eq!(mark.on_flip(false), FlipAction::Schedule);
    }

    #[test]
    fn power_mark_real_flip_after_wake_clears_the_stale_guard() {
        let mut mark = PowerMark::default();
        mark.powered_off();
        assert!(mark.wake());
        // No stale flip ever arrived; the wake frame's flip schedules and clears the guard.
        assert_eq!(mark.on_flip(true), FlipAction::Schedule);
        assert_eq!(mark.on_flip(false), FlipAction::Schedule);
    }

    fn fmt(code: Fourcc) -> Format {
        Format {
            code,
            modifier: Modifier::Linear,
        }
    }

    #[test]
    fn scanout_tranche_keeps_only_renderable_plane_formats() {
        let planes: FormatSet = [fmt(Fourcc::Argb8888), fmt(Fourcc::Nv12)]
            .into_iter()
            .collect();
        let render: FormatSet = [fmt(Fourcc::Argb8888), fmt(Fourcc::Abgr8888)]
            .into_iter()
            .collect();
        let tranche = scanout_tranche_formats(&planes, &render);
        assert_eq!(
            tranche.iter().copied().collect::<Vec<_>>(),
            vec![fmt(Fourcc::Argb8888)]
        );
    }

    #[test]
    fn scanout_tranche_is_empty_when_nothing_overlaps() {
        let planes: FormatSet = [fmt(Fourcc::Nv12)].into_iter().collect();
        let render: FormatSet = [fmt(Fourcc::Argb8888)].into_iter().collect();
        assert_eq!(scanout_tranche_formats(&planes, &render).iter().count(), 0);
    }
}
