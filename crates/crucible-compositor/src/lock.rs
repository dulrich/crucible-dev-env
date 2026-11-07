//! Screen lock: CDE's `ext-session-lock-v1` policy over Smithay's protocol state.
//!
//! Smithay (`smithay::wayland::session_lock`) owns the protocol objects; CDE owns the policy. The
//! pure [`LockMachine`] is the single source of truth for whether the session is locked: rendering,
//! input and focus consult it, never Smithay's `locked_outputs`.
//!
//! ## States
//! - `Unlocked` — normal operation.
//! - `Locking { pending, progress }` — a locker was accepted (its locker and per-output presented
//!   set ride in the phase as [`LockProgress`]); every output shows black (plus its lock
//!   surface) and CDE waits until each output has *presented* such a frame before it sends
//!   `locked` (`SessionLocker::lock`).
//! - `Locked { confirmed }` — `locked` was sent. If the confirmed lock object dies without a
//!   valid unlock (the locker crashed) the session stays locked and black, and a new locker may
//!   take the lock (it goes through `Locking` again).
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
    desktop::{Space, utils::send_frames_surface_tree},
    output::Output,
    reexports::{
        wayland_protocols::ext::session_lock::v1::server::ext_session_lock_v1::ExtSessionLockV1,
        wayland_server::{Resource, protocol::wl_output::WlOutput},
    },
    utils::{Logical, Point, Scale},
    wayland::session_lock::{
        LockSurface, SessionLockHandler, SessionLockManagerState, SessionLocker,
    },
};
use tracing::{info, warn};

