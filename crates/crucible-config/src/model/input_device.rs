//! Per-device key remap tables (`input-device "<vendor>:<product>" { … }`). Codes are Linux
//! evdev codes (resolved from `KEY_*` names at parse time); action strings stay raw and are
//! compiled by `crucible-keys`.

/// What one source key becomes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeyMapTarget {
    /// Rewrite to this evdev code.
    Key(u16),
    /// Run this action string (compiled by `crucible-keys`'s `Action` parser) and swallow the key.
    Action(String),
}

/// One `map "<SRC>" …` entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyMapEntry {
    /// The source evdev code.
    pub source: u16,
    /// What it becomes.
    pub target: KeyMapTarget,
}

/// The device's one hold layer (`layer "<SRC>" { map … }`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InputDeviceLayer {
    /// The evdev code that activates the layer while held (itself swallowed).
    pub key: u16,
    /// Entries overriding the base table while the layer key is held.
    pub maps: Vec<KeyMapEntry>,
}

/// One `input-device` block: a full-takeover table for every node with this vendor:product.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InputDeviceConfig {
    /// USB vendor id.
    pub vendor: u16,
    /// USB product id.
    pub product: u16,
    /// The base table, in file order.
    pub maps: Vec<KeyMapEntry>,
    /// Zero or one hold layer.
    pub layer: Option<InputDeviceLayer>,
}
