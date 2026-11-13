//! The `night-light { temperature 4500; day-temperature 6500; start "21:00"; end "07:00"; fade-minutes 30 }` block.

use crate::error::ConfigError;
use crate::model::NightLightConfig;
use crate::reader::NodeReader;

const DEFAULT_TEMPERATURE: u32 = 4500;
const DEFAULT_DAY_TEMPERATURE: u32 = 6500;
const DEFAULT_FADE_MINUTES: u32 = 30;

/// `"HH:MM"` to minutes after midnight: exactly two digits each, hour 00-23, minute 00-59.
fn parse_hhmm(s: &str) -> Option<u16> {
    let b = s.as_bytes();
    if b.len() != 5 || b[2] != b':' {
        return None;
    }
    let digits = |d: &[u8]| -> Option<u16> {
        if d.iter().all(u8::is_ascii_digit) {
            Some(u16::from(d[0] - b'0') * 10 + u16::from(d[1] - b'0'))
        } else {
            None
        }
    };
    let (h, m) = (digits(&b[..2])?, digits(&b[3..])?);
    (h < 24 && m < 60).then_some(h * 60 + m)
}

fn time_arg(child: &NodeReader<'_>) -> Result<u16, ConfigError> {
    let s = child.string_arg("a \"HH:MM\" time string")?;
    parse_hhmm(s).ok_or_else(|| {
        child.invalid(
            "argument",
            format!("\"{s}\" is not a time of day written \"HH:MM\" (00:00 to 23:59)"),
        )
    })
}

pub(super) fn parse_night_light(node: NodeReader<'_>) -> Result<NightLightConfig, ConfigError> {
    let mut temperature = DEFAULT_TEMPERATURE;
    let mut day = None;
    let mut fade = DEFAULT_FADE_MINUTES;
    let (mut start, mut end) = (None, None);
    for child in node.children_strict(&[], &[])? {
        let child = child?;
        match child.name() {
            "temperature" => temperature = child.u32_arg_in(2500..=6500)?,
            "day-temperature" => day = Some((child.u32_arg_in(2500..=6500)?, child.at())),
            "fade-minutes" => fade = child.u32_arg_in(0..=180)?,
            "start" => start = Some((time_arg(&child)?, child.at())),
            "end" => end = Some((time_arg(&child)?, child.at())),
            _ => return Err(child.unknown_node("night-light")),
        }
    }
    let Some((start, _)) = start else {
        return Err(node.missing("a `start` child"));
    };
    let Some((end, _)) = end else {
        return Err(node.missing("an `end` child"));
    };
    let day_temperature = match day {
        Some((k, at)) if k < temperature => {
            return Err(node.invalid_at(
                at,
                "argument",
                format!("day-temperature {k} is warmer than the night temperature {temperature}; it must be {temperature} or higher"),
            ));
        }
        Some((k, _)) => k,
        None => DEFAULT_DAY_TEMPERATURE,
    };
    Ok(NightLightConfig {
        temperature,
        day_temperature,
        start,
        end,
        fade_minutes: u16::try_from(fade).expect("range-checked to 0..=180"),
    })
}

#[cfg(test)]
mod tests {
    use super::super::{err_at, parse};
    use crate::error::ConfigError;
    use crate::model::{Config, NightLightConfig};

    #[test]
    fn absent_block_is_none() {
        assert_eq!(Config::default().night_light, None);
        assert_eq!(parse("").unwrap().night_light, None);
    }

    #[test]
    fn minimal_block_takes_defaults_and_minutes_of_day() {
        let c = parse("night-light { start \"21:00\"; end \"07:05\"; }\n").unwrap();
        assert_eq!(
            c.night_light,
            Some(NightLightConfig {
                temperature: 4500,
                day_temperature: 6500,
                start: 21 * 60,
                end: 7 * 60 + 5,
                fade_minutes: 30
            })
        );
    }

    #[test]
    fn full_block_and_range_edges_parse() {
        let c = parse(
            "night-light { temperature 2500; start \"00:00\"; end \"23:59\"; fade-minutes 180; }\n",
        )
        .unwrap();
        let n = c.night_light.unwrap();
        assert_eq!(
            (n.temperature, n.start, n.end, n.fade_minutes),
            (2500, 0, 1439, 180)
        );
        let c = parse(
            "night-light { temperature 6500; start \"01:00\"; end \"02:00\"; fade-minutes 0; }\n",
        )
        .unwrap();
        assert_eq!(c.night_light.unwrap().fade_minutes, 0);
    }

