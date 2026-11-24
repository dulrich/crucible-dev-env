//! Screen lock: CDE's `ext-session-lock-v1` policy over Smithay's protocol state.
//!
//! Smithay (`smithay::wayland::session_lock`) owns the protocol objects; CDE owns the policy. The
//! pure [`LockMachine`] is the single source of truth for whether the session is locked: rendering,
//! input and focus consult it, never Smithay's `locked_outputs`.
//!
//! ## States
//! - `Unlocked` — normal operation.
//! - `Locking { pending, progress }` — a locker was accepted (its [`SessionLocker`] rides in the
//!   phase); every output shows black (plus its lock surface; glass adds the trusted wallpaper, see
//!   below) and CDE waits until each lit output
//!   has *presented* such a frame before it sends `locked` (`SessionLocker::lock`). The wait is
//!   the presentation gate's lock arm ([`crate::present_gate::ArmKind::Lock`]), keyed to the
//!   accepted lock.
//! - `Locked { confirmed }` — `locked` was sent. If the confirmed lock object dies without a
//!   valid unlock (the locker crashed) the session stays locked and black, and a new locker may
//!   take the lock (it goes through `Locking` again).
//!
//! ## Glass exception (plan 52)
//! The locked frame is the lock surface over black and nothing else, with one exception: a lock
//! bound to glass ([`GlassEligibility`]) also draws that output's trusted wallpaper surfaces
//! (Background-layer surfaces of a `swaybg` client, [`WallpaperTrusted`]) beneath the lock
//! surface, once the lock surface has a renderable element ([`glass_stack`]); before that, and
//! for every non-glass or ineligible lock, the output is black as before. No window and no other
//! layer is ever drawn while locked, and trusted surfaces get frame callbacks only, never input.
//!
//! ## Unlock guard (plan 07, audit B3)
//! The pinned Smithay calls [`SessionLockHandler::unlock`] even for an *invalid*
//! `unlock_and_destroy` (on an unconfirmed lock), and the callback carries no lock identity. CDE
//! therefore records an unlock *candidate* only while `Locked` with the confirmed lock alive at
//! call time, and decides in a deferred (idle) check: a valid `unlock_and_destroy` destroys the
//! confirmed object at the end of its dispatch, so it is dead at the check; an invalid unlock from
//! another lock leaves it alive. Residual race: a locker crash in the same dispatch as a hostile
//! invalid unlock (or the locker's own client killed by the `InvalidUnlock` error) is
//! indistinguishable from a valid unlock.

use smithay::{
    backend::renderer::{
        ImportAll, ImportMem, Renderer,
        element::{Kind, surface::render_elements_from_surface_tree},
    },
    desktop::{LayerSurface, Space, layer_map_for_output, utils::send_frames_surface_tree},
    output::Output,
    reexports::{
        wayland_protocols::ext::session_lock::v1::server::ext_session_lock_v1::ExtSessionLockV1,
        wayland_server::{
            Client, Resource,
            protocol::{wl_output::WlOutput, wl_surface::WlSurface},
        },
    },
    utils::{Logical, Point, Scale},
    wayland::session_lock::{
        LockSurface, SessionLockHandler, SessionLockManagerState, SessionLocker,
    },
    wayland::{compositor::with_states, shell::wlr_layer::Layer},
};
use tracing::{info, warn};

use crate::{
    element::WindowElement,
    focus::{KeyboardFocusTarget, PointerFocusTarget},
    present_gate::ArmKind,
    render::CustomRenderElements,
    state::{Backend, CrucibleState},
};

/// The clear colour of every locked frame.
pub const LOCK_CLEAR_COLOR: [f32; 4] = [0.0, 0.0, 0.0, 1.0];

/// The lock phase (see the module docs).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LockPhase<L, P = ()> {
    /// Not locked.
    Unlocked,
    /// Blanked; waiting for every output to present a locked frame.
    Locking {
        /// The accepted, not yet confirmed lock.
        pending: L,
        /// What only exists while locking (the glue's [`SessionLocker`]); handed back on
        /// confirmation and dropped with the phase otherwise.
        progress: P,
    },
    /// `locked` was sent for `confirmed` (which may since have died).
    Locked {
        /// The confirmed lock.
        confirmed: L,
    },
}

