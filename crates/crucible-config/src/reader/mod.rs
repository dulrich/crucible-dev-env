//! A typed view of one KDL node: positional arguments, properties and children, with every
//! failure reported as a located [`ConfigError`].

use std::ops::RangeInclusive;

use kdl::{KdlDocument, KdlNode, KdlValue};

use crate::error::{ConfigError, Location};
use crate::model::Rgb;

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

    /// Exactly one positional argument.
    fn single_arg(&self, expected: &'static str) -> Result<&'a KdlValue, ConfigError> {
        let mut all = self.args().into_iter();
        let (first, _) = all.next().ok_or_else(|| self.missing(expected))?;
        if let Some((_, at)) = all.next() {
            return Err(self.unexpected_at(at));
        }
        Ok(first)
    }

    /// Exactly one argument, a `#true` / `#false`.
    pub(crate) fn bool_arg(&self) -> Result<bool, ConfigError> {
        self.single_arg("a #true or #false")?
            .as_bool()
            .ok_or_else(|| self.wrong_type("argument", "#true or #false"))
    }

    /// Exactly one argument, an integer.
    pub(crate) fn int_arg(&self) -> Result<i128, ConfigError> {
        self.single_arg("an integer")?
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

    /// Exactly one string argument naming one of `names` (a `const NAMES` table); anything else
    /// is an `InvalidValue` at the node listing the names in table order.
    pub(crate) fn enum_arg<T: Copy>(
        &self,
        label: &'static str,
        names: &[(T, &str)],
    ) -> Result<T, ConfigError> {
        let s = self.string_arg(label)?;
        names
            .iter()
            .find_map(|(value, name)| (*name == s).then_some(*value))
            .ok_or_else(|| {
                let list: Vec<&str> = names.iter().map(|(_, name)| *name).collect();
                self.invalid(
                    "argument",
                    format!("`{s}` is not one of {}", list.join(", ")),
                )
            })
    }

    /// Exactly one argument, a number: a KDL float (`1.4167`) or an integer (`2`, widened).
    pub(crate) fn float_arg(&self) -> Result<f64, ConfigError> {
        let value = self.single_arg("a number")?;
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

    /// A non-negative integer property `key`, if present.
    pub(crate) fn u32_prop(&self, key: &str) -> Result<Option<u32>, ConfigError> {
        let Some(entry) = self.node.entry(key) else {
            return Ok(None);
        };
        let at = locate(self.text, entry.span().offset());
        let what = format!("property `{key}`");
        let n = entry
            .value()
            .as_integer()
            .ok_or_else(|| self.wrong_type_at(at, &what, "an integer"))?;
        u32::try_from(n)
            .map(Some)
            .map_err(|_| self.invalid_at(at, &what, format!("{n} is outside 0..={}", u32::MAX)))
    }

    /// A non-negative integer property `key` within `range`, if present. A negative or oversize
    /// value is [`NodeReader::u32_prop`]'s error at the property; a value outside `range` is an
    /// `InvalidValue` at the **node** (where the checks it replaces always reported it).
    pub(crate) fn u32_prop_in(
        &self,
        key: &str,
        range: RangeInclusive<u32>,
    ) -> Result<Option<u32>, ConfigError> {
        let Some(n) = self.u32_prop(key)? else {
            return Ok(None);
        };
        if !range.contains(&n) {
            return Err(self.invalid(
                &format!("property `{key}`"),
                format!("{n} is outside {}..={}", range.start(), range.end()),
            ));
        }
        Ok(Some(n))
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
    /// [`ConfigError::Duplicate`] at that second child, raised before the caller sees it.
    pub(crate) fn children(
        &self,
        repeatable: &'static [&'static str],
    ) -> Result<Children<'a>, ConfigError> {
        self.no_properties()?;
        self.no_args()?;
        Ok(self.children_unchecked(repeatable))
    }

    /// Like [`NodeReader::children`] but leaves the block node's own arguments to the caller
    /// (`output "DP-1" { … }`, whose one argument the caller reads first).
    pub(crate) fn children_keeping_args(
        &self,
        repeatable: &'static [&'static str],
    ) -> Result<Children<'a>, ConfigError> {
        self.no_properties()?;
        Ok(self.children_unchecked(repeatable))
    }

    fn children_unchecked(&self, repeatable: &'static [&'static str]) -> Children<'a> {
        let nodes = self.node.children().map_or(&[][..], KdlDocument::nodes);
        Children::new(self.text, nodes, repeatable)
    }
}

/// The once-only child iterator behind [`NodeReader::children`] (also used for the top level).
pub(crate) struct Children<'a> {
    text: &'a str,
    nodes: std::slice::Iter<'a, KdlNode>,
    repeatable: &'static [&'static str],
    seen: Vec<&'a str>,
}

impl<'a> Children<'a> {
    pub(crate) fn new(
        text: &'a str,
        nodes: &'a [KdlNode],
        repeatable: &'static [&'static str],
    ) -> Self {
        Self {
            text,
            nodes: nodes.iter(),
            repeatable,
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
        Some(Ok(reader))
    }
}

#[cfg(test)]
mod tests;
