//! BEP 9 metadata assembly state machine.
//!
//! Tracks received metadata pieces and assembles them into the full info dict
//! when a torrent starts from a magnet link. The assembler is intentionally
//! simple — flag-based state tracking, no complex enum hierarchy.

use std::collections::HashMap;

use bytes::Bytes;
use magpie_bt_metainfo::sha1;
use magpie_bt_wire::{MAX_METADATA_SIZE, metadata_piece_count};

use crate::session::messages::PeerSlot;
use crate::session::torrent::TorrentParams;

/// Errors from metadata assembly.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum MetadataAssemblyError {
    /// SHA-1 of assembled metadata does not match expected info hash.
    #[error("metadata SHA-1 mismatch: expected {expected}, got {got}")]
    HashMismatch {
        /// Expected hex-encoded hash.
        expected: String,
        /// Actual hex-encoded hash.
        got: String,
    },
    /// Assembled metadata could not be parsed as a valid info dict.
    #[error("metadata parse failed: {0}")]
    ParseFailed(String),
    /// Total metadata size not yet learned from any peer.
    #[error("metadata size not known yet")]
    SizeUnknown,
    /// Not all pieces have been received yet.
    #[error("incomplete: {received}/{total} pieces")]
    Incomplete {
        /// Pieces received so far.
        received: u32,
        /// Total pieces expected.
        total: u32,
    },
}

/// Assembles metadata pieces received via BEP 9 `ut_metadata` exchange.
///
/// Created when the engine starts a torrent from a magnet link. The assembler
/// collects 16 KiB pieces, verifies them against the info hash, and produces
/// `TorrentParams` on success.
pub struct MetadataAssembler {
    info_hash: [u8; 20],
    total_size: Option<u64>,
    pieces: Vec<Option<Bytes>>,
    piece_count: u32,
    /// Which peers we've sent requests to (avoid re-requesting from same peer).
    pending_requests: HashMap<u32, PeerSlot>,
}

impl MetadataAssembler {
    /// Create a new assembler for the given info hash. The total metadata
    /// size is unknown until a peer's extension handshake tells us.
    #[must_use]
    pub fn new(info_hash: [u8; 20]) -> Self {
        Self {
            info_hash,
            total_size: None,
            pieces: Vec::new(),
            piece_count: 0,
            pending_requests: HashMap::new(),
        }
    }

    /// Set the total metadata size, learned from a peer's extension handshake.
    /// Allocates the pieces vector. Only the **first** call takes effect — once
    /// the size is known we trust it and ignore subsequent (possibly malicious)
    /// values. Also rejects sizes above [`MAX_METADATA_SIZE`] (16 MiB) to
    /// prevent unbounded allocation from a malicious peer.
    pub fn set_total_size(&mut self, size: u64) {
        if let Some(existing) = self.total_size {
            if existing != size {
                tracing::debug!(
                    existing,
                    proposed = size,
                    "ignoring different metadata_size from peer"
                );
            }
            return;
        }
        if size > MAX_METADATA_SIZE {
            tracing::warn!(
                size,
                max = MAX_METADATA_SIZE,
                "metadata_size exceeds maximum — ignoring"
            );
            return;
        }
        self.total_size = Some(size);
        self.piece_count = metadata_piece_count(size);
        self.pieces = vec![None; self.piece_count as usize];
        self.pending_requests.clear();
    }

    /// Whether the total metadata size has been learned.
    #[must_use]
    pub const fn has_size(&self) -> bool {
        self.total_size.is_some()
    }

    /// Total metadata size, if known.
    #[must_use]
    pub const fn total_size(&self) -> Option<u64> {
        self.total_size
    }

    /// Number of metadata pieces expected.
    #[must_use]
    pub const fn piece_count(&self) -> u32 {
        self.piece_count
    }

    /// Whether all pieces have been received.
    #[must_use]
    pub fn is_complete(&self) -> bool {
        self.piece_count > 0 && self.pieces.iter().all(Option::is_some)
    }

    /// Whether the given piece still needs to be fetched (not received and
    /// not currently pending from another peer).
    #[must_use]
    pub fn needs_piece(&self, piece: u32) -> bool {
        let idx = piece as usize;
        if idx >= self.pieces.len() {
            return false;
        }
        self.pieces[idx].is_none() && !self.pending_requests.contains_key(&piece)
    }