/// The pure lock state machine, generic over an opaque lock handle `L` and the opaque payload `P`
/// carried only while `Locking`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LockMachine<L, P = ()> {
    phase: LockPhase<L, P>,
    unlock_candidate: bool,
}

impl<L, P> Default for LockMachine<L, P> {
    fn default() -> Self {
        Self {
            phase: LockPhase::Unlocked,
            unlock_candidate: false,
        }
    }
}

impl<L, P> LockMachine<L, P> {
    /// The current phase.
    #[must_use]
    pub fn phase(&self) -> &LockPhase<L, P> {
        &self.phase
    }

    /// Whether the session is locked or locking (only lock surfaces and black are shown, plus the
    /// trusted wallpaper under a glass lock's surface).
    #[must_use]
    pub fn is_locked(&self) -> bool {
        !matches!(self.phase, LockPhase::Unlocked)
    }

    /// The pending or confirmed lock, if any.
    #[must_use]
    pub fn current(&self) -> Option<&L> {
        match &self.phase {
            LockPhase::Unlocked => None,
            LockPhase::Locking { pending, .. } => Some(pending),
            LockPhase::Locked { confirmed } => Some(confirmed),
        }
    }

    /// A locker asks for the lock with its `Locking` payload. `current_alive` is whether the
    /// current pending/confirmed lock object is still alive. Returns whether the request is
    /// accepted (it then becomes pending); a refused request drops `handle` and `progress`, and an
    /// accepted one drops the payload of any lock it replaces.
    pub fn request_lock(&mut self, handle: L, progress: P, current_alive: bool) -> bool {
        if self.is_locked() && current_alive {
            return false;
        }
        self.phase = LockPhase::Locking {
            pending: handle,
            progress,
        };
        self.unlock_candidate = false;
        true
    }

    /// Every output presented a locked frame for the lock `handle`. Returns the `Locking` payload
    /// when this confirms that pending lock (the caller then sends `locked`), `None` otherwise (a
    /// replaced lock's report never confirms its successor).
    pub fn all_outputs_presented(&mut self, handle: &L) -> Option<P>
    where
        L: PartialEq,
    {
        match std::mem::replace(&mut self.phase, LockPhase::Unlocked) {
            LockPhase::Locking { pending, progress } if pending == *handle => {
                self.phase = LockPhase::Locked { confirmed: pending };
                Some(progress)
            }
            other => {
                self.phase = other;
                None
            }
        }
    }

    /// Smithay called `unlock()`. Records a candidate only while `Locked` with the confirmed lock
    /// alive at call time; returns whether a deferred [`Self::unlock_check`] is needed.
    pub fn unlock_called(&mut self, confirmed_alive_now: bool) -> bool {
        if matches!(self.phase, LockPhase::Locked { .. }) && confirmed_alive_now {
            self.unlock_candidate = true;
            return true;
        }
        false
    }

    /// The deferred check: unlocks only when a candidate was recorded and the confirmed lock is
    /// now dead. Returns whether the session is now unlocked.
    pub fn unlock_check(&mut self, confirmed_alive_now: bool) -> bool {
        let candidate = std::mem::take(&mut self.unlock_candidate);
        if candidate && !confirmed_alive_now && matches!(self.phase, LockPhase::Locked { .. }) {
            self.phase = LockPhase::Unlocked;
            return true;
        }
        false
    }
}

/// The glass eligibility state machine, generic over the lock handle `H` (like [`LockMachine`]) so
/// it tests without Smithay objects.
///
/// A launch is **pending** for the PID of the CDE-spawned glass locker ([`Self::expect`]); the
/// entry is dropped when that spawn is reaped ([`Self::reaped`]) or replaced by a later launch,
/// and is consumed by the first accepted lock whose peer PID equals it ([`Self::accept`]), which
/// binds glass to that lock handle. Every accepted lock first forgets the previous binding, so a
/// replacement locker is glass only if it was itself an eligible spawn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GlassEligibility<H> {
    pending: Option<u32>,
    bound: Option<H>,
}

