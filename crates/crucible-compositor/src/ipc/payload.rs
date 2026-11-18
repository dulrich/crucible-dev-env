//! The payload codecs carried inside `ok` replies: candidates, bindings and outputs.

use std::path::PathBuf;

use super::{decode_field, encode_field};

/// The kind of a picker candidate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CandidateKind {
    /// A workspace live in this compositor.
    Live,
    /// A saved workspace that is not live.
    Saved,
    /// A folder under a configured picker root.
    Folder,
}

impl CandidateKind {
    fn as_str(self) -> &'static str {
        match self {
            CandidateKind::Live => "live",
            CandidateKind::Saved => "saved",
            CandidateKind::Folder => "folder",
        }
    }
}

/// One `candidates` item: `kind:name[:path]` with `name`/`path` encoded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    /// Live, saved or folder.
    pub kind: CandidateKind,
    /// Workspace name (folders: the folder's basename).
    pub name: String,
    /// Folder path, or a live workspace's root; `None` otherwise.
    pub path: Option<PathBuf>,
    /// The `Super+<n>` slot the workspace is pinned to (0-9), when the compositor chose to send
    /// slots (`picker { slots #true }`); always `None` for folders and when the setting is off.
    pub slot: Option<u8>,
}

/// Formats candidates as the `ok` payload: TAB-separated `kind[#slot]:name[:path]` items. The
/// optional `#<digit>` rides on the kind token, which never contains encoded content, so it
/// cannot clash with `:` (escaped by [`encode_field`]) in names or paths.
#[must_use]
pub fn format_candidates(items: &[Candidate]) -> String {
    items
        .iter()
        .map(|c| {
            let mut s = c.kind.as_str().to_string();
            if let Some(n) = c.slot {
                s.push_str(&format!("#{n}"));
            }
            s.push(':');
            s.push_str(&encode_field(&c.name));
            if let Some(p) = &c.path {
                s.push(':');
                s.push_str(&encode_field(&p.to_string_lossy()));
            }
            s
        })
        .collect::<Vec<_>>()
        .join("\t")
}

/// Parses a `candidates` payload back into items; `None` on any malformed item.
#[must_use]
pub fn parse_candidates(payload: &str) -> Option<Vec<Candidate>> {
    if payload.is_empty() {
        return Some(Vec::new());
    }
    payload
        .split('\t')
        .map(|item| {
            let mut parts = item.split(':');
            let head = parts.next()?;
            let (head, slot) = match head.split_once('#') {
                Some((k, n)) => {
                    let mut digits = n.chars();
                    let d = digits.next()?.to_digit(10)?;
                    if digits.next().is_some() {
                        return None;
                    }
                    (k, Some(u8::try_from(d).ok()?))
                }
                None => (head, None),
            };
            let kind = match head {
                "live" => CandidateKind::Live,
                "saved" => CandidateKind::Saved,
                "folder" => CandidateKind::Folder,
                _ => return None,
            };
            let name = decode_field(parts.next()?)?;
            let path = match parts.next() {
                Some(p) => Some(PathBuf::from(decode_field(p)?)),
                None => None,
            };
            if parts.next().is_some() {
                return None;
            }
            Some(Candidate {
                kind,
                name,
                path,
                slot,
            })
        })
        .collect()
}

/// Formats `(chord, action)` pairs as the `bindings` payload: TAB-separated
/// `encode_field(chord):encode_field(action)` items.
#[must_use]
pub fn format_bindings(entries: &[(String, String)]) -> String {
    entries
        .iter()
        .map(|(c, a)| format!("{}:{}", encode_field(c), encode_field(a)))
        .collect::<Vec<_>>()
        .join("\t")
}

/// Parses a `bindings` payload back into pairs; `None` on any malformed item.
#[must_use]
pub fn parse_bindings(payload: &str) -> Option<Vec<(String, String)>> {
    if payload.is_empty() {
        return Some(Vec::new());
    }
    payload
        .split('\t')
        .map(|item| {
            let (chord, action) = item.split_once(':')?;
            Some((decode_field(chord)?, decode_field(action)?))
        })
        .collect()
}

/// One `outputs` record: an enabled output's connector name, logical position and size, and scale.
#[derive(Debug, Clone, PartialEq)]
pub struct OutputInfo {
    /// Connector name (never contains whitespace or `=`).
    pub name: String,
    /// Logical x.
    pub x: i32,
    /// Logical y.
    pub y: i32,
    /// Logical width.
    pub w: i32,
    /// Logical height.
    pub h: i32,
    /// Fractional scale.
    pub scale: f64,
}

