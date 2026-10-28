//! The pre-warmed overlay host's slot as a pure state machine. Transitions return the effects the
//! compositor must run (see `CrucibleState::run_warm_effects`); the element type is generic so the
//! transitions are unit-tested without Smithay.

use std::path::PathBuf;
use std::time::Instant;

/// A spawned warm host: `crucible overlay-host --socket <socket>` in the overlay terminal.
#[derive(Debug, Clone)]
pub(super) struct WarmHost {
    /// The spawned terminal's PID (killed to end it, kept as the overlay's PID once used).
    pub(super) pid: u32,
    /// The host's mode socket.
    pub(super) socket: PathBuf,
    /// Monotonic spawn time (for the log line when it is used).
    pub(super) spawned_at: Instant,
    /// Whether this host may be respawned once if it dies before use (re-armed by an overlay
    /// close and at startup).
    pub(super) respawn_ok: bool,
}

/// The warm slot. `E` is the hidden toplevel's element type.
#[derive(Debug)]
pub(super) enum WarmSlot<E> {
    /// Warming is off for the session (the warm host came up as an X11 window).
    Disabled,
    /// No warm host runs; `respawn_ok` is carried into the next spawn.
    Idle { respawn_ok: bool },
    /// Spawned, its toplevel has not arrived yet.
    Spawning(WarmHost),
    /// Its toplevel arrived and is stored hidden (never in the Space while warm).
    Ready(WarmHost, E),
}

impl<E> Default for WarmSlot<E> {
    fn default() -> Self {
        Self::Idle { respawn_ok: false }
    }
}

/// An effect a warm-slot transition asks the compositor to run, in order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum WarmEffect<E> {
    /// Forget any pending placement for `pid`, kill it, else politely close `element`.
    Kill {
        pid: Option<u32>,
        element: Option<E>,
    },
    /// Remove a host's mode socket.
    RemoveSocket(PathBuf),
    /// Spawn a warm host if the slot is idle (it then calls [`WarmSlot::spawned`]).
    Spawn,
}

impl<E> WarmSlot<E> {
    /// The live host, spawning or ready.
    pub(super) fn host(&self) -> Option<&WarmHost> {
        match self {
            Self::Spawning(host) | Self::Ready(host, _) => Some(host),
            Self::Disabled | Self::Idle { .. } => None,
        }
    }

    /// The hidden toplevel, once it arrived.
    pub(super) fn element(&self) -> Option<&E> {
        match self {
            Self::Ready(_, element) => Some(element),
            _ => None,
        }
    }

    /// Whether a spawn may start now (idle, not disabled, no host).
    pub(super) fn can_spawn(&self) -> bool {
        matches!(self, Self::Idle { .. })
    }

    /// A spawn started: the slot holds the new host. Ignored unless idle.
    pub(super) fn spawned(&mut self, pid: u32, socket: PathBuf, spawned_at: Instant) {
        if let Self::Idle { respawn_ok } = *self {
            *self = Self::Spawning(WarmHost {
                pid,
                socket,
                spawned_at,
                respawn_ok,
            });
        }
    }

    /// Startup or an overlay closed: re-arm the one-respawn allowance and spawn if idle.
    pub(super) fn arm(&mut self) -> Vec<WarmEffect<E>> {
        match self {
            Self::Disabled => Vec::new(),
            Self::Idle { respawn_ok } => {
                *respawn_ok = true;
                vec![WarmEffect::Spawn]
            }
            Self::Spawning(host) | Self::Ready(host, _) => {
                host.respawn_ok = true;
                Vec::new()
            }
        }
    }