use crate::{
    element::WindowElement,
    focus::{KeyboardFocusTarget, PointerFocusTarget},
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
        /// What only exists while locking (the glue's [`LockProgress`]); handed back on
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

    /// The `Locking` payload, while locking.
    pub fn progress_mut(&mut self) -> Option<&mut P> {
        match &mut self.phase {
            LockPhase::Locking { progress, .. } => Some(progress),
            _ => None,
        }
    }

    /// Whether the session is locked or locking (only lock surfaces and black are shown).
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

    /// Every output presented a locked frame. Returns the `Locking` payload when this confirms the
    /// pending lock (the caller then sends `locked`), `None` otherwise.
    pub fn all_outputs_presented(&mut self) -> Option<P> {
        match std::mem::replace(&mut self.phase, LockPhase::Unlocked) {
            LockPhase::Locking { pending, progress } => {
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

/// CDE's session-lock glue state, held by [`CrucibleState`].
#[derive(Debug)]
pub struct SessionLock {
    /// Smithay's `ext_session_lock_manager_v1` global state.
    pub manager: SessionLockManagerState,
    /// The authoritative lock state; while `Locking` it carries the [`LockProgress`].
    pub machine: LockMachine<ExtSessionLockV1, LockProgress>,
    /// Lock surfaces, one per output.
    surfaces: Vec<(Output, LockSurface)>,
    /// Outputs (udev) whose last queued frame was rendered while locked.
    queued: Vec<Output>,
}

/// What exists only while a lock is pending: the accepted locker (consumed to send `locked`) and
/// the outputs that presented a locked frame since it was accepted.
#[derive(Debug)]
pub struct LockProgress {
    /// The accepted locker.
    locker: SessionLocker,
    /// Outputs that presented a locked frame since the lock was accepted.
    presented: Vec<Output>,
}

impl SessionLock {
    /// Wraps the manager global state; starts unlocked.
    #[must_use]
    pub fn new(manager: SessionLockManagerState) -> Self {
        Self {
            manager,
            machine: LockMachine::default(),
            surfaces: Vec::new(),
            queued: Vec::new(),
        }
    }

    /// Forgets every lock surface and queued-locked-frame flag (on accepting a lock and on
    /// unlocking).
    fn reset_surfaces(&mut self) {
        self.surfaces.clear();
        self.queued.clear();
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

    /// Records that `output`'s just-queued frame was (`locked`) or was not a locked frame (udev).
    pub fn note_queued(&mut self, output: &Output, locked: bool) {
        self.queued.retain(|o| o != output);
        if locked {
            self.queued.push(output.clone());
        }
    }

    /// Consumes the "locked frame queued" flag of `output` (udev page-flip completion).
    pub fn take_queued(&mut self, output: &Output) -> bool {
        let had = self.queued.contains(output);
        self.queued.retain(|o| o != output);
        had
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

/// Sends frame callbacks to `output`'s lock surface (while locked).
pub fn send_lock_frames(
    lock: &SessionLock,
    output: &Output,
    time: impl Into<std::time::Duration>,
    throttle: Option<std::time::Duration>,
) {
    if let Some(surface) = lock.surface_for(output) {
        send_frames_surface_tree(surface.wl_surface(), output, time, throttle, |_, _| {
            Some(output.clone())
        });
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
        let args: Vec<std::ffi::OsString> = words.map(Into::into).collect();
        info!(cmd, "lock: spawning the locker");
        if self
            .spawn(crate::persist::SpawnRequest::new(
                program,
                args,
                crate::persist::SpawnKind::Overlay,
            ))
            .is_none()
        {
            warn!(cmd, "lock: the locker could not be spawned");
        }
    }

    /// Records that `output` presented a locked frame; confirms the lock once every output did.
    pub(crate) fn lock_output_presented(&mut self, output: &Output) {
        let Some(progress) = self.session_lock.machine.progress_mut() else {
            return;
        };
        if !progress.presented.contains(output) {
            progress.presented.push(output.clone());
        }
        self.lock_confirm_if_presented();
    }

    /// Confirms a pending lock once every output presented a locked frame or is powered off (an
    /// off output shows nothing, so nothing unlocked is visible). Re-run after idle blanking.
    pub(crate) fn lock_confirm_if_presented(&mut self) {
        let LockPhase::Locking { progress, .. } = self.session_lock.machine.phase() else {
            return;
        };
        let all = all_presented_or_off(
            self.space.outputs(),
            |o| progress.presented.contains(o),
            |o| self.backend_data.output_powered_off(o),
        );
        if all && let Some(progress) = self.session_lock.machine.all_outputs_presented() {
            info!("lock: every output presented a locked frame; session locked");
            progress.locker.lock();
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
        let progress = LockProgress {
            locker: confirmation,
            presented: Vec::new(),
        };
        if !self
            .session_lock
            .machine
            .request_lock(handle, progress, alive)
        {
            info!("lock: refusing a lock request while already locked");
            // The refused request dropped the unconfirmed locker, which sends `finished`.
            return;
        }
        info!("lock: lock accepted; blanking every output");
        self.session_lock.reset_surfaces();
        self.close_mapped_overlay();
        // The lock wins over snip: drop the selection and its frozen frames now.
        self.discard_snip();
        self.force_keyboard_focus(None);
        self.lock_redraw_all();
        // Confirms at once when no output exists or every output is powered off by idle
        // blanking (an off output renders nothing, so no frame would ever confirm it later).
        // An output that is on and has not presented a locked frame still blocks confirmation.
        self.lock_confirm_if_presented();
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

/// Whether every output either presented a locked frame or is powered off (the Smithay-free core
/// of the lock-confirmation check).
fn all_presented_or_off<O>(
    mut outputs: impl Iterator<Item = O>,
    presented: impl Fn(&O) -> bool,
    powered_off: impl Fn(&O) -> bool,
) -> bool {
    outputs.all(|o| presented(&o) || powered_off(&o))
}

#[cfg(test)]
mod tests {
    use super::{LockMachine, LockPhase, all_presented_or_off};

    #[test]
    fn lock_confirms_when_every_output_presented_or_off() {
        let outputs = [1u8, 2, 3];
        // 1 presented, 2 powered off, 3 neither: not confirmed.
        assert!(!all_presented_or_off(
            outputs.iter(),
            |o| **o == 1,
            |o| **o == 2
        ));
        // 3 also off: confirmed.
        assert!(all_presented_or_off(
            outputs.iter(),
            |o| **o == 1,
            |o| **o != 1
        ));
        // All presented, none off: confirmed (the pre-blanking behaviour).
        assert!(all_presented_or_off(outputs.iter(), |_| true, |_| false));
        // An output whose power-off failed is not off, so it must still present.
        assert!(!all_presented_or_off(
            outputs.iter(),
            |o| **o != 2,
            |_| false
        ));
        // No outputs: vacuously confirmed (as `lock` relies on).
        assert!(all_presented_or_off(
            std::iter::empty::<u8>(),
            |_| false,
            |_| false
        ));
    }

    #[test]
    fn accept_confirm_and_valid_unlock() {
        let mut m = LockMachine::default();
        assert!(m.request_lock(1, (), false));
        assert!(m.is_locked());
        assert!(m.all_outputs_presented().is_some());
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
        assert!(m.all_outputs_presented().is_some());
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
        assert!(m.all_outputs_presented().is_some());
        assert!(!m.unlock_called(false));
        assert!(!m.unlock_check(false));
        assert!(m.is_locked());
    }

    #[test]
    fn new_locker_after_death_is_accepted_and_relocks() {
        let mut m = LockMachine::default();
        assert!(m.request_lock(1, (), false));
        assert!(m.all_outputs_presented().is_some());
        assert!(m.request_lock(2, (), false));
        assert_eq!(
            m.phase(),
            &LockPhase::Locking {
                pending: 2,
                progress: ()
            }
        );
        assert!(m.all_outputs_presented().is_some());
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
