//! The size and colour value readers of [`NodeReader`]: unit-annotated sizes, hex colours and alphas.

use super::*;

/// The units a size key accepts: `(px)` only, or `(px)` / `(pt)` (font sizes).
#[derive(Clone, Copy)]
enum SizeUnits {
    Px,
    PxOrPt,
}

impl SizeUnits {
    /// The accepted forms, spelled with `n` as the example number.
    fn accepted(self, n: &str) -> String {
        match self {
            Self::Px => format!("(px){n}"),
            Self::PxOrPt => format!("(px){n} or (pt){n}"),
        }
    }

    fn expected(self) -> &'static str {
        match self {
            Self::Px => "a (px) size",
            Self::PxOrPt => "a (px) or (pt) size",
        }
    }
}

/// Where a size that does not fit a `u32` is reported: an argument's at the node (with its
/// range), a property's at the property (against `0..=u32::MAX`).
#[derive(Clone, Copy)]
enum SizeSite {
    Argument,
    Property,
}
impl<'a> NodeReader<'a> {
    // ---- unit-annotated sizes -----------------------------------------------------------

    /// Reads one size entry that must carry a unit annotation: `(px)N` (an integer) or, when
    /// `units` is [`SizeUnits::PxOrPt`], `(pt)N` (integer or decimal, converted to px as
    /// `N * 96 / 72` rounded half away from zero), and checks the px value against `range`.
    /// `key` names the key in unit errors; `site` says where a value that does not fit a `u32`
    /// is reported. A value outside `range` is an `InvalidValue` at the **node**, quoting the
    /// value as written (`(pt)80 = 107px`).
    fn size_entry(
        &self,
        entry: &KdlEntry,
        key: &str,
        what: &str,
        units: SizeUnits,
        range: &RangeInclusive<u32>,
        site: SizeSite,
    ) -> Result<u32, ConfigError> {
        let at = locate(self.text, entry.span().offset());
        let value = entry.value();
        let written = value.to_string();
        let accepted = units.accepted(&written);
        let Some(ty) = entry.ty().map(|t| t.value()) else {
            return Err(self.invalid_at(at, what, format!("`{key}` needs a unit: {accepted}")));
        };
        let (px, as_written) = match (ty, units) {
            ("px", _) => {
                let n = value
                    .as_integer()
                    .ok_or_else(|| self.wrong_type_at(at, what, "an integer"))?;
                (n, format!("(px){n}"))
            }
            ("pt", SizeUnits::PxOrPt) => {
                #[allow(clippy::cast_precision_loss)]
                let pt = value
                    .as_float()
                    .or_else(|| value.as_integer().map(|n| n as f64))
                    .ok_or_else(|| self.wrong_type_at(at, what, "a number"))?;
                if !pt.is_finite() || pt < 0.0 {
                    return Err(self.invalid_at(
                        at,
                        what,
                        format!("(pt){written} must not be negative"),
                    ));
                }
                let px = (pt * 96.0 / 72.0).round();
                if px > f64::from(u32::MAX) {
                    return Err(self.invalid_at(
                        at,
                        what,
                        format!("(pt){written} is outside 0..={}", u32::MAX),
                    ));
                }
                #[allow(clippy::cast_possible_truncation)]
                let px = px as i128;
                (px, format!("(pt){written} = {px}px"))
            }
            ("pt", SizeUnits::Px) => {
                return Err(self.invalid_at(
                    at,
                    what,
                    format!("`{key}` accepts only (px), e.g. (px){written}"),
                ));
            }
            (other, _) => {
                return Err(self.invalid_at(
                    at,
                    what,
                    format!("`{key}` has unknown unit ({other}); use {accepted}"),
                ));
            }
        };
        let Ok(n) = u32::try_from(px) else {
            return Err(match site {
                SizeSite::Property => self.invalid_at(
                    at,
                    what,
                    format!("{as_written} is outside 0..={}", u32::MAX),
                ),
                SizeSite::Argument => self.out_of_range(what, &as_written, range),
            });
        };
        if range.contains(&n) {
            Ok(n)
        } else {
            Err(self.out_of_range(what, &as_written, range))
        }
    }

    /// `InvalidValue` at the node: `written` is outside `range`.
    fn out_of_range(&self, what: &str, written: &str, range: &RangeInclusive<u32>) -> ConfigError {
        self.invalid(
            what,
            format!("{written} is outside {}..={}", range.start(), range.end()),
        )
    }