    /// A warm-host toplevel arrived (`pid` from the arrival, `x11` when it is an X11 window). With
    /// no live host it is a stray: ended, slot unchanged. An X11 host disables warming.
    pub(super) fn arrived(
        &mut self,
        element: E,
        pid: Option<u32>,
        x11: bool,
    ) -> Vec<WarmEffect<E>> {
        match std::mem::take(self) {
            Self::Spawning(host) | Self::Ready(host, _) if x11 => {
                *self = Self::Disabled;
                vec![
                    WarmEffect::Kill {
                        pid: Some(host.pid),
                        element: Some(element),
                    },
                    WarmEffect::RemoveSocket(host.socket),
                ]
            }
            Self::Spawning(host) | Self::Ready(host, _) => {
                *self = Self::Ready(host, element);
                Vec::new()
            }
            idle_or_disabled => {
                *self = idle_or_disabled;
                vec![WarmEffect::Kill {
                    pid,
                    element: Some(element),
                }]
            }
        }
    }

    /// Kills the live host (if any) and empties the slot; never respawns. Used when it is not ready
    /// at open and when the hand-off fails (the caller then cold-spawns the overlay).
    pub(super) fn discard(&mut self) -> Vec<WarmEffect<E>> {
        let (host, element) = match std::mem::take(self) {
            Self::Spawning(host) => (host, None),
            Self::Ready(host, element) => (host, Some(element)),
            other => {
                *self = other;
                return Vec::new();
            }
        };
        *self = Self::Idle {
            respawn_ok: host.respawn_ok,
        };
        vec![
            WarmEffect::Kill {
                pid: Some(host.pid),
                element,
            },
            WarmEffect::RemoveSocket(host.socket),
        ]
    }

    /// The host died before use (reaped, or with `element_gone` its surface was destroyed): discard
    /// it and respawn at most once per overlay close. Empty without a live host.
    pub(super) fn died(&mut self, element_gone: bool) -> Vec<WarmEffect<E>> {
        let respawn = self.host().is_some_and(|h| h.respawn_ok);
        let mut effects = self.discard();
        if effects.is_empty() {
            return effects;
        }
        if element_gone {
            // Already gone: never try to close it.
            if let Some(WarmEffect::Kill { element, .. }) = effects.first_mut() {
                *element = None;
            }
        }
        *self = Self::Idle { respawn_ok: false };
        if respawn {
            effects.push(WarmEffect::Spawn);
        }
        effects
    }

    /// The 1 s tick's reaped PIDs: [`Self::died`] when the live host is among them.
    pub(super) fn reaped(&mut self, reaped: &[u32]) -> Vec<WarmEffect<E>> {
        if self.host().is_some_and(|h| reaped.contains(&h.pid)) {
            self.died(false)
        } else {
            Vec::new()
        }
    }

    /// Hands out the ready host and its element for an overlay open, leaving the slot idle (the
    /// caller re-arms it with [`Self::arm`]). `None` (slot unchanged) unless ready.
    pub(super) fn take_ready(&mut self) -> Option<(WarmHost, E)> {
        match std::mem::take(self) {
            Self::Ready(host, element) => {
                *self = Self::Idle {
                    respawn_ok: host.respawn_ok,
                };
                Some((host, element))
            }
            other => {
                *self = other;
                None
            }
        }
    }