impl<H> Default for GlassEligibility<H> {
    fn default() -> Self {
        Self {
            pending: None,
            bound: None,
        }
    }
}

impl<H: PartialEq> GlassEligibility<H> {
    /// A locker launch finished spawning as `pid`: `Some(pid)` for an eligible glass launch,
    /// `None` for an ineligible one or a failed spawn. Replaces any earlier pending entry.
    pub fn launched(&mut self, pid: Option<u32>) {
        self.pending = pid;
    }

    /// The spawn registry reaped `pids`: a pending entry for one of them is dropped.
    pub fn reaped(&mut self, pids: &[u32]) {
        if self.pending.is_some_and(|p| pids.contains(&p)) {
            self.pending = None;
        }
    }

    /// A lock was accepted for `handle` from a client whose peer PID is `peer`: glass is bound to
    /// `handle` only when `peer` is the pending spawn (which is consumed); otherwise the lock is
    /// not glass. Returns whether it is glass.
    pub fn accept(&mut self, peer: Option<u32>, handle: H) -> bool {
        self.bound = None;
        if peer.is_some() && peer == self.pending {
            self.pending = None;
            self.bound = Some(handle);
            return true;
        }
        false
    }

    /// Whether glass is bound to `current`, the pending or confirmed lock handle.
    #[must_use]
    pub fn is_bound(&self, current: Option<&H>) -> bool {
        current.is_some() && self.bound.as_ref() == current
    }
}

/// The per-surface marker of a wallpaper-trusted layer surface; it lives in the surface's data
/// map and so dies with the surface.
#[derive(Debug)]
struct WallpaperTrusted;

/// Whether a new layer surface is wallpaper-trusted: it is on the `Background` layer and its
/// client's executable basename is exactly `swaybg`. The gate guards accidental exposure of
/// another background-layer app on the lock screen, not a hostile same-UID process. **Pure.**
#[must_use]
pub fn is_trusted_wallpaper(layer: Layer, exe: Option<&std::path::Path>) -> bool {
    layer == Layer::Background
        && exe
            .and_then(std::path::Path::file_name)
            .is_some_and(|n| n == "swaybg")
}

/// Tags `surface` wallpaper-trusted (see [`is_trusted_wallpaper`]).
pub fn tag_wallpaper_trusted(surface: &WlSurface) {
    with_states(surface, |states| {
        states
            .data_map
            .insert_if_missing_threadsafe(|| WallpaperTrusted);
    });
}

fn is_wallpaper_tagged(surface: &WlSurface) -> bool {
    with_states(surface, |states| {
        states.data_map.get::<WallpaperTrusted>().is_some()
    })
}

/// The trusted wallpaper surfaces of `output` (Background layer, tagged) with their output-local
/// logical positions.
fn wallpaper_layers(output: &Output) -> Vec<(LayerSurface, Point<i32, Logical>)> {
    let map = layer_map_for_output(output);
    map.layers_on(Layer::Background)
        .filter(|l| is_wallpaper_tagged(l.wl_surface()))
        .filter_map(|l| Some((l.clone(), map.layer_geometry(l)?.loc)))
        .collect()
}

/// The locked frame's element list (front-to-back) for one output: the lock surface's `lock`
/// elements, then — only for a glass-bound lock whose lock surface produced at least one
/// element — the trusted `wallpaper` elements beneath. With no lock element the result is empty
/// (black), glass or not. `wallpaper` is only evaluated when needed. **Pure.**
pub fn glass_stack<T>(
    glass_bound: bool,
    mut lock: Vec<T>,
    wallpaper: impl FnOnce() -> Vec<T>,
) -> Vec<T> {
    if glass_bound && !lock.is_empty() {
        lock.extend(wallpaper());
    }
    lock
}

