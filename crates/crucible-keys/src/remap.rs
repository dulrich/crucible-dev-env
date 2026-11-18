//! Per-device key remapping: the pure engine behind the `input-device` config block.
//!
//! A [`RemapSet`] holds one full-takeover table per USB `(vendor, product)`; [`Remapper`] runs
//! every keyboard event through it before the seat sees it. All codes here are Linux **evdev**
//! codes (the compositor converts backend codes, evdev + 8, at its edge).
//!
//! - A key from a **configured** device resolves through its table (the layer's entries
//!   overriding while the layer key is held): a key target forwards the rewritten code, an
//!   action target fires on press and is swallowed, an unmapped key is swallowed. The layer key
//!   itself is swallowed.
//! - A key from any **other** device forwards its own code.
//! - The **press ledger**, keyed by `(source id, original code)`, records each press's outcome;
//!   the release replays it, whatever the layer or table is by then.
//! - The **holder count** per effective code makes the seat see one press/release pair per code
//!   even when two sources hold it (the pad's `KEY_1` and the main keyboard's `1`): a press
//!   forwards only on 0→1, a release only on 1→0. Every keyboard event goes through it.

use std::collections::HashMap;

use crate::action::{Action, ActionParseError};

/// What a source key resolves to in a table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RemapTarget {
    /// Forward this evdev code instead.
    Key(u16),
    /// Run this action (on press) and swallow the key.
    Action(Action),
}

/// An uncompiled device table: codes plus raw action strings (the config's shape).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct DeviceSpec {
    /// USB vendor id.
    pub vendor: u16,
    /// USB product id.
    pub product: u16,
    /// Base entries: `(source, target)`.
    pub maps: Vec<(u16, SpecTarget)>,
    /// The hold layer: its key and override entries.
    pub layer: Option<(u16, Vec<(u16, SpecTarget)>)>,
}

/// An uncompiled target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SpecTarget {
    /// A target evdev code.
    Key(u16),
    /// A raw action string, compiled with [`Action`]'s parser.
    Action(String),
}

/// An action string in a device table that does not parse.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("input-device {vendor:04x}:{product:04x}: action `{action}`: {source}")]
pub struct RemapError {
    /// The device's vendor id.
    pub vendor: u16,
    /// The device's product id.
    pub product: u16,
    /// The raw action text.
    pub action: String,
    /// Why it does not parse.
    pub source: ActionParseError,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
struct DeviceTable {
    base: HashMap<u16, RemapTarget>,
    layer: Option<(u16, HashMap<u16, RemapTarget>)>,
}

/// Compiled tables keyed by `(vendor, product)`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RemapSet {
    devices: HashMap<(u16, u16), DeviceTable>,
}

impl RemapSet {
    /// Compiles `specs`; the first action that does not parse fails the whole set.
    ///
    /// # Errors
    /// [`RemapError`] naming the device and action.
    pub fn compile(specs: &[DeviceSpec]) -> Result<Self, RemapError> {
        let mut devices = HashMap::new();
        for spec in specs {
            let compile = |maps: &[(u16, SpecTarget)]| -> Result<_, RemapError> {
                maps.iter()
                    .map(|(src, target)| {
                        let target = match target {
                            SpecTarget::Key(code) => RemapTarget::Key(*code),
                            SpecTarget::Action(text) => {
                                RemapTarget::Action(text.parse().map_err(|source| RemapError {
                                    vendor: spec.vendor,
                                    product: spec.product,
                                    action: text.clone(),
                                    source,
                                })?)
                            }
                        };
                        Ok((*src, target))
                    })
                    .collect::<Result<HashMap<_, _>, _>>()
            };
            let table = DeviceTable {
                base: compile(&spec.maps)?,
                layer: match &spec.layer {
                    Some((key, maps)) => Some((*key, compile(maps)?)),
                    None => None,
                },
            };
            devices.insert((spec.vendor, spec.product), table);
        }
        Ok(Self { devices })
    }

