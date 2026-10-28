//! The typed errors a config load can produce, each carrying a [`Location`].

use std::path::PathBuf;

/// A 1-based line/column position in the config source.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Location {
    /// 1-based line number.
    pub line: usize,
    /// 1-based column (in characters).
    pub column: usize,
}

impl std::fmt::Display for Location {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{}", self.line, self.column)
    }
}

/// Why a config file could not be loaded. Every parse-level variant carries a [`Location`].
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    /// The file exists but could not be read.
    #[error("cannot read {}: {source}", path.display())]
    Io {
        /// The config path.
        path: PathBuf,
        /// The underlying I/O error.
        source: std::io::Error,
    },
    /// The text is not valid KDL.
    #[error("{at}: invalid KDL: {message}")]
    Syntax {
        /// Where the syntax error is.
        at: Location,
        /// The parser's message.
        message: String,
    },
    /// A node name the schema does not define (at the top level or inside a block).
    #[error("{at}: unknown node `{name}` in {context}")]
    UnknownNode {
        /// Where the node is.
        at: Location,
        /// The node name.
        name: String,
        /// `"the top level"`, `"tiling"`, `"picker"`, `"launcher"`, `"overlay"`, `"header"`,
        /// `"output"` or `"binds"`.
        context: &'static str,
    },
    /// A property the node does not accept.
    #[error("{at}: unknown property `{name}` on `{node}`")]
    UnknownProperty {
        /// Where the property is.
        at: Location,
        /// The node it appears on.
        node: String,
        /// The property name.
        name: String,
    },
    /// More arguments (or any arguments, where none are allowed) than the node takes.
    #[error("{at}: unexpected extra argument on `{node}`")]
    UnexpectedArgument {
        /// Where the argument is.
        at: Location,
        /// The node it appears on.
        node: String,
    },
    /// A required argument is absent.
    #[error("{at}: `{node}` is missing {expected}")]
    MissingArgument {
        /// Where the node is.
        at: Location,
        /// The node name.
        node: String,
        /// What is missing, e.g. `"a command string"`.
        expected: &'static str,
    },
    /// An argument or property has the wrong KDL type.
    #[error("{at}: `{node}` {what} must be {expected}")]
    WrongType {
        /// Where the value is.
        at: Location,
        /// The node it appears on.
        node: String,
        /// Which value, e.g. `"argument"` or ``"property `inner`"``.
        what: String,
        /// The required type, e.g. `"a string"`.
        expected: &'static str,
    },
    /// A value of the right type but outside what the schema allows (negative or oversized gap,
    /// empty string).
    #[error("{at}: `{node}` {what}: {message}")]
    InvalidValue {
        /// Where the value is.
        at: Location,
        /// The node it appears on.
        node: String,
        /// Which value.
        what: String,
        /// What is wrong with it.
        message: String,
    },
    /// A single-use node appears twice.
    #[error("{at}: duplicate `{name}` node")]
    Duplicate {
        /// Where the second occurrence is.
        at: Location,
        /// The node name.
        name: String,
    },
}