    /// Return the index of the first piece we still need (not received,
    /// not pending).
    #[must_use]
    pub fn next_needed_piece(&self) -> Option<u32> {
        (0..self.piece_count).find(|&i| self.needs_piece(i))
    }

    /// Mark a piece as requested from the given peer.
    pub fn mark_pending(&mut self, piece: u32, peer: PeerSlot) {
        self.pending_requests.insert(piece, peer);
    }

    /// Store a received metadata piece. Removes it from pending.
    ///
    /// # Errors
    ///
    /// Returns `Err` if the piece index is out of range.
    pub fn receive_piece(&mut self, piece: u32, data: Bytes) -> Result<(), MetadataAssemblyError> {
        if self.total_size.is_none() {
            return Err(MetadataAssemblyError::SizeUnknown);
        }
        // Belt-and-suspenders: reject piece indices beyond expected count.
        if piece >= self.piece_count {
            return Err(MetadataAssemblyError::Incomplete {
                received: self.received_count(),
                total: self.piece_count,
            });
        }
        let idx = piece as usize;
        if idx >= self.pieces.len() {
            return Err(MetadataAssemblyError::Incomplete {
                received: self.received_count(),
                total: self.piece_count,
            });
        }
        self.pieces[idx] = Some(data);
        self.pending_requests.remove(&piece);
        Ok(())
    }

    /// A peer rejected our request — clear the pending marker so we can
    /// re-request from another peer. This is normal protocol flow.
    pub fn receive_reject(&mut self, piece: u32) {
        self.pending_requests.remove(&piece);
    }

    /// If all pieces are present, concatenate and return the full metadata bytes.
    #[must_use]
    #[allow(clippy::cast_possible_truncation)]
    pub fn try_assemble(&self) -> Option<Vec<u8>> {
        if !self.is_complete() {
            return None;
        }
        let total = self.total_size.unwrap_or(0) as usize;
        let mut out = Vec::with_capacity(total);
        for data in self.pieces.iter().flatten() {
            out.extend_from_slice(data);
        }
        // Trim to exact total_size — the last piece may have trailing bytes
        // that are padding.
        out.truncate(total);
        Some(out)
    }

    /// Assemble all pieces, verify SHA-1 against the expected info hash,
    /// then parse into `TorrentParams`.
    ///
    /// # Errors
    ///
    /// Returns [`MetadataAssemblyError`] on hash mismatch, incomplete
    /// assembly, or parse failure.
    pub fn verify_and_parse(&self) -> Result<(Vec<u8>, TorrentParams), MetadataAssemblyError> {
        if self.total_size.is_none() {
            return Err(MetadataAssemblyError::SizeUnknown);
        }
        let assembled = self
            .try_assemble()
            .ok_or_else(|| MetadataAssemblyError::Incomplete {
                received: self.received_count(),
                total: self.piece_count,
            })?;

        // SHA-1 verify
        let actual_hash = sha1(&assembled);
        if actual_hash != self.info_hash {
            return Err(MetadataAssemblyError::HashMismatch {
                expected: hex_encode(&self.info_hash),
                got: hex_encode(&actual_hash),
            });
        }

        // Parse the info dict by wrapping it in a minimal .torrent envelope
        // (`d4:info<info_bytes>e`) and using the public `parse()` API. This
        // avoids exposing metainfo internals.
        let mut torrent_bytes = Vec::with_capacity(assembled.len() + 12);
        torrent_bytes.extend_from_slice(b"d4:info");
        torrent_bytes.extend_from_slice(&assembled);
        torrent_bytes.push(b'e');
        let meta_info = magpie_bt_metainfo::parse(&torrent_bytes)
            .map_err(|e| MetadataAssemblyError::ParseFailed(e.to_string()))?;
        let v1 =
            meta_info.info.v1.as_ref().ok_or_else(|| {
                MetadataAssemblyError::ParseFailed("not a v1 info dict".to_string())
            })?;
        let piece_count = u32::try_from(v1.pieces.len() / 20)
            .map_err(|_| MetadataAssemblyError::ParseFailed("piece count overflow".to_string()))?;

        // Sanity-check parsed values to reject absurd torrents that would
        // exhaust memory downstream. Limits are generous (2M pieces ≈ 32 TiB
        // at 16 MiB piece length; 64 MiB piece length is beyond any real
        // torrent).
        if piece_count > 2_000_000 {
            return Err(MetadataAssemblyError::ParseFailed(format!(
                "piece_count {piece_count} exceeds safety limit of 2 000 000"
            )));
        }
        if meta_info.info.piece_length > 64 * 1024 * 1024 {
            return Err(MetadataAssemblyError::ParseFailed(format!(
                "piece_length {} exceeds safety limit of 64 MiB",
                meta_info.info.piece_length
            )));
        }

        let total_length = match &v1.files {
            magpie_bt_metainfo::FileListV1::Single { length } => *length,
            magpie_bt_metainfo::FileListV1::Multi { files } => files.iter().map(|f| f.length).sum(),
        };
        let meta = TorrentParams {
            piece_count,
            piece_length: meta_info.info.piece_length,
            total_length,
            piece_hashes: v1.pieces.to_vec(),
            private: meta_info.info.private,
        };

        Ok((assembled, meta))
    }