    /// Whether no device is configured.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.devices.is_empty()
    }
}

/// What the compositor does with one key event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RemapOutcome {
    /// Hand this evdev code to the seat (with the event's own state).
    Forward(u16),
    /// Run this action; the key is swallowed.
    Action(Action),
    /// Drop the event.
    Swallow,
}

/// A press's recorded outcome, replayed by its release.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Recorded {
    /// Forwarded as this effective code (counted in the holders).
    Forward(u16),
    /// Swallowed (unmapped, or an action).
    Swallow,
    /// The layer key of this device.
    Layer((u16, u16)),
}

/// The stateful resolver: the current [`RemapSet`], the press ledger and the holder counts.
#[derive(Debug, Clone, Default)]
pub struct Remapper {
    set: RemapSet,
    ledger: HashMap<(String, u16), Recorded>,
    holders: HashMap<u16, u32>,
}

impl Remapper {
    /// An empty remapper (every key forwards unchanged).
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Swaps in a new table set. The ledger and holder counts survive, so a key held across a
    /// reload still releases as it was pressed.
    pub fn set_remaps(&mut self, set: RemapSet) {
        self.set = set;
    }

    /// Forgets every ledger entry of `source` (a removed device), keeping the holder counts in
    /// step, so a reused source id inherits no held key or layer.
    pub fn remove_source(&mut self, source: &str) {
        let mut released = Vec::new();
        self.ledger.retain(|(src, _), rec| {
            if src != source {
                return true;
            }
            if let Recorded::Forward(code) = rec {
                released.push(*code);
            }
            false
        });
        for code in released {
            self.release_holder(code);
        }
    }

    fn layer_held(&self, device: (u16, u16)) -> bool {
        self.ledger
            .values()
            .any(|rec| *rec == Recorded::Layer(device))
    }

    /// Decrements `code`'s holders; true when it reached zero.
    fn release_holder(&mut self, code: u16) -> bool {
        match self.holders.get_mut(&code) {
            Some(n) if *n > 1 => {
                *n -= 1;
                false
            }
            Some(_) => {
                self.holders.remove(&code);
                true
            }
            None => false,
        }
    }

