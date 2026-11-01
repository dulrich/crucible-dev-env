//! The one field codec CDE uses wherever a name, path or reason must survive a line/`:`/TAB
//! framed channel: the `CDE_SOCKET` IPC protocol (re-exported as `crucible_compositor::ipc::
//! {encode_field, decode_field}`) and the `crucible pick --rename|--root <value>` argument
//! ([`crate::invocation::PickTarget`]).

/// Percent-encodes `%`, `:`, TAB, LF and CR in `field`; every other character passes through.
#[must_use]
pub fn encode_field(field: &str) -> String {
    let mut out = String::with_capacity(field.len());
    for c in field.chars() {
        match c {
            '%' => out.push_str("%25"),
            ':' => out.push_str("%3A"),
            '\t' => out.push_str("%09"),
            '\n' => out.push_str("%0A"),
            '\r' => out.push_str("%0D"),
            _ => out.push(c),
        }
    }
    out
}

/// Inverse of [`encode_field`]. Returns `None` for a `%` not followed by one of the five escapes
/// (`25`, `3A`, `09`, `0A`, `0D`; hex digits case-insensitive).
#[must_use]
pub fn decode_field(field: &str) -> Option<String> {
    let mut out = String::with_capacity(field.len());
    let mut rest = field;
    while let Some(i) = rest.find('%') {
        out.push_str(&rest[..i]);
        let esc = rest.get(i + 1..i + 3)?;
        out.push(match esc.to_ascii_uppercase().as_str() {
            "25" => '%',
            "3A" => ':',
            "09" => '\t',
            "0A" => '\n',
            "0D" => '\r',
            _ => return None,
        });
        rest = &rest[i + 3..];
    }
    out.push_str(rest);
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codec_round_trips_every_special_character() {
        for s in [
            "a:b",
            "tab\there",
            "l\nf",
            "c\rr",
            "100%",
            "%3A literal",
            "plain ü",
            "",
        ] {
            let enc = encode_field(s);
            assert!(!enc.contains([':', '\t', '\n', '\r']), "{enc:?}");
            assert_eq!(decode_field(&enc).as_deref(), Some(s));
        }
        assert_eq!(encode_field("a%:\t\n\r"), "a%25%3A%09%0A%0D");
    }

    #[test]
    fn decode_rejects_unknown_or_truncated_escapes() {
        assert_eq!(decode_field("%41"), None);
        assert_eq!(decode_field("%2"), None);
        assert_eq!(decode_field("x%"), None);
        assert_eq!(decode_field("%3a").as_deref(), Some(":"));
    }
}