/// CDE's session-lock glue state, held by [`CrucibleState`].
#[derive(Debug)]
pub struct SessionLock {
    /// Smithay's `ext_session_lock_manager_v1` global state.
    pub manager: SessionLockManagerState,
    /// The authoritative lock state; while `Locking` it carries the accepted [`SessionLocker`]
    /// (consumed to send `locked`).
    pub machine: LockMachine<ExtSessionLockV1, SessionLocker>,
    /// Which lock (if any) is bound to glass, and the pending glass spawn.
    pub glass: GlassEligibility<ExtSessionLockV1>,
    /// Lock surfaces, one per output.
    surfaces: Vec<(Output, LockSurface)>,
}

impl SessionLock {
    /// Wraps the manager global state; starts unlocked.
    #[must_use]
    pub fn new(manager: SessionLockManagerState) -> Self {
        Self {
            manager,
            machine: LockMachine::default(),
            glass: GlassEligibility::default(),
            surfaces: Vec::new(),
        }
    }

    /// Forgets every lock surface (on accepting a lock and on unlocking).
    fn reset_surfaces(&mut self) {
        self.surfaces.clear();
    }

    /// Whether the session is locked or locking.
    #[must_use]
    pub fn is_locked(&self) -> bool {
        self.machine.is_locked()
    }

    /// The live lock surface of `output`, if any.
    #[must_use]
    pub fn surface_for(&self, output: &Output) -> Option<&LockSurface> {
        self.surfaces
            .iter()
            .find(|(o, s)| o == output && s.alive())
            .map(|(_, s)| s)
    }

    /// Whether the current (pending or confirmed) lock is bound to glass.
    #[must_use]
    pub fn glass_bound(&self) -> bool {
        self.glass.is_bound(self.machine.current())
    }

    fn current_alive(&self) -> bool {
        self.machine.current().is_some_and(Resource::is_alive)
    }
}

/// The render elements of `output`'s lock surface (none if it has none yet), output-local.
pub fn lock_render_elements<R>(
    lock: &SessionLock,
    output: &Output,
    renderer: &mut R,
    scale: Scale<f64>,
) -> Vec<CustomRenderElements<R>>
where
    R: Renderer + ImportAll + ImportMem,
    R::TextureId: Clone + 'static,
{
    let Some(surface) = lock.surface_for(output) else {
        return Vec::new();
    };
    render_elements_from_surface_tree(
        renderer,
        surface.wl_surface(),
        (0, 0),
        scale,
        1.0,
        Kind::Unspecified,
    )
}

/// The locked frame's elements for `output`: [`lock_render_elements`], plus the trusted
/// wallpaper beneath them for a glass-bound lock whose lock surface has an element
/// ([`glass_stack`]).
pub fn locked_elements<R>(
    lock: &SessionLock,
    output: &Output,
    renderer: &mut R,
    scale: Scale<f64>,
) -> Vec<CustomRenderElements<R>>
where
    R: Renderer + ImportAll + ImportMem,
    R::TextureId: Clone + 'static,
{
    let lock_elements = lock_render_elements(lock, output, renderer, scale);
    glass_stack(lock.glass_bound(), lock_elements, || {
        wallpaper_layers(output)
            .into_iter()
            .flat_map(|(layer, loc)| {
                render_elements_from_surface_tree(
                    renderer,
                    layer.wl_surface(),
                    loc.to_physical_precise_round(scale),
                    scale,
                    1.0,
                    Kind::Unspecified,
                )
            })
            .collect()
    })
}

/// Sends frame callbacks to `output`'s lock surface (while locked), and for a glass-bound lock to
/// its trusted wallpaper surfaces so a wallpaper rotation during the lock lands.
pub fn send_lock_frames(
    lock: &SessionLock,
    output: &Output,
    time: impl Into<std::time::Duration>,
    throttle: Option<std::time::Duration>,
) {
    let time = time.into();
    if let Some(surface) = lock.surface_for(output) {
        send_frames_surface_tree(surface.wl_surface(), output, time, throttle, |_, _| {
            Some(output.clone())
        });
    }
    if lock.glass_bound() {
        for (layer, _) in wallpaper_layers(output) {
            send_frames_surface_tree(layer.wl_surface(), output, time, throttle, |_, _| {
                Some(output.clone())
            });
        }
    }
}

