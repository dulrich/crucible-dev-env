//! Typed errors for the persistence core.

use std::path::PathBuf;

/// Everything that can go wrong parsing, writing or storing a workspace record.
#[derive(Debug, thiserror::Error)]
pub enum PersistError {
    /// The text is not valid KDL.
    #[error("KDL parse error at line {line}, column {column}: {message}")]
    Parse {
        /// 1-based line of the first diagnostic.
        line: usize,
        /// 1-based column of the first diagnostic.
        column: usize,
        /// The parser's message.
        message: String,
    },
    /// The file's `version` is newer than this build understands.
    #[error("workspace file version {found} is newer than supported version {supported}")]
    UnsupportedVersion {
        /// The version found in the file.
        found: i128,
        /// The newest version this build reads.
        supported: i128,
    },
    /// A node name is not part of the schema (or appears where it is not allowed).
    #[error("line {line}: unknown node `{name}`")]
    UnknownNode {
        /// The node name.
        name: String,
        /// 1-based line.
        line: usize,
    },
    /// A property is not part of the schema for its node.
    #[error("line {line}: unknown property `{prop}` on `{node}`")]
    UnknownProperty {
        /// The node carrying the property.
        node: String,
        /// The property key.
        prop: String,
        /// 1-based line.
        line: usize,
    },
    /// A required field is absent.
    #[error("line {line}: `{node}` is missing required {field}")]
    Missing {
        /// The node lacking the field.
        node: String,
        /// The field (property, argument or child) name.
        field: String,
        /// 1-based line.
        line: usize,
    },
    /// A field has the wrong type or an out-of-range value.
    #[error("line {line}: invalid {field} on `{node}`: expected {expected}")]
    Invalid {
        /// The node carrying the field.
        node: String,
        /// The field name.
        field: String,
        /// What was expected.
        expected: String,
        /// 1-based line.
        line: usize,
    },
    /// A `split` must have exactly two children.
    #[error("line {line}: `split` needs exactly 2 children, found {found}")]
    WrongChildCount {
        /// How many children were found.
        found: usize,
        /// 1-based line.
        line: usize,
    },
    /// A node that may appear once appears several times.
    #[error("line {line}: duplicate `{name}`")]
    Duplicate {
        /// The repeated node name.
        name: String,
        /// 1-based line.
        line: usize,
    },
    /// A path cannot be written to the UTF-8 file format.
    #[error("path is not valid UTF-8: {0:?}")]
    NonUtf8Path(PathBuf),
    /// The workspace name cannot be mapped to a file name (empty or too long).
    #[error("workspace name {0:?} cannot be used as a file name")]
    InvalidName(String),
    /// A filesystem operation failed.
    #[error("{op} {path}: {source}")]
    Io {
        /// What was being attempted.
        op: &'static str,
        /// The path involved.
        path: PathBuf,
        /// The underlying error.
        source: std::io::Error,
    },
}
