//! Terminal-side helpers shared by every overlay: key decoding and the text-fitting helpers
//! (control-character neutralising, truncation, scrolling window).

/// A decoded key press.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Key {
    /// A printable character.
    Char(char),
    /// Enter / Return.
    Enter,
    /// Alt+Enter (ESC then CR or LF).
    AltEnter,
    /// A lone Escape.
    Esc,
    /// Backspace.
    Backspace,
    /// Arrow up.
    Up,
    /// Arrow down.
    Down,
    /// Delete (`ESC [ 3 ~`).
    Delete,
    /// Ctrl+C.
    CtrlC,
    /// Ctrl+D.
    CtrlD,
    /// Ctrl+N.
    CtrlN,
    /// Ctrl+P.
    CtrlP,
}

/// Decodes as many keys as possible from the front of `buf`, draining what it consumes. An
/// incomplete trailing escape sequence or UTF-8 character stays in the buffer for the next read.
pub fn decode_keys(buf: &mut Vec<u8>) -> Vec<Key> {
    let mut keys = Vec::new();
    while let Some(&b) = buf.first() {
        let consumed = match b {
            0x1b => match buf.get(1) {
                None => {
                    keys.push(Key::Esc);
                    1
                }
                Some(b'[') => {
                    // CSI: parameters, then a final byte 0x40..=0x7e.
                    let Some(end) = buf[2..].iter().position(|c| (0x40..=0x7e).contains(c)) else {
                        break;
                    };
                    let end = end + 2;
                    match (end, buf[end]) {
                        (2, b'A') => keys.push(Key::Up),
                        (2, b'B') => keys.push(Key::Down),
                        (3, b'~') if buf[2] == b'3' => keys.push(Key::Delete),
                        _ => {}
                    }
                    end + 1
                }
                Some(b'\r' | b'\n') => {
                    keys.push(Key::AltEnter);
                    2
                }
                Some(b'O') => {
                    let Some(&f) = buf.get(2) else { break };
                    match f {
                        b'A' => keys.push(Key::Up),
                        b'B' => keys.push(Key::Down),
                        _ => {}
                    }
                    3
                }
                Some(_) => {
                    keys.push(Key::Esc);
                    1
                }
            },
            b'\r' | b'\n' => {
                keys.push(Key::Enter);
                1
            }
            0x03 => {
                keys.push(Key::CtrlC);
                1
            }
            0x04 => {
                keys.push(Key::CtrlD);
                1
            }
            0x0e => {
                keys.push(Key::CtrlN);
                1
            }
            0x10 => {
                keys.push(Key::CtrlP);
                1
            }
            0x08 | 0x7f => {
                keys.push(Key::Backspace);
                1
            }
            0x00..=0x1f => 1,
            _ => {
                let need = match b {
                    0x20..=0x7f => 1,
                    0xc0..=0xdf => 2,
                    0xe0..=0xef => 3,
                    0xf0..=0xf7 => 4,
                    _ => {
                        buf.remove(0);
                        continue;
                    }
                };
                if buf.len() < need {
                    break;
                }
                match std::str::from_utf8(&buf[..need])
                    .ok()
                    .and_then(|s| s.chars().next())
                {
                    Some(c) => {
                        keys.push(Key::Char(c));
                        need
                    }
                    None => 1,
                }
            }
        };
        buf.drain(..consumed);
    }
    keys
}

/// Replaces control characters so a name can never inject terminal escapes.
pub(crate) fn sanitize(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_control() { '?' } else { c })
        .collect()
}

/// Truncates to `width` characters.
pub(crate) fn fit(s: &str, width: usize) -> String {
    s.chars().take(width).collect()
}

/// The `[start, end)` slice of `total` rows to show in `height` lines, keeping `selected` visible.
#[must_use]
pub fn window(selected: usize, total: usize, height: usize) -> (usize, usize) {
    if height == 0 || total <= height {
        return (0, total);
    }
    let start = selected.saturating_sub(height - 1).min(total - height);
    (start, start + height)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keys(bytes: &[u8]) -> Vec<Key> {
        decode_keys(&mut bytes.to_vec())
    }

    #[test]
    fn decodes_basic_keys() {
        assert_eq!(
            keys(b"a\x7f\r\x03\x04\x0e\x10"),
            vec![
                Key::Char('a'),
                Key::Backspace,
                Key::Enter,
                Key::CtrlC,
                Key::CtrlD,
                Key::CtrlN,
                Key::CtrlP
            ]
        );
    }

    #[test]
    fn decodes_alt_enter_and_keeps_lone_esc() {
        assert_eq!(keys(b"\x1b\r"), vec![Key::AltEnter]);
        assert_eq!(keys(b"\x1b\n"), vec![Key::AltEnter]);
        assert_eq!(keys(b"\x1b"), vec![Key::Esc]);
        assert_eq!(keys(b"\x1bx"), vec![Key::Esc, Key::Char('x')]);
    }

    #[test]
    fn decodes_arrows_and_esc() {
        assert_eq!(keys(b"\x1b[A\x1b[B"), vec![Key::Up, Key::Down]);
        assert_eq!(keys(b"\x1bOA\x1bOB"), vec![Key::Up, Key::Down]);
        assert_eq!(keys(b"\x1b"), vec![Key::Esc]);
        assert_eq!(keys(b"\x1b[1;5C"), vec![]);
    }

    #[test]
    fn decodes_delete_and_ignores_other_tilde_keys() {
        assert_eq!(keys(b"\x1b[3~"), vec![Key::Delete]);
        assert_eq!(keys(b"\x1b[3~\x1b[A"), vec![Key::Delete, Key::Up]);
        // Insert / PageUp / Home are still swallowed, and a partial Delete waits for its `~`.
        assert_eq!(keys(b"\x1b[2~\x1b[5~\x1b[1~\x1b[3;5~"), vec![]);
        assert_eq!(keys(b"\x1b[3~x"), vec![Key::Delete, Key::Char('x')]);
        let mut partial = b"\x1b[3".to_vec();
        assert!(decode_keys(&mut partial).is_empty());
        assert_eq!(partial.len(), 3);
        assert_eq!(keys(b"\x1b"), vec![Key::Esc]);
    }

    #[test]
    fn decodes_utf8_and_keeps_partial_input() {
        assert_eq!(keys("é".as_bytes()), vec![Key::Char('é')]);
        let mut buf = vec![0xc3];
        assert!(decode_keys(&mut buf).is_empty());
        assert_eq!(buf, vec![0xc3]);
        buf.push(0xa9);
        assert_eq!(decode_keys(&mut buf), vec![Key::Char('é')]);
        assert!(buf.is_empty());
        let mut csi = b"\x1b[".to_vec();
        assert!(decode_keys(&mut csi).is_empty());
        assert_eq!(csi.len(), 2);
    }

    #[test]
    fn invalid_utf8_does_not_wedge() {
        let mut buf = vec![0xff, b'a'];
        assert_eq!(decode_keys(&mut buf), vec![Key::Char('a')]);
        assert!(buf.is_empty());
    }

    #[test]
    fn window_keeps_selection_visible() {
        assert_eq!(window(0, 3, 10), (0, 3));
        assert_eq!(window(0, 20, 5), (0, 5));
        assert_eq!(window(4, 20, 5), (0, 5));
        assert_eq!(window(5, 20, 5), (1, 6));
        assert_eq!(window(19, 20, 5), (15, 20));
    }
}
