//! A typed view of one KDL node: positional arguments, properties and children, with every
//! failure reported as a located [`ConfigError`].

use std::ops::RangeInclusive;

use kdl::{KdlDocument, KdlEntry, KdlNode, KdlValue};

use crate::error::{ConfigError, Location};
use crate::model::{Named, Rgb};

/// Converts a char offset into a 1-based line/column.
pub(crate) fn locate(text: &str, offset: usize) -> Location {
    let (mut line, mut column) = (1, 1);
    for ch in text.chars().take(offset) {
        if ch == '\n' {
            line += 1;
            column = 1;
        } else {
            column += 1;
        }
    }
    Location { line, column }
}

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

/// One node of the parsed document together with the source text its locations refer to.
#[derive(Clone, Copy)]
pub(crate) struct NodeReader<'a> {
    text: &'a str,
    node: &'a KdlNode,
}

impl<'a> NodeReader<'a> {
    pub(crate) fn new(text: &'a str, node: &'a KdlNode) -> Self {
        Self { text, node }
    }

    pub(crate) fn name(&self) -> &'a str {
        self.node.name().value()
    }

    /// Where the node starts.
    pub(crate) fn at(&self) -> Location {
        locate(self.text, self.node.span().offset())
    }

    fn node_name(&self) -> String {
        self.name().to_string()
    }

    // ---- error constructors -------------------------------------------------------------

    /// `UnknownNode` for this node inside `context`.
    pub(crate) fn unknown_node(&self, context: &'static str) -> ConfigError {
        ConfigError::UnknownNode {
            at: self.at(),
            name: self.node_name(),
            context,
        }
    }

    /// `MissingArgument` at the node.
    pub(crate) fn missing(&self, expected: &'static str) -> ConfigError {
        ConfigError::MissingArgument {
            at: self.at(),
            node: self.node_name(),
            expected,
        }
    }

    /// `WrongType` at the node.
    pub(crate) fn wrong_type(&self, what: &str, expected: &'static str) -> ConfigError {
        self.wrong_type_at(self.at(), what, expected)
    }

    fn wrong_type_at(&self, at: Location, what: &str, expected: &'static str) -> ConfigError {
        ConfigError::WrongType {
            at,
            node: self.node_name(),
            what: what.to_string(),
            expected,
        }
    }

    /// `InvalidValue` at the node.
    pub(crate) fn invalid(&self, what: &str, message: String) -> ConfigError {
        self.invalid_at(self.at(), what, message)
    }

    pub(crate) fn invalid_at(&self, at: Location, what: &str, message: String) -> ConfigError {
        ConfigError::InvalidValue {
            at,
            node: self.node_name(),
            what: what.to_string(),
            message,
        }
    }

    fn unexpected_at(&self, at: Location) -> ConfigError {
        ConfigError::UnexpectedArgument {
            at,
            node: self.node_name(),
        }
    }

    // ---- arguments ----------------------------------------------------------------------

    /// The node's positional arguments with their locations.
    fn args(&self) -> Vec<(&'a KdlValue, Location)> {
        self.node
            .entries()
            .iter()
            .filter(|e| e.name().is_none())
            .map(|e| (e.value(), locate(self.text, e.span().offset())))
            .collect()
    }

    /// Rejects any positional argument (for nodes that take none).
    pub(crate) fn no_args(&self) -> Result<(), ConfigError> {
        match self.args().first() {
            Some((_, at)) => Err(self.unexpected_at(*at)),
            None => Ok(()),
        }
    }

    /// Exactly one positional argument, as its entry (value, type annotation and location).
    fn single_arg(&self, expected: &'static str) -> Result<&'a KdlEntry, ConfigError> {
        let mut all = self.node.entries().iter().filter(|e| e.name().is_none());
        let first = all.next().ok_or_else(|| self.missing(expected))?;
        if let Some(e) = all.next() {
            return Err(self.unexpected_at(locate(self.text, e.span().offset())));
        }
        Ok(first)
    }

    /// Exactly one argument, a `#true` / `#false`.
    pub(crate) fn bool_arg(&self) -> Result<bool, ConfigError> {
        self.single_arg("a #true or #false")?
            .value()
            .as_bool()
            .ok_or_else(|| self.wrong_type("argument", "#true or #false"))
    }

    /// Exactly one argument, an integer.
    pub(crate) fn int_arg(&self) -> Result<i128, ConfigError> {
        self.single_arg("an integer")?
            .value()
            .as_integer()
            .ok_or_else(|| self.wrong_type("argument", "an integer"))
    }

    /// Exactly one integer argument within `range`; outside it is an `InvalidValue` at the node
    /// that quotes the integer as written (`-1 is outside 8..=64`).
    pub(crate) fn u32_arg_in(&self, range: RangeInclusive<u32>) -> Result<u32, ConfigError> {
        let n = self.int_arg()?;
        u32::try_from(n)
            .ok()
            .filter(|n| range.contains(n))
            .ok_or_else(|| {
                self.invalid(
                    "argument",
                    format!("{n} is outside {}..={}", range.start(), range.end()),
                )
            })
    }

    /// Exactly one string argument naming a [`Named`] value; anything else is an `InvalidValue`
    /// at the node listing the names in table order.
    pub(crate) fn enum_arg<T: Named>(&self, label: &'static str) -> Result<T, ConfigError> {
        self.enum_arg_as(label, "argument")
    }

    /// [`NodeReader::enum_arg`] whose `InvalidValue` names the value `what` (`destination`).
    pub(crate) fn enum_arg_as<T: Named>(
        &self,
        label: &'static str,
        what: &str,
    ) -> Result<T, ConfigError> {
        let s = self.string_arg(label)?;
        T::from_name(s).ok_or_else(|| {
            let list: Vec<&str> = T::NAMES.iter().map(|(_, name)| *name).collect();
            self.invalid(what, format!("`{s}` is not one of {}", list.join(", ")))
        })
    }

    /// Exactly one argument, a number: a KDL float (`1.4167`) or an integer (`2`, widened).
    pub(crate) fn float_arg(&self) -> Result<f64, ConfigError> {
        let value = self.single_arg("a number")?.value();
        #[allow(clippy::cast_precision_loss)]
        value
            .as_float()
            .or_else(|| value.as_integer().map(|n| n as f64))
            .ok_or_else(|| self.wrong_type("argument", "a number"))
    }

    /// Exactly `labels.len()` string arguments, `labels` naming each for a missing-argument error.
    pub(crate) fn string_args(&self, labels: &[&'static str]) -> Result<Vec<&'a str>, ConfigError> {
        let all = self.args();
        let mut out = Vec::new();
        for (i, label) in labels.iter().enumerate() {
            let Some((value, at)) = all.get(i) else {
                return Err(self.missing(label));
            };
            let s = value.as_string().ok_or_else(|| {
                self.wrong_type_at(*at, &format!("argument {}", i + 1), "a string")
            })?;
            out.push(s);
        }
        if let Some((_, at)) = all.get(labels.len()) {
            return Err(self.unexpected_at(*at));
        }
        Ok(out)
    }

    /// Exactly one string argument.
    pub(crate) fn string_arg(&self, label: &'static str) -> Result<&'a str, ConfigError> {
        Ok(self.string_args(&[label])?[0])
    }

    /// Exactly one string argument that is not blank; `what` names it in the error.
    pub(crate) fn nonempty_string_arg(
        &self,
        label: &'static str,
        what: &str,
    ) -> Result<String, ConfigError> {
        let s = self.string_arg(label)?;
        self.nonempty(what, s)?;
        Ok(s.to_string())
    }

    /// All positional arguments, all strings (`terminal-args`).
    pub(crate) fn string_list(&self) -> Result<Vec<String>, ConfigError> {
        self.args()
            .into_iter()
            .map(|(value, at)| {
                value
                    .as_string()
                    .map(str::to_string)
                    .ok_or_else(|| self.wrong_type_at(at, "argument", "a string"))
            })
            .collect()
    }

    /// `s` must not be blank; `what` names it in the error.
    pub(crate) fn nonempty(&self, what: &str, s: &str) -> Result<(), ConfigError> {
        if s.trim().is_empty() {
            return Err(self.invalid(what, "must not be empty".to_string()));
        }
        Ok(())
    }

    // ---- properties ---------------------------------------------------------------------

    /// Rejects any property outside `allowed`.
    pub(crate) fn allow_properties(&self, allowed: &[&str]) -> Result<(), ConfigError> {
        for entry in self.node.entries() {
            if let Some(key) = entry.name()
                && !allowed.contains(&key.value())
            {
                return Err(ConfigError::UnknownProperty {
                    at: locate(self.text, entry.span().offset()),
                    node: self.node_name(),
                    name: key.value().to_string(),
                });
            }
        }
        Ok(())
    }

    /// Rejects any property (for nodes that take none).
    pub(crate) fn no_properties(&self) -> Result<(), ConfigError> {
        self.allow_properties(&[])
    }

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
        Rgb::from_hex(s)
            .ok_or_else(|| self.invalid("argument", format!("`{s}` is not a #rrggbb colour")))
    }

    /// Exactly two arguments, an integer then a string (`palette 1 "#f62b5a"`), with the
    /// integer's location.
    pub(crate) fn int_string_args(
        &self,
        labels: [&'static str; 2],
    ) -> Result<(i128, Location, &'a str), ConfigError> {
        let all = self.args();
        let (Some((n, n_at)), Some((s, s_at))) = (all.first(), all.get(1)) else {
            return Err(self.missing(if all.is_empty() { labels[0] } else { labels[1] }));
        };
        if let Some((_, at)) = all.get(2) {
            return Err(self.unexpected_at(*at));
        }
        let n = n
            .as_integer()
            .ok_or_else(|| self.wrong_type_at(*n_at, "argument 1", "an integer"))?;
        let s = s
            .as_string()
            .ok_or_else(|| self.wrong_type_at(*s_at, "argument 2", "a string"))?;
        Ok((n, *n_at, s))
    }

    /// A string property `key`, if present, with its location; `expected` words a type error.
    pub(crate) fn string_prop(
        &self,
        key: &str,
        expected: &'static str,
    ) -> Result<Option<(&'a str, Location)>, ConfigError> {
        let Some(entry) = self.node.entry(key) else {
            return Ok(None);
        };
        let at = locate(self.text, entry.span().offset());
        let s = entry
            .value()
            .as_string()
            .ok_or_else(|| self.wrong_type_at(at, &format!("property `{key}`"), expected))?;
        Ok(Some((s, at)))
    }

    // ---- children -----------------------------------------------------------------------

    /// Iterates the children of a block node, rejecting properties and arguments on the block
    /// itself. A child whose name is not in `repeatable` may appear once; a second occurrence is
    /// [`ConfigError::Duplicate`] at that second child, raised before the caller sees it. Children
    /// may carry properties (the caller validates them).
    pub(crate) fn children(
        &self,
        repeatable: &'static [&'static str],
    ) -> Result<Children<'a>, ConfigError> {
        self.no_properties()?;
        self.no_args()?;
        Ok(self.children_unchecked(repeatable, None))
    }

    /// Like [`NodeReader::children`] but every child must be property-free except those named in
    /// `exempt` (`None` of them: pass `&[]`); a property elsewhere is an `UnknownProperty`
    /// raised before the caller sees the child.
    pub(crate) fn children_strict(
        &self,
        repeatable: &'static [&'static str],
        exempt: &'static [&'static str],
    ) -> Result<Children<'a>, ConfigError> {
        self.no_properties()?;
        self.no_args()?;
        Ok(self.children_unchecked(repeatable, Some(exempt)))
    }

    /// Like [`NodeReader::children_strict`] but leaves the block node's own arguments to the
    /// caller (`output "DP-1" { … }`, whose one argument the caller reads first).
    pub(crate) fn children_keeping_args(
        &self,
        repeatable: &'static [&'static str],
    ) -> Result<Children<'a>, ConfigError> {
        self.no_properties()?;
        Ok(self.children_unchecked(repeatable, Some(&[])))
    }

    fn children_unchecked(
        &self,
        repeatable: &'static [&'static str],
        strict: Option<&'static [&'static str]>,
    ) -> Children<'a> {
        let nodes = self.node.children().map_or(&[][..], KdlDocument::nodes);
        Children::new(self.text, nodes, repeatable, strict)
    }
}