/// The lock surface under global `pos`, as a pointer focus (only meaningful while locked).
#[must_use]
pub fn lock_surface_under(
    lock: &SessionLock,
    space: &Space<WindowElement>,
    pos: Point<f64, Logical>,
) -> Option<(PointerFocusTarget, Point<f64, Logical>)> {
    let output = space.output_under(pos).next()?;
    let geo = space.output_geometry(output)?;
    let surface = lock.surface_for(output)?;
    Some((
        PointerFocusTarget::WlSurface(surface.wl_surface().clone()),
        geo.loc.to_f64(),
    ))
}

impl<B: Backend + 'static> CrucibleState<B> {
    /// Whether the session is locked or locking.
    #[must_use]
    pub fn is_session_locked(&self) -> bool {
        self.session_lock.is_locked()
    }

    /// Spawns the configured `lock-command` detached (the `lock` action). A failure is logged; the
    /// lock itself starts only on the locker's protocol request.
    pub(crate) fn spawn_locker(&mut self) {
        let cmd = self.config.lock_command.clone();
        let mut words = cmd.split_whitespace();
        let Some(program) = words.next() else {
            warn!("lock-command is empty; not spawning a locker");
            return;
        };
        let words: Vec<&str> = words.collect();
        // With a `lock` block: refresh the generated swaylock config (a `user` background follows
        // wallpaper rotations) and pass it to a plain `swaylock`.
        let fragment = crate::lock_config::write_swaylock_config(&self.config);
        let (args, inserted) = crate::lock_config::insert_fragment(
            program,
            &words,
            fragment.as_ref().map(|f| f.path.as_path()),
        );
        let eligible = self.config.lock.as_ref().is_some_and(|l| {
            crate::lock_config::glass_launch_eligible(l.background, fragment.as_ref(), inserted)
        });
        info!(cmd, "lock: spawning the locker");
        let pid = self.spawn(crate::persist::SpawnRequest::new(
            program,
            args,
            crate::persist::SpawnKind::Overlay,
        ));
        if pid.is_none() {
            warn!(cmd, "lock: the locker could not be spawned");
        }
        // A pending glass entry follows this spawn (and replaces any earlier one).
        self.session_lock.glass.launched(pid.filter(|_| eligible));
    }

    /// The spawn registry reaped `pids`: a glass locker that exited before taking the lock is no
    /// longer eligible.
    pub(crate) fn locker_reaped(&mut self, pids: &[u32]) {
        self.session_lock.glass.reaped(pids);
    }

    /// The peer PID of `client` (`SO_PEERCRED` at connect time).
    fn peer_pid(&self, client: &Client) -> Option<u32> {
        let creds = client.get_credentials(&self.display_handle).ok()?;
        u32::try_from(creds.pid).ok()
    }

    /// The lock arm released: every lit output presented a frame queued after `handle` was
    /// accepted (every such frame is a locked one), or no output is lit. Confirms `handle` if it is
    /// still the pending lock — the machine moves to `Locked`, then `locked` is sent.
    fn lock_confirm(&mut self, handle: &ExtSessionLockV1) {
        if let Some(locker) = self.session_lock.machine.all_outputs_presented(handle) {
            info!("lock: every output presented a locked frame; session locked");
            locker.lock();
        }
    }

    /// Re-sizes `output`'s lock surface after a mode change (no-op when unlocked).
    pub(crate) fn lock_output_resized(&mut self, output: &Output) {
        if !self.is_session_locked() {
            return;
        }
        if let Some(surface) = self.session_lock.surface_for(output) {
            let size = lock_size(&self.space, output);
            surface.with_pending_state(|s| s.size = size);
            surface.send_configure();
        }
    }

    fn lock_redraw_all(&mut self) {
        let outputs: Vec<Output> = self.space.outputs().cloned().collect();
        for output in &outputs {
            self.backend_data.reset_buffers(output);
        }
    }

    /// The deferred unlock check (B3 guard), run from an idle callback.
    fn lock_unlock_check(&mut self) {
        let alive = self.session_lock.current_alive();
        if !self.session_lock.machine.unlock_check(alive) {
            if self.is_session_locked() {
                warn!("lock: unlock request ignored (not the confirmed lock's valid unlock)");
            }
            return;
        }
        info!("lock: session unlocked");
        self.session_lock.reset_surfaces();
        self.force_keyboard_focus(None);
        self.hover_target = None;
        self.hover_deferred = false;
        self.reconcile_keyboard_focus();
        self.lock_redraw_all();
    }
}