    fn size_arg_in(
        &self,
        units: SizeUnits,
        range: RangeInclusive<u32>,
    ) -> Result<u32, ConfigError> {
        let entry = self.single_arg(units.expected())?;
        let key = self.name();
        self.size_entry(entry, key, "argument", units, &range, SizeSite::Argument)
    }

    /// Exactly one `(px)N` argument within `range` (header `divider`, cursor `size`). A bare
    /// number, `(pt)` or another annotation is an `InvalidValue` at the argument; outside
    /// `range` is an `InvalidValue` at the node, quoting the value as written.
    pub(crate) fn px_arg_in(&self, range: RangeInclusive<u32>) -> Result<u32, ConfigError> {
        self.size_arg_in(SizeUnits::Px, range)
    }

    /// Exactly one font-size argument, `(px)N` or `(pt)N`, converted to px and checked against
    /// `range` (header `size`).
    pub(crate) fn font_size_arg_in(&self, range: RangeInclusive<u32>) -> Result<u32, ConfigError> {
        self.size_arg_in(SizeUnits::PxOrPt, range)
    }

    /// A `(px)N` property `key` within `range`, if present. Unit and overflow errors are at the
    /// property; a value outside `range` is an `InvalidValue` at the node.
    pub(crate) fn px_prop_in(
        &self,
        key: &str,
        range: RangeInclusive<u32>,
    ) -> Result<Option<u32>, ConfigError> {
        let Some(entry) = self.node.entry(key) else {
            return Ok(None);
        };
        let what = format!("property `{key}`");
        self.size_entry(entry, key, &what, SizeUnits::Px, &range, SizeSite::Property)
            .map(Some)
    }

    /// A non-negative `(px)N` property `key`, if present (tiling `gap` / `ring` / `ring-inset`).
    pub(crate) fn px_prop(&self, key: &str) -> Result<Option<u32>, ConfigError> {
        self.px_prop_in(key, 0..=u32::MAX)
    }

    /// A `"#rrggbb"` string property `key`, if present, validated.
    pub(crate) fn hex_prop(&self, key: &str) -> Result<Option<Rgb>, ConfigError> {
        let Some((s, at)) = self.string_prop(key, "a \"#rrggbb\" string")? else {
            return Ok(None);
        };
        Rgb::from_hex(s).map(Some).ok_or_else(|| {
            self.invalid_at(
                at,
                &format!("property `{key}`"),
                format!("`{s}` is not a #rrggbb colour"),
            )
        })
    }

    /// A point-size property `key` (`size=(pt)11.5` or a bare `size=11.5`), if present, with its
    /// location. Any other annotation is an `InvalidValue` at the property; a non-number a
    /// `WrongType`.
    pub(crate) fn pt_prop(&self, key: &str) -> Result<Option<(f64, Location)>, ConfigError> {
        let Some(entry) = self.node.entry(key) else {
            return Ok(None);
        };
        let at = locate(self.text, entry.span().offset());
        let what = format!("property `{key}`");
        if let Some(ty) = entry.ty().map(|t| t.value())
            && ty != "pt"
        {
            return Err(self.invalid_at(
                at,
                &what,
                format!("`{key}` is in points: write (pt)N or a bare number, not ({ty})"),
            ));
        }
        let value = entry.value();
        #[allow(clippy::cast_precision_loss)]
        let n = value
            .as_float()
            .or_else(|| value.as_integer().map(|n| n as f64))
            .ok_or_else(|| self.wrong_type_at(at, &what, "a number"))?;
        Ok(Some((n, at)))
    }

    /// Exactly one `"#rrggbb"` string argument, validated.
    pub(crate) fn hex_arg(&self) -> Result<Rgb, ConfigError> {
        let s = self.string_arg("a \"#rrggbb\" string")?;
        self.hex_value("argument", s)
    }

    /// The already-read string argument `s` (named `what`, e.g. `argument 2`) as a `#rrggbb`
    /// colour; the `InvalidValue` is at the node, as for [`Self::hex_arg`].
    pub(crate) fn hex_value(&self, what: &str, s: &str) -> Result<Rgb, ConfigError> {
        Rgb::from_hex(s).ok_or_else(|| self.invalid(what, format!("`{s}` is not a #rrggbb colour")))
    }

    /// Exactly one number argument in `0.0..=1.0`, as an [`Alpha`].
    pub(crate) fn alpha_arg(&self) -> Result<Alpha, ConfigError> {
        let a = self.float_arg()?;
        Alpha::from_f64(a)
            .ok_or_else(|| self.invalid("argument", format!("{a} is outside 0.0..=1.0")))
    }
}
