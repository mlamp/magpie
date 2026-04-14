//! In-memory `Storage` implementation — backed by a `Vec<u8>` behind a
//! `RwLock` for concurrent reads.
#![allow(
    clippy::significant_drop_tightening,
    clippy::cast_possible_truncation,
    clippy::missing_panics_doc
)]

use std::sync::RwLock;

use super::error::{StorageError, StorageErrorKind};
use super::traits::Storage;

/// `Storage` implementation backed by a fixed-size in-memory buffer.
///
/// Creating a `MemoryStorage` zero-fills its capacity once. Reads and writes
/// are byte-checked against the capacity, never grow the buffer.
///
/// Concurrency: a single `RwLock<Vec<u8>>` serialises all writes. This is
/// fine for tests and for the metadata-only phase of a BEP 9 magnet fetch
/// but will bottleneck under gigabit+ sustained piece throughput. Sharded
/// locking is an M1+ follow-up (tracked alongside the `FileStorage`
/// vectorised I/O work in ADR-0008).
#[derive(Debug)]
pub struct MemoryStorage {
    buf: RwLock<Vec<u8>>,
    capacity: u64,
}

impl MemoryStorage {
    /// Creates a new memory storage with `capacity` zero-initialised bytes.
    #[must_use]
    pub fn new(capacity: u64) -> Self {
        let cap_usize = usize::try_from(capacity).expect("capacity fits in usize on this target");
        Self {
            buf: RwLock::new(vec![0_u8; cap_usize]),
            capacity,
        }
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

impl Storage for MemoryStorage {
    fn capacity(&self) -> u64 {
        self.capacity
    }

    fn write_block(&self, offset: u64, buf: &[u8]) -> Result<(), StorageError> {
        self.bounds_check(offset, buf.len() as u64)?;
        let offset_usize = offset as usize;
        let mut guard = self.buf.write().expect("poisoned");
        guard[offset_usize..offset_usize + buf.len()].copy_from_slice(buf);
        Ok(())
    }

    fn read_block(&self, offset: u64, buf: &mut [u8]) -> Result<(), StorageError> {
        self.bounds_check(offset, buf.len() as u64)?;
        let offset_usize = offset as usize;
        let guard = self.buf.read().expect("poisoned");
        buf.copy_from_slice(&guard[offset_usize..offset_usize + buf.len()]);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_small() {
        let s = MemoryStorage::new(32);
        s.write_block(8, b"hello").unwrap();
        let mut buf = [0_u8; 5];
        s.read_block(8, &mut buf).unwrap();
        assert_eq!(&buf, b"hello");
    }

    #[test]
    fn rejects_out_of_bounds() {
        let s = MemoryStorage::new(4);
        assert!(matches!(
            s.write_block(2, b"12345").unwrap_err().kind,
            StorageErrorKind::OutOfBounds { .. }
        ));
        let mut buf = [0_u8; 5];
        assert!(s.read_block(0, &mut buf).is_err());
    }

    #[test]
    fn rejects_offset_overflow() {
        let s = MemoryStorage::new(16);
        assert!(matches!(
            s.write_block(u64::MAX - 1, b"xy").unwrap_err().kind,
            StorageErrorKind::OutOfBounds { .. }
        ));
    }

    #[test]
    fn writev_sequential() {
        use super::super::traits::IoVec;
        let s = MemoryStorage::new(16);
        s.writev(&[
            IoVec {
                offset: 0,
                buf: b"hello",
            },
            IoVec {
                offset: 5,
                buf: b" world",
            },
        ])
        .unwrap();
        let mut out = [0_u8; 11];
        s.read_block(0, &mut out).unwrap();
        assert_eq!(&out, b"hello world");
    }
}