/// The logical size of `output` for its lock surface.
fn lock_size(
    space: &Space<WindowElement>,
    output: &Output,
) -> Option<smithay::utils::Size<u32, Logical>> {
    let geo = space.output_geometry(output)?;
    Some(
        (
            u32::try_from(geo.size.w).unwrap_or(0),
            u32::try_from(geo.size.h).unwrap_or(0),
        )
            .into(),
    )
}

impl<B: Backend + 'static> SessionLockHandler for CrucibleState<B> {
    fn lock_state(&mut self) -> &mut SessionLockManagerState {
        &mut self.session_lock.manager
    }

    fn lock(&mut self, confirmation: SessionLocker) {
        let handle = confirmation.ext_session_lock().clone();
        let alive = self.session_lock.current_alive();
        if !self
            .session_lock
            .machine
            .request_lock(handle.clone(), confirmation, alive)
        {
            info!("lock: refusing a lock request while already locked");
            // The refused request dropped the unconfirmed locker, which sends `finished`.
            return;
        }
        let peer = handle.client().and_then(|c| self.peer_pid(&c));
        let glass = self.session_lock.glass.accept(peer, handle.clone());
        info!(glass, "lock: lock accepted; blanking every output");
        self.session_lock.reset_surfaces();
        // The lock wins over a reveal hold (and so releases any deferred capture).
        self.clear_reveal_holds();
        self.close_mapped_overlay();
        // The lock wins over snip: drop the selection and its frozen frames now.
        self.discard_snip();
        self.force_keyboard_focus(None);
        self.lock_redraw_all();
        // The lock arm, keyed to this lock (a replaced lock's arm is dropped unrun). It confirms
        // at once when no output is lit (an off output renders nothing, so no frame would ever
        // confirm it later); a lit output that has not presented a locked frame blocks it.
        drop(self.present_gate.disarm(ArmKind::Lock));
        self.arm_present_gate(
            ArmKind::Lock,
            "lock",
            Box::new(move |state| state.lock_confirm(&handle)),
        );
    }

    fn unlock(&mut self) {
        let alive = self.session_lock.current_alive();
        if self.session_lock.machine.unlock_called(alive) {
            self.handle.insert_idle(|state| state.lock_unlock_check());
        } else {
            warn!("lock: unlock request ignored (no live confirmed lock)");
        }
    }

    fn new_surface(&mut self, surface: LockSurface, output: WlOutput) {
        // Only the current locker's surfaces are shown.
        let ours = self
            .session_lock
            .machine
            .current()
            .and_then(Resource::client);
        let theirs = surface.wl_surface().client();
        if ours.is_none() || ours.map(|c| c.id()) != theirs.map(|c| c.id()) {
            warn!("lock: ignoring a lock surface from a client that holds no lock");
            return;
        }
        let Some(output) = Output::from_resource(&output) else {
            return;
        };
        let size = lock_size(&self.space, &output);
        surface.with_pending_state(|s| s.size = size);
        self.session_lock.surfaces.retain(|(o, _)| o != &output);
        self.session_lock
            .surfaces
            .push((output.clone(), surface.clone()));
        let focused_is_lock = matches!(
            self.keyboard_focus(),
            Some(KeyboardFocusTarget::LockSurface(ref s)) if s.is_alive()
        );
        if !focused_is_lock {
            self.force_keyboard_focus(Some(KeyboardFocusTarget::LockSurface(
                surface.wl_surface().clone(),
            )));
        }
        self.backend_data.reset_buffers(&output);
    }
}

#[cfg(test)]
mod tests {
    use super::{
        GlassEligibility, Layer, LockMachine, LockPhase, glass_stack, is_trusted_wallpaper,
    };
    use std::path::Path;