/// The once-only child iterator behind [`NodeReader::children`] (also used for the top level).
pub(crate) struct Children<'a> {
    text: &'a str,
    nodes: std::slice::Iter<'a, KdlNode>,
    repeatable: &'static [&'static str],
    strict: Option<&'static [&'static str]>,
    seen: Vec<&'a str>,
}

impl<'a> Children<'a> {
    /// `strict` is `Some(exempt)` to reject properties on every child not named in `exempt`.
    pub(crate) fn new(
        text: &'a str,
        nodes: &'a [KdlNode],
        repeatable: &'static [&'static str],
        strict: Option<&'static [&'static str]>,
    ) -> Self {
        Self {
            text,
            nodes: nodes.iter(),
            repeatable,
            strict,
            seen: Vec::new(),
        }
    }
}

impl<'a> Iterator for Children<'a> {
    type Item = Result<NodeReader<'a>, ConfigError>;

    fn next(&mut self) -> Option<Self::Item> {
        let reader = NodeReader::new(self.text, self.nodes.next()?);
        let name = reader.name();
        if !self.repeatable.contains(&name) {
            if self.seen.contains(&name) {
                return Some(Err(ConfigError::Duplicate {
                    at: reader.at(),
                    name: name.to_string(),
                }));
            }
            self.seen.push(name);
        }
        if let Some(exempt) = self.strict
            && !exempt.contains(&name)
            && let Err(err) = reader.no_properties()
        {
            return Some(Err(err));
        }
        Some(Ok(reader))
    }
}

#[cfg(test)]
mod tests;
