//! Typed errors for metainfo parsing.

use magpie_bt_bencode::DecodeError;
use thiserror::Error;

/// Error returned when parsing a `.torrent` file.
#[derive(Debug, Error)]
#[error("metainfo parse error: {kind}")]
pub struct ParseError {
    /// Classification of the failure.
    pub kind: ParseErrorKind,
}

impl ParseError {
    pub(crate) const fn new(kind: ParseErrorKind) -> Self {
        Self { kind }
    }
}

impl From<DecodeError> for ParseError {
    fn from(err: DecodeError) -> Self {
        Self::new(ParseErrorKind::Bencode(err))
    }
}

/// Classification of metainfo parse failures.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum ParseErrorKind {
    /// Underlying bencode could not be decoded.
    #[error("bencode decode: {0}")]
    Bencode(#[from] DecodeError),
    /// A required top-level or nested field was missing.
    #[error("missing required field `{0}`")]
    MissingField(&'static str),
    /// A field had the wrong bencode type.
    #[error("field `{field}` has wrong type; expected {expected}")]
    WrongType {
        /// Field name.
        field: &'static str,
        /// Human-readable description of the expected type.
        expected: &'static str,
    },
    /// The integer value of a field was out of its permitted range.
    #[error("field `{field}` value {value} is out of range")]
    ValueOutOfRange {
        /// Field name.
        field: &'static str,
        /// Offending value (stringified to avoid losing precision).
        value: String,
    },
    /// `info.piece length` was not a positive power of two.
    #[error("piece length {0} is not a positive power of two")]
    InvalidPieceLength(u64),
    /// The `info.pieces` blob length was not a multiple of 20 (SHA-1 digest size).
    #[error("`pieces` blob length {0} is not a multiple of 20")]
    InvalidPiecesBlob(usize),
    /// A v2 piece-root value was not exactly 32 bytes.
    #[error("v2 pieces root has wrong length {0}, expected 32")]
    InvalidPiecesRoot(usize),
    /// `meta version` was present but not `2` (the only version BEP 52 defines).
    #[error("unsupported `meta version` {0}")]
    UnsupportedMetaVersion(u64),
    /// The info dictionary neither satisfies v1 nor v2 structure rules.
    #[error("info dictionary is neither v1, v2, nor hybrid")]
    UnrecognisedInfo,
    /// A v1 info dict contained both `length` and `files`, which is disallowed.
    #[error("info dict has both `length` and `files`")]
    ConflictingV1Layout,
    /// A path component in a file entry was empty or contained `/` or `\0`.
    #[error("invalid path component")]
    InvalidPathComponent,
    /// File tree structure was malformed (e.g. entry was neither a file leaf nor a subdir).
    #[error("malformed v2 file tree")]
    MalformedFileTree,
}