    #[test]
    fn a_matching_accept_binds_glass_to_that_lock_only() {
        let mut g: GlassEligibility<u8> = GlassEligibility::default();
        g.launched(Some(100));
        assert!(g.accept(Some(100), 1));
        assert!(g.is_bound(Some(&1)));
        assert!(!g.is_bound(Some(&2)), "bound to the accepted handle");
        assert!(!g.is_bound(None), "no current lock, no glass");
    }

    #[test]
    fn a_peer_pid_mismatch_is_not_glass() {
        let mut g: GlassEligibility<u8> = GlassEligibility::default();
        g.launched(Some(100));
        assert!(!g.accept(Some(101), 1));
        assert!(!g.accept(None, 2), "an unknown peer is never glass");
        assert!(!g.is_bound(Some(&2)));
        // The pending entry survives a foreign locker: the real one can still take the lock.
        assert!(g.accept(Some(100), 3));
        assert!(g.is_bound(Some(&3)));
    }

    #[test]
    fn a_locker_reaped_before_acceptance_is_not_eligible() {
        let mut g: GlassEligibility<u8> = GlassEligibility::default();
        g.launched(Some(100));
        g.reaped(&[7, 100]);
        assert!(!g.accept(Some(100), 1));
        assert!(!g.is_bound(Some(&1)));
        // An unrelated reap leaves the entry alone.
        g.launched(Some(200));
        g.reaped(&[100]);
        assert!(g.accept(Some(200), 2));
    }

    #[test]
    fn a_replacement_lock_is_not_glass_unless_it_was_itself_eligible() {
        let mut g: GlassEligibility<u8> = GlassEligibility::default();
        g.launched(Some(100));
        assert!(g.accept(Some(100), 1));
        // The locker crashed; a replacement (same or new PID) takes the lock without a spawn.
        assert!(!g.accept(Some(100), 2), "the entry was consumed");
        assert!(!g.is_bound(Some(&1)) && !g.is_bound(Some(&2)));
        // A replacement that is itself an eligible spawn is glass.
        g.launched(Some(300));
        assert!(g.accept(Some(300), 3));
        assert!(g.is_bound(Some(&3)));
    }

    #[test]
    fn a_later_launch_replaces_the_pending_entry() {
        let mut g: GlassEligibility<u8> = GlassEligibility::default();
        g.launched(Some(100));
        g.launched(Some(101));
        assert!(!g.accept(Some(100), 1), "replaced");
        assert!(g.accept(Some(101), 2));
        // An ineligible launch (or a failed spawn) clears it.
        g.launched(Some(102));
        g.launched(None);
        assert!(!g.accept(Some(102), 3));
    }

    #[test]
    fn only_a_swaybg_background_surface_is_trusted() {
        let swaybg = Some(Path::new("/usr/bin/swaybg"));
        assert!(is_trusted_wallpaper(Layer::Background, swaybg));
        assert!(!is_trusted_wallpaper(Layer::Bottom, swaybg));
        assert!(!is_trusted_wallpaper(Layer::Overlay, swaybg));
        for exe in [
            "/usr/bin/swaybg2",
            "/usr/bin/swaybg.sh",
            "/x/not-swaybg",
            "/",
        ] {
            assert!(
                !is_trusted_wallpaper(Layer::Background, Some(Path::new(exe))),
                "{exe}"
            );
        }
        assert!(!is_trusted_wallpaper(Layer::Background, None));
    }

    #[test]
    fn the_locked_stack_adds_wallpaper_only_for_glass_with_a_lock_element() {
        let wall = || vec![10, 11];
        // Glass, a lock element, trusted surfaces: lock first, wallpaper beneath.
        assert_eq!(glass_stack(true, vec![1], wall), vec![1, 10, 11]);
        // Glass with no trusted surface: the lock element over black.
        assert_eq!(glass_stack(true, vec![1], Vec::new), vec![1]);
        // Glass but no usable lock element: black, and the wallpaper is never even built.
        assert_eq!(
            glass_stack(true, Vec::<i32>::new(), || unreachable!("no lock element")),
            Vec::<i32>::new()
        );
        // Not glass: exactly today's stack.
        assert_eq!(
            glass_stack(false, vec![1], || unreachable!("not glass")),
            vec![1]
        );
        assert_eq!(
            glass_stack(false, Vec::<i32>::new(), || unreachable!("not glass")),
            Vec::<i32>::new()
        );
    }

