//! Typed errors for storage operations.

use thiserror::Error;

/// Error returned by a [`super::Storage`] operation.
#[derive(Debug, Error)]
#[error("storage error: {kind}")]
pub struct StorageError {
    /// Classification.
    pub kind: StorageErrorKind,
}

impl StorageError {
    pub(crate) const fn new(kind: StorageErrorKind) -> Self {
        Self { kind }
    }
}

impl From<std::io::Error> for StorageError {
    fn from(err: std::io::Error) -> Self {
        Self::new(StorageErrorKind::Io(err))
    }
}

/// Classification of storage failures.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum StorageErrorKind {
    /// A read or write ran past the end of the backing object.
    #[error("I/O out of bounds: offset {offset}, len {len}, capacity {capacity}")]
    OutOfBounds {
        /// Offset that was requested.
        offset: u64,
        /// Length that was requested.
        len: u64,
        /// Capacity of the backing object.
        capacity: u64,
    },
    /// The underlying I/O layer returned an error.
    #[error("I/O: {0}")]
    Io(#[from] std::io::Error),
}
