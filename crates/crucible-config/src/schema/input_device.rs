//! The `input-device "<vendor>:<product>" { map …; layer "<KEY>" { map … } }` block.

use crate::error::ConfigError;
use crate::evdev::evdev_code;
use crate::model::{InputDeviceConfig, InputDeviceLayer, KeyMapEntry, KeyMapTarget};
use crate::reader::NodeReader;

/// Parses `"1532:0207"` (hex, case-insensitive) into `(vendor, product)`.
fn parse_usb_id(s: &str) -> Option<(u16, u16)> {
    let (v, p) = s.split_once(':')?;
    let hex = |x: &str| {
        (!x.is_empty() && x.len() <= 4 && x.chars().all(|c| c.is_ascii_hexdigit()))
            .then(|| u16::from_str_radix(x, 16).ok())
            .flatten()
    };
    Some((hex(v)?, hex(p)?))
}

pub(super) fn parse_input_device(node: NodeReader<'_>) -> Result<InputDeviceConfig, ConfigError> {
    let id = node.string_arg("a \"<vendor>:<product>\" string")?;
    let (vendor, product) = parse_usb_id(id).ok_or_else(|| {
        node.invalid(
            "device id",
            format!("`{id}` is not `<vendor>:<product>` in hex (e.g. \"1532:0207\")"),
        )
    })?;
    let mut out = InputDeviceConfig {
        vendor,
        product,
        maps: Vec::new(),
        layer: None,
    };
    for child in node.children_keeping_args_exempt(&["map"], &["map"])? {
        let child = child?;
        match child.name() {
            "map" => push_map(&mut out.maps, child)?,
            "layer" => {
                let name = child.string_arg("a key name")?;
                let key = key_code(&child, name)?;
                let mut maps = Vec::new();
                for inner in child.children_keeping_args_exempt(&["map"], &["map"])? {
                    let inner = inner?;
                    match inner.name() {
                        "map" => push_map(&mut maps, inner)?,
                        _ => return Err(inner.unknown_node("layer")),
                    }
                }
                out.layer = Some(InputDeviceLayer { key, maps });
            }
            _ => return Err(child.unknown_node("input-device")),
        }
    }
    Ok(out)
}

fn key_code(node: &NodeReader<'_>, name: &str) -> Result<u16, ConfigError> {
    evdev_code(name).ok_or_else(|| {
        node.invalid(
            "key name",
            format!("`{name}` is not a known evdev key name (e.g. KEY_1, KEY_LEFTMETA)"),
        )
    })
}

/// `map "<SRC>" "<DST>"` or `map "<SRC>" action="<action>"`; a repeated source is a duplicate.
fn push_map(maps: &mut Vec<KeyMapEntry>, node: NodeReader<'_>) -> Result<(), ConfigError> {
    node.allow_properties(&["action"])?;
    let action = node.string_prop("action", "an action string")?;
    let (source, target) = if let Some((action, _)) = action {
        let src = node.string_arg("a source key name")?;
        node.nonempty("property `action`", action)?;
        (src, KeyMapTarget::Action(action.to_string()))
    } else {
        let a = node.string_args(&["a source key name", "a target key name"])?;
        (a[0], KeyMapTarget::Key(key_code(&node, a[1])?))
    };
    let source_code = key_code(&node, source)?;
    if maps.iter().any(|m| m.source == source_code) {
        return Err(ConfigError::Duplicate {
            at: node.at(),
            name: format!("map \"{source}\""),
        });
    }
    maps.push(KeyMapEntry {
        source: source_code,
        target,
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::super::{err_at, parse};
    use super::parse_usb_id;
    use crate::error::ConfigError;
    use crate::model::{KeyMapEntry, KeyMapTarget};

    #[test]
    fn parses_maps_actions_and_one_layer() {
        let c = parse(
            "input-device \"1532:0207\" {\n  map \"KEY_GRAVE\" \"KEY_1\"\n  map \"KEY_1\" action=\"slot 1\"\n  layer \"KEY_LEFTALT\" {\n    map \"KEY_1\" \"KEY_2\"\n  }\n}\n",
        )
        .unwrap();
        let d = &c.input_devices[0];
        assert_eq!((d.vendor, d.product), (0x1532, 0x0207));
        assert_eq!(
            d.maps,
            vec![
                KeyMapEntry {
                    source: 41,
                    target: KeyMapTarget::Key(2)
                },
                KeyMapEntry {
                    source: 2,
                    target: KeyMapTarget::Action("slot 1".into())
                },
            ]
        );
        let layer = d.layer.as_ref().unwrap();
        assert_eq!(layer.key, 56);
        assert_eq!(layer.maps.len(), 1);
    }

    #[test]
    fn usb_id_is_hex_case_insensitive() {
        assert_eq!(parse_usb_id("1532:0207"), Some((0x1532, 0x0207)));
        assert_eq!(parse_usb_id("abCD:ef01"), Some((0xabcd, 0xef01)));
        for bad in ["1532", "1532:", "zz:01", "12345:1", "1532:0207:1", ""] {
            assert_eq!(parse_usb_id(bad), None, "{bad}");
        }
    }

    #[test]
    fn rejects_bad_id_unknown_key_second_layer_and_duplicates() {
        let (err, at) = err_at("\ninput-device \"razer\" { }\n");
        assert!(matches!(err, ConfigError::InvalidValue { .. }), "{err:?}");
        assert_eq!(at.line, 2);
        let (err, at) = err_at("input-device \"1532:0207\" {\n  map \"KEY_NOPE\" \"KEY_1\"\n}\n");
        assert!(matches!(err, ConfigError::InvalidValue { .. }), "{err:?}");
        assert_eq!(at.line, 2);
        let (err, at) = err_at(
            "input-device \"1532:0207\" {\n  layer \"KEY_A\" { }\n  layer \"KEY_B\" { }\n}\n",
        );
        assert!(matches!(err, ConfigError::Duplicate { .. }), "{err:?}");
        assert_eq!(at.line, 3);
        let (err, _) = err_at(
            "input-device \"1532:0207\" {\n  map \"KEY_A\" \"KEY_1\"\n  map \"KEY_A\" \"KEY_2\"\n}\n",
        );
        assert!(matches!(err, ConfigError::Duplicate { .. }), "{err:?}");
        let (err, _) = err_at("input-device \"1532:0207\" { }\ninput-device \"1532:0207\" { }\n");
        assert!(matches!(err, ConfigError::Duplicate { .. }), "{err:?}");
        let (err, _) = err_at("input-device \"1532:0207\" {\n  bind \"KEY_A\" \"KEY_1\"\n}\n");
        assert!(matches!(err, ConfigError::UnknownNode { .. }), "{err:?}");
        let (err, _) =
            err_at("input-device \"1532:0207\" {\n  map \"KEY_A\" \"KEY_1\" foo=\"x\"\n}\n");
        assert!(
            matches!(err, ConfigError::UnknownProperty { .. }),
            "{err:?}"
        );
    }
}
