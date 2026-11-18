//! The Linux evdev `KEY_*` name table (`linux/input-event-codes.h`) the `input-device` block
//! names keys with. One Smithay-free table: config validation resolves names to codes here, and
//! the remap engine (`crucible-keys`) only ever sees the numeric codes.

/// Every accepted name with its evdev code: letters, digits, F1–F24, modifiers, navigation,
/// editing keys, punctuation, keypad and a few media keys.
pub const EVDEV_KEYS: &[(&str, u16)] = &[
    ("KEY_ESC", 1),
    ("KEY_1", 2),
    ("KEY_2", 3),
    ("KEY_3", 4),
    ("KEY_4", 5),
    ("KEY_5", 6),
    ("KEY_6", 7),
    ("KEY_7", 8),
    ("KEY_8", 9),
    ("KEY_9", 10),
    ("KEY_0", 11),
    ("KEY_MINUS", 12),
    ("KEY_EQUAL", 13),
    ("KEY_BACKSPACE", 14),
    ("KEY_TAB", 15),
    ("KEY_Q", 16),
    ("KEY_W", 17),
    ("KEY_E", 18),
    ("KEY_R", 19),
    ("KEY_T", 20),
    ("KEY_Y", 21),
    ("KEY_U", 22),
    ("KEY_I", 23),
    ("KEY_O", 24),
    ("KEY_P", 25),
    ("KEY_LEFTBRACE", 26),
    ("KEY_RIGHTBRACE", 27),
    ("KEY_ENTER", 28),
    ("KEY_LEFTCTRL", 29),
    ("KEY_A", 30),
    ("KEY_S", 31),
    ("KEY_D", 32),
    ("KEY_F", 33),
    ("KEY_G", 34),
    ("KEY_H", 35),
    ("KEY_J", 36),
    ("KEY_K", 37),
    ("KEY_L", 38),
    ("KEY_SEMICOLON", 39),
    ("KEY_APOSTROPHE", 40),
    ("KEY_GRAVE", 41),
    ("KEY_LEFTSHIFT", 42),
    ("KEY_BACKSLASH", 43),
    ("KEY_Z", 44),
    ("KEY_X", 45),
    ("KEY_C", 46),
    ("KEY_V", 47),
    ("KEY_B", 48),
    ("KEY_N", 49),
    ("KEY_M", 50),
    ("KEY_COMMA", 51),
    ("KEY_DOT", 52),
    ("KEY_SLASH", 53),
    ("KEY_RIGHTSHIFT", 54),
    ("KEY_KPASTERISK", 55),
    ("KEY_LEFTALT", 56),
    ("KEY_SPACE", 57),
    ("KEY_CAPSLOCK", 58),
    ("KEY_F1", 59),
    ("KEY_F2", 60),
    ("KEY_F3", 61),
    ("KEY_F4", 62),
    ("KEY_F5", 63),
    ("KEY_F6", 64),
    ("KEY_F7", 65),
    ("KEY_F8", 66),
    ("KEY_F9", 67),
    ("KEY_F10", 68),
    ("KEY_NUMLOCK", 69),
    ("KEY_SCROLLLOCK", 70),
    ("KEY_KP7", 71),
    ("KEY_KP8", 72),
    ("KEY_KP9", 73),
    ("KEY_KPMINUS", 74),
    ("KEY_KP4", 75),
    ("KEY_KP5", 76),
    ("KEY_KP6", 77),
    ("KEY_KPPLUS", 78),
    ("KEY_KP1", 79),
    ("KEY_KP2", 80),
    ("KEY_KP3", 81),
    ("KEY_KP0", 82),
    ("KEY_KPDOT", 83),
    ("KEY_102ND", 86),
    ("KEY_F11", 87),
    ("KEY_F12", 88),
    ("KEY_KPENTER", 96),
    ("KEY_RIGHTCTRL", 97),
    ("KEY_KPSLASH", 98),
    ("KEY_SYSRQ", 99),
    ("KEY_RIGHTALT", 100),
    ("KEY_HOME", 102),
    ("KEY_UP", 103),
    ("KEY_PAGEUP", 104),
    ("KEY_LEFT", 105),
    ("KEY_RIGHT", 106),
    ("KEY_END", 107),
    ("KEY_DOWN", 108),
    ("KEY_PAGEDOWN", 109),
    ("KEY_INSERT", 110),
    ("KEY_DELETE", 111),
    ("KEY_MUTE", 113),
    ("KEY_VOLUMEDOWN", 114),
    ("KEY_VOLUMEUP", 115),
    ("KEY_KPEQUAL", 117),
    ("KEY_PAUSE", 119),
    ("KEY_KPCOMMA", 121),
    ("KEY_LEFTMETA", 125),
    ("KEY_RIGHTMETA", 126),
    ("KEY_COMPOSE", 127),
    ("KEY_MENU", 139),
    ("KEY_NEXTSONG", 163),
    ("KEY_PLAYPAUSE", 164),
    ("KEY_PREVIOUSSONG", 165),
    ("KEY_STOPCD", 166),
    ("KEY_F13", 183),
    ("KEY_F14", 184),
    ("KEY_F15", 185),
    ("KEY_F16", 186),
    ("KEY_F17", 187),
    ("KEY_F18", 188),
    ("KEY_F19", 189),
    ("KEY_F20", 190),
    ("KEY_F21", 191),
    ("KEY_F22", 192),
    ("KEY_F23", 193),
    ("KEY_F24", 194),
    ("KEY_PRINT", 210),
];

/// The evdev code for `name` (exact `KEY_*` spelling, case-insensitive), or `None`.
#[must_use]
pub fn evdev_code(name: &str) -> Option<u16> {
    EVDEV_KEYS
        .iter()
        .find_map(|(n, code)| n.eq_ignore_ascii_case(name).then_some(*code))
}

/// The `KEY_*` name for `code`, or `None` when the table does not list it.
#[must_use]
pub fn evdev_name(code: u16) -> Option<&'static str> {
    EVDEV_KEYS
        .iter()
        .find_map(|(n, c)| (*c == code).then_some(*n))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn table_round_trips_and_is_unique() {
        for (name, code) in EVDEV_KEYS {
            assert_eq!(evdev_code(name), Some(*code), "{name}");
            assert_eq!(evdev_name(*code), Some(*name), "{code}");
        }
        assert_eq!(evdev_code("key_grave"), Some(41));
        assert_eq!(evdev_code("KEY_NOPE"), None);
        assert_eq!(evdev_code("GRAVE"), None);
    }
}