/// Formats outputs as the single-line `ok` payload: space-separated `name=x,y,wxh@scale` records,
/// ordered by x, then y, then name.
#[must_use]
pub fn format_outputs(items: &[OutputInfo]) -> String {
    let mut sorted: Vec<&OutputInfo> = items.iter().collect();
    sorted.sort_by(|a, b| (a.x, a.y, &a.name).cmp(&(b.x, b.y, &b.name)));
    sorted
        .iter()
        .map(|o| format!("{}={},{},{}x{}@{}", o.name, o.x, o.y, o.w, o.h, o.scale))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Parses an `outputs` payload back into records; `None` on any malformed record.
#[must_use]
pub fn parse_outputs(payload: &str) -> Option<Vec<OutputInfo>> {
    payload
        .split_ascii_whitespace()
        .map(|rec| {
            let (name, rest) = rec.split_once('=')?;
            let (pos, rest) = rest.split_once(',')?;
            let (y, rest) = rest.split_once(',')?;
            let (w, rest) = rest.split_once('x')?;
            let (h, scale) = rest.split_once('@')?;
            if name.is_empty() {
                return None;
            }
            Some(OutputInfo {
                name: name.to_string(),
                x: pos.parse().ok()?,
                y: y.parse().ok()?,
                w: w.parse().ok()?,
                h: h.parse().ok()?,
                scale: scale.parse().ok()?,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ipc::Response;

    #[test]
    fn outputs_reply_is_one_sorted_line_and_round_trips() {
        let o = |name: &str, x, scale| OutputInfo {
            name: name.into(),
            x,
            y: 0,
            w: 1920,
            h: 1080,
            scale,
        };
        let payload = format_outputs(&[o("HDMI-A-1", 1920, 1.5), o("DP-1", 0, 1.0)]);
        assert_eq!(
            payload,
            "DP-1=0,0,1920x1080@1 HDMI-A-1=1920,0,1920x1080@1.5"
        );
        assert!(!payload.contains('\n'));
        assert_eq!(
            parse_outputs(&payload),
            Some(vec![o("DP-1", 0, 1.0), o("HDMI-A-1", 1920, 1.5)])
        );
        assert_eq!(format_outputs(&[]), "");
        assert_eq!(Response::Ok(payload).to_line().lines().count(), 1);
        assert_eq!(parse_outputs("DP-1=bad"), None);
    }

    #[test]
    fn bindings_round_trip_with_encoded_fields() {
        let entries = vec![
            ("Super+comma".to_string(), "open-settings".to_string()),
            ("Super+F2".to_string(), "run echo 50%:a\tb".to_string()),
        ];
        let payload = format_bindings(&entries);
        assert_eq!(payload.split('\t').count(), 2);
        assert_eq!(parse_bindings(&payload), Some(entries));
        assert_eq!(parse_bindings(""), Some(vec![]));
        assert_eq!(format_bindings(&[]), "");
        assert_eq!(parse_bindings("nocolon"), None);
        assert_eq!(parse_bindings("a:%ZZ"), None);
    }

    #[test]
    fn candidates_round_trip_with_encoded_fields() {
        let items = vec![
            Candidate {
                kind: CandidateKind::Live,
                name: "a:b".into(),
                path: Some(PathBuf::from("/r\toot")),
                slot: None,
            },
            Candidate {
                kind: CandidateKind::Saved,
                name: "100%".into(),
                path: None,
                slot: None,
            },
            Candidate {
                kind: CandidateKind::Folder,
                name: "x\ny".into(),
                path: Some(PathBuf::from("/p:q/x\ny")),
                slot: None,
            },
        ];
        let payload = format_candidates(&items);
        assert_eq!(payload.split('\t').count(), 3);
        assert!(payload.starts_with("live:a%3Ab:/r%09oot\tsaved:100%25\tfolder:"));
        assert_eq!(parse_candidates(&payload), Some(items));
        assert_eq!(parse_candidates(""), Some(vec![]));
        assert_eq!(parse_candidates("weird:x"), None);
    }

    #[test]
    fn candidates_round_trip_slots_and_accept_legacy_payloads() {
        let items = vec![
            Candidate {
                kind: CandidateKind::Live,
                name: "a:b#3".into(),
                path: Some(PathBuf::from("/r:s")),
                slot: Some(3),
            },
            Candidate {
                kind: CandidateKind::Saved,
                name: "z".into(),
                path: None,
                slot: Some(0),
            },
        ];
        let payload = format_candidates(&items);
        assert_eq!(payload, "live#3:a%3Ab#3:/r%3As\tsaved#0:z");
        assert_eq!(parse_candidates(&payload), Some(items));
        let legacy = parse_candidates("live:a:/r\tsaved:b").unwrap();
        assert!(legacy.iter().all(|c| c.slot.is_none()));
        assert_eq!(legacy.len(), 2);
        assert_eq!(parse_candidates("live#:a"), None);
        assert_eq!(parse_candidates("live#12:a"), None);
        assert_eq!(parse_candidates("live#x:a"), None);
    }
}