    // The presentation rules (every lit output presents a post-acceptance frame, off outputs
    // exempt, a synchronous adoption commit counts, output removal re-evaluates) are the gate's
    // lock arm: `present_gate::tests`.

    #[test]
    fn a_replaced_locks_confirmation_never_confirms_its_successor() {
        let mut m: LockMachine<u8, &str> = LockMachine::default();
        assert!(m.request_lock(1, "first", false));
        // The pending locker died; a new one is accepted.
        assert!(m.request_lock(2, "second", false));
        assert_eq!(m.all_outputs_presented(&1), None, "a stale arm");
        assert!(matches!(m.phase(), LockPhase::Locking { pending: 2, .. }));
        assert_eq!(m.all_outputs_presented(&2), Some("second"));
        assert_eq!(m.phase(), &LockPhase::Locked { confirmed: 2 });
        assert_eq!(m.all_outputs_presented(&2), None, "confirms once");
    }

    #[test]
    fn accept_confirm_and_valid_unlock() {
        let mut m = LockMachine::default();
        assert!(m.request_lock(1, (), false));
        assert!(m.is_locked());
        assert!(m.all_outputs_presented(&1).is_some());
        assert_eq!(m.phase(), &LockPhase::Locked { confirmed: 1 });
        // Valid unlock_and_destroy: alive at the call, destroyed by the deferred check.
        assert!(m.unlock_called(true));
        assert!(m.unlock_check(false));
        assert!(!m.is_locked());
    }

    #[test]
    fn b3_rejected_second_lock_and_its_invalid_unlock_leave_cde_locked() {
        let mut m = LockMachine::default();
        assert!(m.request_lock(1, (), false));
        assert!(m.all_outputs_presented(&1).is_some());
        // A second locker while the confirmed one is alive is rejected.
        assert!(!m.request_lock(2, (), true));
        // Its invalid unlock: Smithay still calls unlock(); the confirmed lock stays alive.
        assert!(m.unlock_called(true));
        assert!(!m.unlock_check(true));
        assert_eq!(m.phase(), &LockPhase::Locked { confirmed: 1 });
    }

    #[test]
    fn unlock_after_locker_death_stays_locked() {
        let mut m = LockMachine::default();
        assert!(m.request_lock(1, (), false));
        assert!(m.all_outputs_presented(&1).is_some());
        assert!(!m.unlock_called(false));
        assert!(!m.unlock_check(false));
        assert!(m.is_locked());
    }

    #[test]
    fn new_locker_after_death_is_accepted_and_relocks() {
        let mut m = LockMachine::default();
        assert!(m.request_lock(1, (), false));
        assert!(m.all_outputs_presented(&1).is_some());
        assert!(m.request_lock(2, (), false));
        assert_eq!(
            m.phase(),
            &LockPhase::Locking {
                pending: 2,
                progress: ()
            }
        );
        assert!(m.all_outputs_presented(&2).is_some());
        assert!(m.unlock_called(true));
        assert!(m.unlock_check(false));
        assert!(!m.is_locked());
    }

    #[test]
    fn reject_while_locking_and_never_unlock_an_unconfirmed_lock() {
        let mut m = LockMachine::default();
        assert!(m.request_lock(1, (), false));
        assert!(!m.request_lock(2, (), true));
        // An unlock while still Locking (unconfirmed) is never a candidate.
        assert!(!m.unlock_called(true));
        assert!(!m.unlock_check(false));
        assert_eq!(
            m.phase(),
            &LockPhase::Locking {
                pending: 1,
                progress: ()
            }
        );
        // The pending locker died: a new locker may take over.
        assert!(m.request_lock(3, (), false));
        assert_eq!(
            m.phase(),
            &LockPhase::Locking {
                pending: 3,
                progress: ()
            }
        );
    }
}