    fn received_count(&self) -> u32 {
        self.pieces
            .iter()
            .filter(|p| p.is_some())
            .count()
            .try_into()
            .unwrap_or(u32::MAX)
    }
}

fn hex_encode(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes
        .iter()
        .fold(String::with_capacity(bytes.len() * 2), |mut s, b| {
            let _ = write!(s, "{b:02x}");
            s
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_assembler_has_no_size() {
        let a = MetadataAssembler::new([0; 20]);
        assert!(!a.has_size());
        assert!(!a.is_complete());
        assert_eq!(a.next_needed_piece(), None);
    }

    #[test]
    fn set_total_size_allocates_pieces() {
        let mut a = MetadataAssembler::new([0; 20]);
        a.set_total_size(32768);
        assert!(a.has_size());
        assert_eq!(a.piece_count(), 2);
        assert!(!a.is_complete());
        assert_eq!(a.next_needed_piece(), Some(0));
    }

    #[test]
    fn receive_piece_stores_data() {
        let mut a = MetadataAssembler::new([0; 20]);
        a.set_total_size(16384);
        assert!(a.needs_piece(0));
        a.receive_piece(0, Bytes::from_static(b"hello")).unwrap();
        assert!(!a.needs_piece(0));
        assert!(a.is_complete());
    }

    #[test]
    fn pending_prevents_double_request() {
        let mut a = MetadataAssembler::new([0; 20]);
        a.set_total_size(32768);
        a.mark_pending(0, PeerSlot(1));
        assert!(!a.needs_piece(0));
        assert_eq!(a.next_needed_piece(), Some(1));
    }

    #[test]
    fn reject_clears_pending() {
        let mut a = MetadataAssembler::new([0; 20]);
        a.set_total_size(32768);
        a.mark_pending(0, PeerSlot(1));
        assert!(!a.needs_piece(0));
        a.receive_reject(0);
        assert!(a.needs_piece(0));
    }

    #[test]
    fn try_assemble_concatenates() {
        let mut a = MetadataAssembler::new([0; 20]);
        a.set_total_size(5); // tiny for test
        // piece_count = 1 (5 bytes < 16384)
        a.receive_piece(0, Bytes::from_static(b"hello")).unwrap();
        let assembled = a.try_assemble().unwrap();
        assert_eq!(assembled, b"hello");
    }

    #[test]
    fn set_total_size_rejects_oversized() {
        let mut a = MetadataAssembler::new([0; 20]);
        a.set_total_size(MAX_METADATA_SIZE + 1);
        assert!(!a.has_size());
        assert_eq!(a.piece_count(), 0);
    }

    #[test]
    fn set_total_size_ignores_subsequent_calls() {
        let mut a = MetadataAssembler::new([0; 20]);
        a.set_total_size(32768);
        assert_eq!(a.piece_count(), 2);
        // Second call with different size is ignored
        a.set_total_size(16384);
        assert_eq!(a.piece_count(), 2);
        assert_eq!(a.total_size(), Some(32768));
    }

    #[test]
    fn receive_piece_rejects_out_of_range() {
        let mut a = MetadataAssembler::new([0; 20]);
        a.set_total_size(16384); // 1 piece
        assert!(a.receive_piece(1, Bytes::from_static(b"x")).is_err());
    }
}
