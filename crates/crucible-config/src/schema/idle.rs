//! The `idle { lock-after 300; blank-after 600 }` block.

use crate::error::ConfigError;
use crate::model::IdleConfig;
use crate::reader::NodeReader;

pub(super) fn parse_idle(node: NodeReader<'_>) -> Result<IdleConfig, ConfigError> {
    let mut out = IdleConfig::default();
    for child in node.children(&[])? {
        let child = child?;
        child.no_properties()?;
        let slot = match child.name() {
            "lock-after" => &mut out.lock_after,
            "blank-after" => &mut out.blank_after,
            _ => return Err(child.unknown_node("idle")),
        };
        // 1..=u32::MAX: zero, negatives and non-integers are errors, never coerced.
        *slot = Some(child.u32_arg_in(1..=u32::MAX)?);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::super::{err_at, parse};
    use crate::error::ConfigError;
    use crate::model::{Config, IdleConfig};

    #[test]
    fn idle_defaults_to_both_timers_off() {
        assert_eq!(Config::default().idle, IdleConfig::default());
        assert_eq!(IdleConfig::default().lock_after, None);
        assert_eq!(IdleConfig::default().blank_after, None);
        assert_eq!(parse("idle { }\n").unwrap().idle, IdleConfig::default());
    }

    #[test]
    fn idle_parses_each_timer_independently() {
        let c = parse("idle { lock-after 300; blank-after 600; }\n").unwrap();
        assert_eq!(c.idle.lock_after, Some(300));
        assert_eq!(c.idle.blank_after, Some(600));
        let c = parse("idle { blank-after 30; }\n").unwrap();
        assert_eq!(c.idle.lock_after, None);
        assert_eq!(c.idle.blank_after, Some(30));
    }

    #[test]
    fn idle_rejects_zero_negative_and_non_integer() {
        for bad in ["0", "-5", "1.5", "\"300\"", "#true"] {
            let (err, at) = err_at(&format!("\nidle {{ lock-after {bad}; }}\n"));
            assert!(
                matches!(
                    err,
                    ConfigError::InvalidValue { .. } | ConfigError::WrongType { .. }
                ),
                "{bad}: {err:?}"
            );
            assert_eq!(at.line, 2, "{bad}");
        }
        let (err, _) = err_at("idle { blank-after; }\n");
        assert!(matches!(err, ConfigError::MissingArgument { .. }));
    }

    #[test]
    fn idle_rejects_unknown_child_and_repeats() {
        let (err, _) = err_at("idle { sleep-after 5; }\n");
        assert!(matches!(err, ConfigError::UnknownNode { .. }), "{err:?}");
        let (err, _) = err_at("idle { lock-after 5; lock-after 6; }\n");
        assert!(matches!(err, ConfigError::Duplicate { .. }), "{err:?}");
    }
}
