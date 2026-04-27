//! File-backed `Storage` implementation using the stdlib positional-I/O
//! primitives ([`FileExt::read_at`] / [`write_at`][FileExt::write_at]).
//!
//! **Unix-only for M0.** Windows support is deferred — rationale: the Windows
//! `FileExt` surface is `seek_read` / `seek_write` which mutate file position
//! rather than being purely positional, so concurrent writes from multiple
//! peer tasks would race. Until we add an overlapped-I/O or `io_ring` backend
//! for Windows, the crate compiles only for Unix targets.
//!
//! This lands without `unsafe` because `std::os::unix::fs::FileExt` already
//! wraps the underlying `pread`/`pwrite` syscalls. Vectorised `preadv` /
//! `pwritev` via direct `libc` calls is tracked by ADR-0008.

use std::fs::File;
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};

use super::error::{StorageError, StorageErrorKind};
use super::traits::Storage;

/// `Storage` implementation backed by a single open file.
///
/// The file is opened (or created and pre-allocated) at construction time; the
/// capacity is set with [`File::set_len`] so that subsequent `read_at` / `write_at`
/// calls can target any offset in the torrent's logical layout.
#[derive(Debug)]
pub struct FileStorage {
    file: File,
    path: PathBuf,
    capacity: u64,
}

impl FileStorage {
    /// Creates a file-backed storage, opening `path` for read+write. If the
    /// file already exists it is truncated and resized; if it doesn't it is
    /// created.
    ///
    /// # Errors
    /// Propagates filesystem errors.
    pub fn create(path: impl AsRef<Path>, capacity: u64) -> std::io::Result<Self> {
        let path = path.as_ref().to_path_buf();
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&path)?;
        file.set_len(capacity)?;
        Ok(Self {
            file,
            path,
            capacity,
        })
    }

    /// Opens an existing file for read+write without truncating, using the
    /// current file length as capacity.
    ///
    /// # Errors
    /// Propagates filesystem errors.
    pub fn open(path: impl AsRef<Path>) -> std::io::Result<Self> {
        let path = path.as_ref().to_path_buf();
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)?;
        let capacity = file.metadata()?.len();
        Ok(Self {
            file,
            path,
            capacity,
        })
    }

    /// Path the storage was constructed with. The only path magpie ever
    /// touches for this backend; G2 [`Storage::delete`] unlinks exactly this.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    fn bounds_check(&self, offset: u64, len: u64) -> Result<(), StorageError> {
        let end = offset.checked_add(len).ok_or_else(|| {
            StorageError::new(StorageErrorKind::OutOfBounds {
                offset,
                len,
                capacity: self.capacity,
            })
        })?;
        if end > self.capacity {
            return Err(StorageError::new(StorageErrorKind::OutOfBounds {
                offset,
                len,
                capacity: self.capacity,
            }));
        }
        Ok(())
    }
}

impl Storage for FileStorage {
    fn capacity(&self) -> u64 {
        self.capacity
    }

    fn write_block(&self, offset: u64, buf: &[u8]) -> Result<(), StorageError> {
        self.bounds_check(offset, buf.len() as u64)?;
        write_exact_at(&self.file, buf, offset)?;
        Ok(())
    }

    fn read_block(&self, offset: u64, buf: &mut [u8]) -> Result<(), StorageError> {
        self.bounds_check(offset, buf.len() as u64)?;
        read_exact_at(&self.file, buf, offset)?;
        Ok(())
    }

    /// G2 unlinks the path the backend was constructed with. The path is
    /// consumer-supplied at construction (magpie never derives it from
    /// torrent metainfo), so there is no in-magpie path-traversal exposure.
    /// On Unix the open `File` handle remains valid until dropped, which is
    /// the documented contract; readers/writers in flight when `delete` runs
    /// continue to operate against the now-unlinked inode.
    fn delete(&self) -> Result<(), StorageError> {
        std::fs::remove_file(&self.path).map_err(|e| StorageError::new(StorageErrorKind::Io(e)))
    }
}

fn write_exact_at(file: &File, mut buf: &[u8], mut offset: u64) -> std::io::Result<()> {
    while !buf.is_empty() {
        match file.write_at(buf, offset) {
            Ok(0) => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::WriteZero,
                    "failed to write whole buffer",
                ));
            }
            Ok(n) => {
                buf = &buf[n..];
                offset += n as u64;
            }
            Err(ref e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

fn read_exact_at(file: &File, mut buf: &mut [u8], mut offset: u64) -> std::io::Result<()> {
    while !buf.is_empty() {
        match file.read_at(buf, offset) {
            Ok(0) => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "failed to fill whole buffer",
                ));
            }
            Ok(n) => {
                let tmp = buf;
                buf = &mut tmp[n..];
                offset += n as u64;
            }
            Err(ref e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

#[cfg(test)]
// Every test in this module touches the real filesystem (tempdir +
// FileStorage I/O). Miri's default isolation blocks mkdir/open/write,
// so the whole module is excluded from miri runs. See docs/DISCIPLINES.md.
#[cfg(not(miri))]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn roundtrip_file() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("t.dat");
        let s = FileStorage::create(&path, 1024).unwrap();
        s.write_block(100, b"hello world").unwrap();
        let mut buf = [0_u8; 11];
        s.read_block(100, &mut buf).unwrap();
        assert_eq!(&buf, b"hello world");
    }

    #[test]
    fn rejects_out_of_bounds() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("t.dat");
        let s = FileStorage::create(&path, 8).unwrap();
        assert!(matches!(
            s.write_block(4, b"too-many-bytes").unwrap_err().kind,
            StorageErrorKind::OutOfBounds { .. }
        ));
    }

    #[test]
    fn open_existing() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("t.dat");
        {
            let s = FileStorage::create(&path, 64).unwrap();
            s.write_block(0, b"persist").unwrap();
        }
        let s = FileStorage::open(&path).unwrap();
        let mut buf = [0_u8; 7];
        s.read_block(0, &mut buf).unwrap();
        assert_eq!(&buf, b"persist");
        assert_eq!(s.capacity(), 64);
    }
}
