//! Reveal hold (plan 38): the pure model ([`model`]) and its wiring to the Smithay [`Space`]
//! (the deadline timer, renderer probing, release and the render admission).
//!
//! [`Space`]: smithay::desktop::Space

mod model;

pub use model::{
    REVEAL_HOLD_TIMEOUT, RenderAdmission, RevealHolds, is_placeholder_buffer, output_for_rect,
};

use crucible_layout::Rect;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use tracing::{debug, warn};

use crate::element::WindowElement;
use crate::state::{Backend, CrucibleState};

/// Reveal-hold wiring (plan 38): the pure model is [`model`].
impl<B: Backend + 'static> CrucibleState<B> {
    /// The one "lock and snip win" predicate: while the session is locked or a snip is active, no
    /// hold begins and the render pass is never held. The hold set is cleared at the moment the
    /// rule starts to apply ([`Self::clear_reveal_holds`], called when a lock is accepted and when
    /// snip starts, before `snip` is set), so while this is true there is nothing left to hold.
    fn reveal_holds_suppressed(&self) -> bool {
        self.session_lock.is_locked() || self.snip.is_some()
    }

    /// Starts (or extends) a reveal hold for `element`, just re-shown by a configure `serial`
    /// that cleared `Suspended`, on the output it landed on. Only an xdg toplevel whose committed
    /// buffer is a placeholder holds ([`is_placeholder_buffer`]); a client that kept a full-size
    /// buffer (foot) never does. Lock and snip win: no new hold while either is active.
    pub(crate) fn begin_reveal_hold(
        &mut self,
        element: &WindowElement,
        placed: smithay::utils::Rectangle<i32, smithay::utils::Logical>,
        serial: smithay::utils::Serial,
    ) {
        use smithay::reexports::calloop::timer::{TimeoutAction, Timer};

        if self.reveal_holds_suppressed() {
            return;
        }
        let Some(root) = element.toplevel().map(|t| t.wl_surface().clone()) else {
            return;
        };
        // Not `outputs_for_element`: Smithay fills that only in `Space::refresh`, and a window
        // returning from hidden was unmapped, so its fresh entry has no outputs yet.
        let geometries: Vec<_> = self
            .space
            .outputs()
            .filter_map(|o| {
                self.space
                    .output_geometry(o)
                    .map(|g| (o.clone(), Rect::new(g.loc.x, g.loc.y, g.size.w, g.size.h)))
            })
            .collect();
        let Some(output) = output_for_rect(
            &geometries,
            Rect::new(placed.loc.x, placed.loc.y, placed.size.w, placed.size.h),
        ) else {
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
    pub(crate) fn render_admission(&self, output: &smithay::output::Output) -> RenderAdmission {
        if self.reveal_holds_suppressed() {
            return RenderAdmission::Render;
        }
        self.reveal_holds.admission(output)
    }
}
