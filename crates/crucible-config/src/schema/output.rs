//! The `output "<connector>" { scale … }` block.

use crate::error::ConfigError;
use crate::model::{OUTPUT_SCALE_MAX, OUTPUT_SCALE_MIN, OUTPUT_SCALE_STEPS, OutputConfig};
use crate::reader::NodeReader;

pub(super) fn parse_output(node: NodeReader<'_>) -> Result<(String, OutputConfig), ConfigError> {
    node.no_properties()?;
    let name = node.nonempty_string_arg("a connector name string", "connector name")?;
    let mut out = OutputConfig::default();
    for child in node.children_keeping_args(&[])? {
        let child = child?;
        match child.name() {
            "scale" => {
                let n = child.float_arg()?;
                if !(OUTPUT_SCALE_MIN..=OUTPUT_SCALE_MAX).contains(&n) {
                    return Err(child.invalid(
                        "argument",
                        format!("{n} is outside {OUTPUT_SCALE_MIN}..={OUTPUT_SCALE_MAX}"),
                    ));
                }
                let steps = (n * f64::from(OUTPUT_SCALE_STEPS)).round();
                #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                {
                    out.scale_120 = steps as u32;
                }
            }
            _ => return Err(child.unknown_node("output")),
        }
    }
    Ok((name, out))
}

#[cfg(test)]
mod tests {
    use super::super::{err_at, parse};
    use crate::error::ConfigError;
    use crate::model::Config;

    #[test]
    fn output_scale_parses_float_and_integer_and_quantizes() {
        let c =
            parse("output \"DP-1\" { scale 1.4167; }\noutput \"HDMI-A-1\" { scale 2; }\n").unwrap();
        assert_eq!(c.outputs["DP-1"].scale_120, 170);
        assert_eq!(c.outputs["HDMI-A-1"].scale_120, 240);
        assert!((c.output_scale("DP-1") - 170.0 / 120.0).abs() < 1e-12);
        assert!((c.output_scale("HDMI-A-1") - 2.0).abs() < 1e-12);
        assert!((c.max_output_scale() - 2.0).abs() < 1e-12);
    }

    #[test]
    fn output_scale_unlisted_or_unspecified_is_one() {
        let c = parse("output \"DP-1\" { }\n").unwrap();
        assert_eq!(c.outputs["DP-1"].scale_120, 120);
        assert!((c.output_scale("DP-2") - 1.0).abs() < 1e-12);
        assert!((Config::default().max_output_scale() - 1.0).abs() < 1e-12);
    }

    #[test]
    fn output_scale_range_is_inclusive_and_enforced() {
        assert_eq!(
            parse("output \"A\" { scale 0.5; }\n").unwrap().outputs["A"].scale_120,
            60
        );
        assert_eq!(
            parse("output \"A\" { scale 4; }\n").unwrap().outputs["A"].scale_120,
            480
        );
        for bad in ["0.49", "4.01", "0", "-1", "#nan"] {
            let (err, at) = err_at(&format!("output \"A\" {{ scale {bad}; }}\n"));
            assert!(matches!(err, ConfigError::InvalidValue { .. }), "{bad}");
            assert_eq!(at.line, 1);
        }
    }

    #[test]
    fn output_errors_duplicate_unknown_child_missing_name_wrong_type() {
        let (err, at) = err_at("output \"A\" { scale 1; }\noutput \"A\" { scale 2; }\n");
        assert!(matches!(err, ConfigError::Duplicate { .. }));
        assert_eq!(at.line, 2);
        let (err, _) = err_at("output \"A\" { mode 1; }\n");
        assert!(matches!(
            err,
            ConfigError::UnknownNode {
                context: "output",
                ..
            }
        ));
        let (err, _) = err_at("output { scale 1; }\n");
        assert!(matches!(err, ConfigError::MissingArgument { .. }));
        let (err, _) = err_at("output \"A\" { scale \"big\"; }\n");
        assert!(matches!(err, ConfigError::WrongType { .. }));
    }
}