    /// Resolves one key event: `source` is the event-source identity (libinput sysname),
    /// `selector` the device's `(vendor, product)` if known, `code` the evdev code.
    pub fn process(
        &mut self,
        source: &str,
        selector: Option<(u16, u16)>,
        code: u16,
        pressed: bool,
    ) -> RemapOutcome {
        let ledger_key = (source.to_string(), code);
        if !pressed {
            return match self.ledger.remove(&ledger_key) {
                Some(Recorded::Forward(eff)) => {
                    if self.release_holder(eff) {
                        RemapOutcome::Forward(eff)
                    } else {
                        RemapOutcome::Swallow
                    }
                }
                Some(Recorded::Swallow | Recorded::Layer(_)) => RemapOutcome::Swallow,
                // A release with no recorded press (held before startup): an unconfigured
                // device's key forwards as before unless another source holds the code.
                None => {
                    let configured = selector.is_some_and(|s| self.set.devices.contains_key(&s));
                    if configured || self.holders.contains_key(&code) {
                        RemapOutcome::Swallow
                    } else {
                        RemapOutcome::Forward(code)
                    }
                }
            };
        }
        // A repeated press (no release in between) changes nothing.
        if self.ledger.contains_key(&ledger_key) {
            return RemapOutcome::Swallow;
        }
        let device = selector.filter(|s| self.set.devices.contains_key(s));
        let (recorded, outcome) = match device {
            None => (Recorded::Forward(code), None),
            Some(device) => {
                let table = &self.set.devices[&device];
                match &table.layer {
                    Some((key, _)) if *key == code => (Recorded::Layer(device), None),
                    layer => {
                        let target = layer
                            .as_ref()
                            .filter(|_| self.layer_held(device))
                            .and_then(|(_, maps)| maps.get(&code))
                            .or_else(|| table.base.get(&code));
                        match target {
                            Some(RemapTarget::Key(eff)) => (Recorded::Forward(*eff), None),
                            Some(RemapTarget::Action(action)) => (
                                Recorded::Swallow,
                                Some(RemapOutcome::Action(action.clone())),
                            ),
                            None => (Recorded::Swallow, None),
                        }
                    }
                }
            }
        };
        self.ledger.insert(ledger_key, recorded);
        if let Some(outcome) = outcome {
            return outcome;
        }
        match recorded {
            Recorded::Forward(eff) => {
                let n = self.holders.entry(eff).or_insert(0);
                *n += 1;
                if *n == 1 {
                    RemapOutcome::Forward(eff)
                } else {
                    RemapOutcome::Swallow
                }
            }
            _ => RemapOutcome::Swallow,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use RemapOutcome::{Forward, Swallow};

    const PAD: (u16, u16) = (0x1532, 0x0207);
    // evdev codes.
    const GRAVE: u16 = 41;
    const K1: u16 = 2;
    const K2: u16 = 3;
    const K3: u16 = 4;
    const K4: u16 = 5;
    const K5: u16 = 6;
    const K6: u16 = 7;
    const K7: u16 = 8;
    const K8: u16 = 9;
    const K9: u16 = 10;
    const K0: u16 = 11;
    const TAB: u16 = 15;
    const Q: u16 = 16;
    const W: u16 = 17;
    const E: u16 = 18;
    const R: u16 = 19;
    const A: u16 = 30;
    const SPACE: u16 = 57;
    const ENTER: u16 = 28;
    const LEFTALT: u16 = 56;
    const LEFTMETA: u16 = 125;
    const UP: u16 = 103;
    const DOWN: u16 = 108;
    const LEFT: u16 = 105;
    const RIGHT: u16 = 106;

    const PAD_SOURCES: [u16; 10] = [GRAVE, K1, K2, K3, K4, TAB, Q, W, E, R];
    const DIGITS: [u16; 10] = [K1, K2, K3, K4, K5, K6, K7, K8, K9, K0];

    fn key(src: u16, dst: u16) -> (u16, SpecTarget) {
        (src, SpecTarget::Key(dst))
    }

    fn arrows_and_enter() -> Vec<(u16, SpecTarget)> {
        vec![
            key(UP, UP),
            key(DOWN, DOWN),
            key(LEFT, LEFT),
            key(RIGHT, RIGHT),
            key(SPACE, ENTER),
        ]
    }

    /// The docs' starting mapping.
    fn starting() -> DeviceSpec {
        let mut maps: Vec<_> = PAD_SOURCES
            .iter()
            .zip(DIGITS)
            .map(|(s, d)| key(*s, d))
            .collect();
        maps.extend(arrows_and_enter());
        maps.push(key(LEFTALT, LEFTMETA));
        DeviceSpec {
            vendor: PAD.0,
            product: PAD.1,
            maps,
            layer: None,
        }
    }

    /// The docs' reversed variant.
    fn reversed() -> DeviceSpec {
        let mut maps: Vec<_> = PAD_SOURCES
            .iter()
            .zip(DIGITS)
            .enumerate()
            .map(|(i, (s, _))| (*s, SpecTarget::Action(format!("slot {}", (i + 1) % 10))))
            .collect();
        maps.extend(arrows_and_enter());
        let layer = PAD_SOURCES
            .iter()
            .zip(DIGITS)
            .map(|(s, d)| key(*s, d))
            .collect();
        DeviceSpec {
            vendor: PAD.0,
            product: PAD.1,
            maps,
            layer: Some((LEFTALT, layer)),
        }
    }

    fn remapper(spec: DeviceSpec) -> Remapper {
        let mut r = Remapper::new();
        r.set_remaps(RemapSet::compile(&[spec]).unwrap());
        r
    }

    fn tap(r: &mut Remapper, src: &str, sel: Option<(u16, u16)>, code: u16) -> [RemapOutcome; 2] {
        [
            r.process(src, sel, code, true),
            r.process(src, sel, code, false),
        ]
    }

    #[test]
    fn unconfigured_device_passes_through() {
        let mut r = remapper(starting());
        assert_eq!(
            tap(&mut r, "event3", Some((1, 2)), A),
            [Forward(A), Forward(A)]
        );
        assert_eq!(
            tap(&mut r, "event3", None, GRAVE),
            [Forward(GRAVE), Forward(GRAVE)]
        );
        let mut empty = Remapper::new();
        assert_eq!(
            tap(&mut empty, "e", Some(PAD), GRAVE),
            [Forward(GRAVE), Forward(GRAVE)]
        );
    }

    #[test]
    fn unmapped_key_is_swallowed_and_key_maps_rewrite() {
        let mut r = remapper(starting());
        assert_eq!(tap(&mut r, "pad", Some(PAD), A), [Swallow, Swallow]);
        assert_eq!(
            tap(&mut r, "pad", Some(PAD), GRAVE),
            [Forward(K1), Forward(K1)]
        );
        assert_eq!(
            tap(&mut r, "pad", Some(PAD), SPACE),
            [Forward(ENTER), Forward(ENTER)]
        );
        assert_eq!(
            tap(&mut r, "pad", Some(PAD), LEFTALT),
            [Forward(LEFTMETA), Forward(LEFTMETA)]
        );
    }

    #[test]
    fn starting_table_maps_every_documented_key() {
        let mut r = remapper(starting());
        for (s, d) in PAD_SOURCES.iter().zip(DIGITS) {
            assert_eq!(tap(&mut r, "pad", Some(PAD), *s), [Forward(d), Forward(d)]);
        }
        for arrow in [UP, DOWN, LEFT, RIGHT] {
            assert_eq!(
                tap(&mut r, "pad", Some(PAD), arrow),
                [Forward(arrow), Forward(arrow)]
            );
        }
    }

    #[test]
    fn action_fires_on_press_only_and_layer_overrides() {
        let mut r = remapper(reversed());
        assert_eq!(
            tap(&mut r, "pad", Some(PAD), GRAVE),
            [RemapOutcome::Action(Action::Slot(1)), Swallow]
        );
        assert_eq!(
            tap(&mut r, "pad", Some(PAD), R),
            [RemapOutcome::Action(Action::Slot(0)), Swallow]
        );
        // The layer key (on another node of the same device) is swallowed and enables the layer.
        assert_eq!(r.process("pad-b", Some(PAD), LEFTALT, true), Swallow);
        assert_eq!(
            tap(&mut r, "pad", Some(PAD), GRAVE),
            [Forward(K1), Forward(K1)]
        );
        assert_eq!(tap(&mut r, "pad", Some(PAD), R), [Forward(K0), Forward(K0)]);
        // Keys the layer does not list fall back to the base table.
        assert_eq!(
            tap(&mut r, "pad", Some(PAD), SPACE),
            [Forward(ENTER), Forward(ENTER)]
        );
        assert_eq!(r.process("pad-b", Some(PAD), LEFTALT, false), Swallow);
        assert_eq!(
            r.process("pad", Some(PAD), Q, true),
            RemapOutcome::Action(Action::Slot(7))
        );
    }

    #[test]
    fn release_pairs_with_press_across_layer_change() {
        let mut r = remapper(reversed());
        r.process("pad", Some(PAD), LEFTALT, true);
        assert_eq!(r.process("pad", Some(PAD), GRAVE, true), Forward(K1));
        r.process("pad", Some(PAD), LEFTALT, false);
        // Layer gone: the release still replays the press's outcome.
        assert_eq!(r.process("pad", Some(PAD), GRAVE, false), Forward(K1));
        // And the other way round: pressed as an action, released after the layer engaged.
        assert_eq!(
            r.process("pad", Some(PAD), GRAVE, true),
            RemapOutcome::Action(Action::Slot(1))
        );
        r.process("pad", Some(PAD), LEFTALT, true);
        assert_eq!(r.process("pad", Some(PAD), GRAVE, false), Swallow);
    }

    #[test]
    fn release_pairs_with_press_across_table_reload() {
        let mut r = remapper(starting());
        assert_eq!(r.process("pad", Some(PAD), GRAVE, true), Forward(K1));
        r.set_remaps(RemapSet::compile(&[reversed()]).unwrap());
        assert_eq!(r.process("pad", Some(PAD), GRAVE, false), Forward(K1));
        r.set_remaps(RemapSet::default());
        // Unconfigured now: a press recorded as swallowed still releases swallowed.
        let mut r = remapper(starting());
        r.process("pad", Some(PAD), A, true);
        r.set_remaps(RemapSet::default());
        assert_eq!(r.process("pad", Some(PAD), A, false), Swallow);
    }

    #[test]
    fn seat_code_ownership_regression() {
        // Pad Super+1 held; the main keyboard presses and releases 1; the pad releases. The seat
        // sees exactly one press and one release per effective code.
        let mut r = remapper(starting());
        let kbd = Some((0x046d, 0xc31c));
        assert_eq!(
            r.process("pad", Some(PAD), LEFTALT, true),
            Forward(LEFTMETA)
        );
        assert_eq!(r.process("pad", Some(PAD), GRAVE, true), Forward(K1));
        assert_eq!(r.process("kbd", kbd, K1, true), Swallow);
        assert_eq!(r.process("kbd", kbd, K1, false), Swallow);
        assert_eq!(r.process("pad", Some(PAD), GRAVE, false), Forward(K1));
        // Real Super held while the pad's Super is pressed and released.
        assert_eq!(r.process("kbd", kbd, LEFTMETA, true), Swallow);
        assert_eq!(r.process("pad", Some(PAD), LEFTALT, false), Swallow);
        assert_eq!(r.process("kbd", kbd, LEFTMETA, false), Forward(LEFTMETA));
        assert!(r.holders.is_empty() && r.ledger.is_empty());
    }

    #[test]
    fn repeated_press_and_orphan_release() {
        let mut r = remapper(starting());
        assert_eq!(r.process("pad", Some(PAD), GRAVE, true), Forward(K1));
        assert_eq!(r.process("pad", Some(PAD), GRAVE, true), Swallow);
        assert_eq!(r.process("pad", Some(PAD), GRAVE, false), Forward(K1));
        // A release never pressed: a configured device swallows, another device forwards.
        assert_eq!(r.process("pad", Some(PAD), GRAVE, false), Swallow);
        assert_eq!(r.process("kbd", None, A, false), Forward(A));
    }

    #[test]
    fn removed_source_drops_its_entries_and_layer() {
        let mut r = remapper(reversed());
        r.process("pad", Some(PAD), LEFTALT, true);
        assert_eq!(r.process("pad", Some(PAD), GRAVE, true), Forward(K1));
        r.remove_source("pad");
        assert!(r.holders.is_empty() && r.ledger.is_empty());
        // The reused id holds no layer.
        assert_eq!(
            r.process("pad", Some(PAD), GRAVE, true),
            RemapOutcome::Action(Action::Slot(1))
        );
    }

    #[test]
    fn bad_action_fails_compile_and_none_is_rejected() {
        for bad in ["none", "frobnicate", "slot 12"] {
            let spec = DeviceSpec {
                vendor: PAD.0,
                product: PAD.1,
                maps: vec![(A, SpecTarget::Action(bad.into()))],
                layer: None,
            };
            let err = RemapSet::compile(&[spec]).unwrap_err();
            assert_eq!(err.action, bad);
            assert!(err.to_string().contains("1532:0207"), "{err}");
        }
    }
}