    #[test]
    fn day_temperature_parses_and_may_equal_the_night_value() {
        let n = parse(
            "night-light { temperature 3500; day-temperature 5500; start \"21:00\"; end \"07:00\"; }\n",
        )
        .unwrap()
        .night_light
        .unwrap();
        assert_eq!((n.temperature, n.day_temperature), (3500, 5500));
        let n = parse("night-light { day-temperature 4500; start \"21:00\"; end \"07:00\"; }\n")
            .unwrap()
            .night_light
            .unwrap();
        assert_eq!((n.temperature, n.day_temperature), (4500, 4500));
    }

    #[test]
    fn day_temperature_warmer_than_night_is_a_located_error() {
        let (err, at) = err_at(
            "\nnight-light {\n temperature 5000\n day-temperature 4000\n start \"21:00\"\n end \"07:00\"\n}\n",
        );
        assert!(matches!(err, ConfigError::InvalidValue { .. }), "{err:?}");
        assert_eq!(at.line, 4);
        // Order does not matter: the check runs after every child is read.
        let (err, at) = err_at(
            "\nnight-light {\n day-temperature 4000\n temperature 5000\n start \"21:00\"\n end \"07:00\"\n}\n",
        );
        assert!(matches!(err, ConfigError::InvalidValue { .. }), "{err:?}");
        assert_eq!(at.line, 3);
    }

    #[test]
    fn out_of_range_numbers_are_located_errors() {
        for bad in [
            "temperature 2499",
            "temperature 6501",
            "temperature -1",
            "temperature 4500.5",
            "temperature \"4500\"",
            "day-temperature 2499",
            "day-temperature 6501",
            "fade-minutes 181",
            "fade-minutes -1",
        ] {
            let (err, at) = err_at(&format!(
                "\nnight-light {{\n {bad}\n start \"21:00\"\n end \"07:00\"\n}}\n"
            ));
            assert!(
                matches!(
                    err,
                    ConfigError::InvalidValue { .. } | ConfigError::WrongType { .. }
                ),
                "{bad}: {err:?}"
            );
            assert_eq!(at.line, 3, "{bad}");
        }
    }

    #[test]
    fn malformed_times_are_rejected() {
        for bad in [
            "\"24:00\"",
            "\"12:60\"",
            "\"9:00\"",
            "\"09:0\"",
            "\"0900\"",
            "\"09:00:00\"",
            "\"-1:00\"",
            "\"+9:00\"",
            "\"ab:cd\"",
            "\"\"",
            "900",
            "#true",
        ] {
            let (err, at) = err_at(&format!(
                "\nnight-light {{\n start {bad}\n end \"07:00\"\n}}\n"
            ));
            assert!(
                matches!(
                    err,
                    ConfigError::InvalidValue { .. } | ConfigError::WrongType { .. }
                ),
                "{bad}: {err:?}"
            );
            assert_eq!(at.line, 3, "{bad}");
        }
    }

    #[test]
    fn start_and_end_are_required_and_equal_means_all_day() {
        let (err, _) = err_at("night-light { end \"07:00\"; }\n");
        assert!(
            matches!(err, ConfigError::MissingArgument { .. }),
            "{err:?}"
        );
        let (err, _) = err_at("night-light { start \"07:00\"; }\n");
        assert!(
            matches!(err, ConfigError::MissingArgument { .. }),
            "{err:?}"
        );
        let (err, _) = err_at("night-light { }\n");
        assert!(
            matches!(err, ConfigError::MissingArgument { .. }),
            "{err:?}"
        );
        let n = parse("night-light {\n start \"07:00\"\n end \"07:00\"\n}\n")
            .unwrap()
            .night_light
            .unwrap();
        assert_eq!((n.start, n.end), (7 * 60, 7 * 60));
    }

    #[test]
    fn unknown_children_and_repeats_are_errors() {
        let (err, _) = err_at("night-light { sunrise \"07:00\"; }\n");
        assert!(matches!(err, ConfigError::UnknownNode { .. }), "{err:?}");
        let (err, _) = err_at("night-light { temperature 3000; temperature 3500; }\n");
        assert!(matches!(err, ConfigError::Duplicate { .. }), "{err:?}");
        let (err, _) = err_at(
            "night-light { start \"1:00\"; end \"2:00\"; }\nnight-light { start \"01:00\"; end \"02:00\"; }\n",
        );
        assert!(matches!(err, ConfigError::InvalidValue { .. }), "{err:?}");
        let (err, _) = err_at(
            "night-light { start \"01:00\"; end \"02:00\"; }\nnight-light { start \"01:00\"; end \"02:00\"; }\n",
        );
        assert!(matches!(err, ConfigError::Duplicate { .. }), "{err:?}");
    }
}
