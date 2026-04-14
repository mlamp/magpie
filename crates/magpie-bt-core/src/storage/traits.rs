//! Core `Storage` trait and its vectorised-I/O newtypes.

use super::error::StorageError;

/// Immutable vector-I/O slice (`(offset, bytes_to_write)`).
#[derive(Debug, Clone, Copy)]
pub struct IoVec<'a> {
    /// Byte offset into the storage object.
    pub offset: u64,
    /// Bytes to write at that offset.
    pub buf: &'a [u8],
}

/// Mutable vector-I/O slice (`(offset, bytes_to_read)`).
#[derive(Debug)]
pub struct IoVecMut<'a> {
    /// Byte offset into the storage object.
    pub offset: u64,
    /// Destination buffer to populate from storage.
    pub buf: &'a mut [u8],
}

/// Block-granular storage backing a torrent's file payload.
///
/// Implementations must be thread-safe (`Send + Sync`) because verification
/// and multiple peer sessions may read and write concurrently.
pub trait Storage: Send + Sync {
    /// Returns the total capacity (bytes) of the backing object.
    fn capacity(&self) -> u64;

    /// Writes `buf` at `offset`.
    ///
    /// # Errors
    /// Returns [`StorageError`] with `OutOfBounds` if the write would extend
    /// past [`Storage::capacity`], or `Io` for backend failures.
    fn write_block(&self, offset: u64, buf: &[u8]) -> Result<(), StorageError>;

    /// Reads `buf.len()` bytes from `offset` into `buf`.
    ///
    /// # Errors
    /// See [`Storage::write_block`].
    fn read_block(&self, offset: u64, buf: &mut [u8]) -> Result<(), StorageError>;

    /// Vectorised write. Default falls back to sequential `write_block` calls
    /// — implementations with native scatter/gather should override.
    ///
    /// # Errors
    /// See [`Storage::write_block`].
    fn writev(&self, iov: &[IoVec<'_>]) -> Result<(), StorageError> {
        for v in iov {
            self.write_block(v.offset, v.buf)?;
        }
        Ok(())
    }

    /// Vectorised read. Default falls back to sequential `read_block` calls.
    ///
    /// # Errors
    /// See [`Storage::read_block`].
    fn readv(&self, iov: &mut [IoVecMut<'_>]) -> Result<(), StorageError> {
        for v in iov {
            self.read_block(v.offset, v.buf)?;
        }
        Ok(())
    }

    /// Permanently delete the backing storage. Used by
    /// [`Engine::remove`](crate::engine::Engine::remove) when called with
    /// `delete_files = true` (G1 in `docs/api-audit.md`).
    ///
    /// Default impl is a no-op (`Ok(())`) — appropriate for in-memory or
    /// otherwise non-file backends. File-backed storages override to unlink
    /// the file owned at construction time.
    ///
    /// # Path safety
    ///
    /// Implementations MUST only delete storage they own. magpie does not
    /// derive paths from torrent metainfo (the consumer hands the storage
    /// backend a fully-resolved path at construction); therefore there is no
    /// in-magpie path-traversal surface to defend against. Backends that
    /// derive paths from untrusted input must reject `..` / absolute paths
    /// at construction, not here.
    ///
    /// # Errors
    /// Backend-specific. Returns [`StorageError`] with `Io` for filesystem
    /// failures.
    fn delete(&self) -> Result<(), StorageError> {
        Ok(())
    }
}