    /// The overlay terminal config changed: replace a live host with a fresh one.
    pub(super) fn config_changed(&mut self) -> Vec<WarmEffect<E>> {
        let mut effects = self.discard();
        if !effects.is_empty() {
            effects.push(WarmEffect::Spawn);
        }
        effects
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    type Slot = WarmSlot<&'static str>;
    type Fx = WarmEffect<&'static str>;

    fn sock(n: u32) -> PathBuf {
        PathBuf::from(format!("/s{n}.sock"))
    }

    /// A slot armed at startup whose spawn of `pid` started.
    fn spawning(pid: u32) -> Slot {
        let mut slot = Slot::default();
        assert_eq!(slot.arm(), vec![Fx::Spawn]);
        slot.spawned(pid, sock(pid), Instant::now());
        slot
    }

    fn ready(pid: u32) -> Slot {
        let mut slot = spawning(pid);
        assert_eq!(slot.arrived("w", Some(pid), false), vec![]);
        slot
    }

    fn discard_fx(pid: u32, element: Option<&'static str>) -> Vec<Fx> {
        vec![
            Fx::Kill {
                pid: Some(pid),
                element,
            },
            Fx::RemoveSocket(sock(pid)),
        ]
    }

    #[test]
    fn wayland_arrival_makes_the_host_ready() {
        let slot = ready(7);
        assert_eq!(slot.element(), Some(&"w"));
        assert_eq!(slot.host().map(|h| h.pid), Some(7));
    }

    #[test]
    fn x11_arrival_disables_warming_and_later_spawns_are_noops() {
        let mut slot = spawning(7);
        assert_eq!(slot.arrived("x", Some(7), true), discard_fx(7, Some("x")));
        assert!(matches!(slot, WarmSlot::Disabled));
        assert_eq!(slot.arm(), vec![]);
        assert!(!slot.can_spawn());
        slot.spawned(8, sock(8), Instant::now());
        assert!(matches!(slot, WarmSlot::Disabled));
        assert_eq!(slot.config_changed(), vec![]);
        assert_eq!(slot.died(false), vec![]);
    }

    #[test]
    fn stray_arrival_without_a_host_is_ended_and_leaves_the_slot() {
        let mut slot = Slot::Idle { respawn_ok: true };
        assert_eq!(
            slot.arrived("s", Some(9), false),
            vec![Fx::Kill {
                pid: Some(9),
                element: Some("s")
            }]
        );
        assert!(matches!(slot, WarmSlot::Idle { respawn_ok: true }));
        let mut slot = Slot::Disabled;
        assert_eq!(
            slot.arrived("s", None, true),
            vec![Fx::Kill {
                pid: None,
                element: Some("s")
            }]
        );
        assert!(matches!(slot, WarmSlot::Disabled));
    }

    #[test]
    fn hand_off_failure_discards_and_leaves_a_cold_spawn() {
        let mut slot = ready(7);
        // The mode line could not be sent: discard; no warm respawn (the overlay spawns cold).
        assert_eq!(slot.discard(), discard_fx(7, Some("w")));
        assert!(slot.can_spawn());
        assert!(slot.take_ready().is_none());
    }

    #[test]
    fn not_ready_at_open_discards() {
        let mut slot = spawning(7);
        assert!(slot.take_ready().is_none());
        assert!(matches!(slot, WarmSlot::Spawning(_)));
        assert_eq!(slot.discard(), discard_fx(7, None));
        assert!(matches!(slot, WarmSlot::Idle { respawn_ok: true }));
        assert_eq!(slot.discard(), vec![]);
    }

    #[test]
    fn died_before_use_respawns_once_then_waits_for_a_rearm() {
        let mut slot = ready(7);
        let mut fx = discard_fx(7, Some("w"));
        fx.push(Fx::Spawn);
        assert_eq!(slot.reaped(&[1, 7]), fx);
        slot.spawned(8, sock(8), Instant::now());
        assert_eq!(slot.reaped(&[1]), vec![]);
        // The respawned host dies too (surface destroyed): no further respawn.
        assert_eq!(slot.died(true), discard_fx(8, None));
        assert!(matches!(slot, WarmSlot::Idle { respawn_ok: false }));
        // An overlay close re-arms and spawns.
        assert_eq!(slot.arm(), vec![Fx::Spawn]);
        slot.spawned(9, sock(9), Instant::now());
        let mut fx = discard_fx(9, None);
        fx.push(Fx::Spawn);
        assert_eq!(slot.died(false), fx);
    }

    #[test]
    fn take_for_hand_off_empties_the_slot_and_the_rearm_spawns() {
        let mut slot = ready(7);
        let (host, element) = slot.take_ready().expect("ready");
        assert_eq!((host.pid, element), (7, "w"));
        assert!(slot.host().is_none());
        assert_eq!(slot.arm(), vec![Fx::Spawn]);
    }

    #[test]
    fn config_change_replaces_a_live_host_only() {
        let mut slot = ready(7);
        let mut fx = discard_fx(7, Some("w"));
        fx.push(Fx::Spawn);
        assert_eq!(slot.config_changed(), fx);
        assert!(slot.can_spawn());
        assert_eq!(slot.config_changed(), vec![]);
        assert_eq!(Slot::Disabled.config_changed(), vec![]);
    }
}
