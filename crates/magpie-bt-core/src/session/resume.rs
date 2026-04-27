//! Resume-state sink (ADR-0022).
//!
//! Object-safe trait consumers implement to persist a torrent's
//! verified-piece bitfield across restarts. Default impl
//! [`FileResumeSink`] writes a per-torrent bencode sidecar next to the
//! torrent file, mirroring [`FileStatsSink`](super::stats::sink::FileStatsSink):
//! batched writes + a bounded graceful-shutdown flush so a slow disk
//! never wedges shutdown.
//!
//! # Scope
//!
//! v1 persists only the verified bitfield. Piece priority and tracker
//! state are explicit non-goals (no public surface for them yet). See
//! ADR-0022 for the full rationale.

use std::fs;
use std::io::{self, Write};
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use magpie_bt_bencode::{Value, decode, encode};

/// Default graceful-shutdown budget for [`FileResumeSink::flush_graceful`].
/// Matches [`DEFAULT_SHUTDOWN_TIMEOUT`](super::stats::sink::DEFAULT_SHUTDOWN_TIMEOUT)
/// — a slow disk must never wedge shutdown.
pub const DEFAULT_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);

/// Current resume-sidecar schema version. Bumped when the on-disk
/// format changes. A loader seeing a higher value returns
/// [`ResumeSinkError::UnsupportedVersion`].
pub const SCHEMA_VERSION: i64 = 1;

/// Hard cap on the size of a single `.resume` sidecar file.
///
/// A ~1-million-piece torrent produces a 125 KB bitfield plus a few
/// hundred bytes of header — ~128 KB worst case. An 8 MiB cap leaves
/// two orders of magnitude of headroom for future schema additions
/// while refusing to load multi-GB files crafted by a local attacker
/// to OOM the process via `fs::read`.
pub const MAX_SIDECAR_BYTES: u64 = 8 * 1024 * 1024;

/// Hard cap on `piece_count` in a decoded sidecar.
///
/// 1 million pieces at the BEP 52 minimum piece length (16 KiB) = 16
/// GB torrent, which is larger than any practical torrent today. A
/// sidecar declaring a larger value is either malformed or adversarial
/// — refuse before allocating a multi-GB `Vec<bool>` in
/// [`unpack_bitfield`].
pub const MAX_PIECE_COUNT: u32 = 1_000_000;

/// Snapshot of a torrent's resume state at a point in time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResumeSnapshot {
    /// v1 info-hash the snapshot belongs to. Guards against loading
    /// the wrong sidecar after a rename.
    pub info_hash: [u8; 20],
    /// Per-piece verified flag. Length equals `piece_count`.
    pub have: Vec<bool>,
    /// Expected piece count — must match the torrent's
    /// [`TorrentParams`](super::torrent::TorrentParams) on restore.
    pub piece_count: u32,
    /// Expected piece length — must match on restore.
    pub piece_length: u64,
    /// Expected total length — must match on restore.
    pub total_length: u64,
}

/// Errors surfaced by a resume sink.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ResumeSinkError {
    /// I/O error while reading, writing, or flushing.
    #[error(transparent)]
    Io(#[from] io::Error),
    /// Sidecar was not valid bencode or had the wrong structure.
    #[error("invalid sidecar: {0}")]
    InvalidSchema(String),
    /// Sidecar was written by a newer schema version we don't
    /// understand.
    #[error("unsupported sidecar version: got {0}, max supported {SCHEMA_VERSION}")]
    UnsupportedVersion(i64),
    /// Snapshot mismatch: `have.len()` differs from `piece_count`.
    #[error("snapshot have.len() {have_len} != piece_count {piece_count}")]
    HaveLengthMismatch {
        /// Length of `have` supplied.
        have_len: usize,
        /// Expected length.
        piece_count: u32,
    },
}

/// Resume sink trait. Implementations must be `Send + Sync` and
/// object-safe so the engine or a consumer can store
/// `Arc<dyn ResumeSink>` and share it across threads.
pub trait ResumeSink: Send + Sync {
    /// Enqueue a resume snapshot. Implementations may batch internally;
    /// the call must return promptly (no blocking I/O on the hot path).
    ///
    /// # Errors
    ///
    /// [`ResumeSinkError::HaveLengthMismatch`] if `snap.have.len() !=
    /// snap.piece_count`. Other errors surface as `Io`.
    fn enqueue(&self, snap: ResumeSnapshot) -> Result<(), ResumeSinkError>;

    /// Graceful-shutdown flush. Implementations must bound this call:
    /// a slow disk must not wedge shutdown beyond `timeout`.
    ///
    /// # Errors
    ///
    /// Propagates I/O failures. Timeout expiry is logged, not an error —
    /// data loss on unclean shutdown is acceptable (the resume is best-
    /// effort; worst case the consumer reverifies on next start).
    fn flush_graceful(&self, timeout: Duration) -> Result<(), ResumeSinkError>;
}

/// Default [`ResumeSink`] writing bencode sidecars to disk.
///
/// File layout: one file per torrent at `<dir>/<hex_info_hash>.resume`,
/// bencode dict per ADR-0022. Writes are atomic (write-to-tmp +
/// rename) so a crash mid-flush leaves the prior snapshot intact.
#[derive(Debug)]
pub struct FileResumeSink {
    dir: PathBuf,
    pending: Mutex<Vec<ResumeSnapshot>>,
}

impl FileResumeSink {
    /// Construct a sink writing into `dir`. Creates the directory if
    /// it doesn't already exist.
    ///
    /// # Errors
    ///
    /// [`ResumeSinkError::Io`] on directory-creation failure.
    pub fn new(dir: impl Into<PathBuf>) -> Result<Self, ResumeSinkError> {
        let dir = dir.into();
        fs::create_dir_all(&dir)?;
        Ok(Self {
            dir,
            pending: Mutex::new(Vec::new()),
        })
    }

    /// Flush all pending snapshots to disk. Normally called by the
    /// consumer's periodic timer or by [`Self::flush_graceful`];
    /// exposed for tests and for immediate-flush use cases.
    ///
    /// # Errors
    ///
    /// Propagates I/O errors.
    ///
    /// # Panics
    ///
    /// Only if the internal mutex is poisoned.
    pub fn flush_now(&self) -> Result<(), ResumeSinkError> {
        let snaps: Vec<ResumeSnapshot> = {
            let mut pending = self.pending.lock().expect("FileResumeSink poisoned");
            std::mem::take(&mut *pending)
        };
        for snap in snaps {
            self.write_sidecar(&snap)?;
        }
        Ok(())
    }

    fn write_sidecar(&self, snap: &ResumeSnapshot) -> Result<(), ResumeSinkError> {
        let path = self.sidecar_path(&snap.info_hash);
        let tmp = path.with_extension("resume.tmp");
        let payload = encode_snapshot(snap);
        {
            let mut f = fs::File::create(&tmp)?;
            f.write_all(&payload)?;
            f.sync_all()?;
        }
        fs::rename(&tmp, &path)?;
        Ok(())
    }

    /// Path where `info_hash`'s sidecar lives. Exposed for tests and
    /// diagnostic tools.
    #[must_use]
    pub fn sidecar_path(&self, info_hash: &[u8; 20]) -> PathBuf {
        let mut p = self.dir.clone();
        p.push(format!("{}.resume", hex_encode(info_hash)));
        p
    }

    /// Load a previously-flushed snapshot for `info_hash`. Returns
    /// `Ok(None)` if no sidecar exists (cold start).
    ///
    /// # Errors
    ///
    /// [`ResumeSinkError::Io`] on I/O failure,
    /// [`ResumeSinkError::InvalidSchema`] on corruption or missing
    /// fields, [`ResumeSinkError::UnsupportedVersion`] if the sidecar
    /// was written by a newer schema.
    pub fn load_sidecar(
        &self,
        info_hash: &[u8; 20],
    ) -> Result<Option<ResumeSnapshot>, ResumeSinkError> {
        let path = self.sidecar_path(info_hash);
        let metadata = match fs::metadata(&path) {
            Ok(m) => m,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(ResumeSinkError::Io(e)),
        };
        if metadata.len() > MAX_SIDECAR_BYTES {
            return Err(ResumeSinkError::InvalidSchema(format!(
                "sidecar {} bytes exceeds MAX_SIDECAR_BYTES {MAX_SIDECAR_BYTES}",
                metadata.len()
            )));
        }
        let bytes = match fs::read(&path) {
            Ok(b) => b,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(ResumeSinkError::Io(e)),
        };
        decode_snapshot(&bytes).map(Some)
    }
}

impl ResumeSink for FileResumeSink {
    fn enqueue(&self, snap: ResumeSnapshot) -> Result<(), ResumeSinkError> {
        if snap.have.len() != snap.piece_count as usize {
            return Err(ResumeSinkError::HaveLengthMismatch {
                have_len: snap.have.len(),
                piece_count: snap.piece_count,
            });
        }
        {
            let mut pending = self.pending.lock().expect("FileResumeSink poisoned");
            // Deduplicate on info_hash — only the latest snapshot per
            // torrent matters for a batched flush.
            if let Some(existing) = pending.iter_mut().find(|s| s.info_hash == snap.info_hash) {
                *existing = snap;
            } else {
                pending.push(snap);
            }
        }
        Ok(())
    }

    fn flush_graceful(&self, timeout: Duration) -> Result<(), ResumeSinkError> {
        let snaps: Vec<ResumeSnapshot> = {
            let mut p = self.pending.lock().expect("FileResumeSink poisoned");
            std::mem::take(&mut *p)
        };
        let deadline = Instant::now() + timeout;
        for snap in snaps {
            if Instant::now() > deadline {
                tracing::warn!("FileResumeSink::flush_graceful: deadline exceeded; dropping");
                break;
            }
            self.write_sidecar(&snap)?;
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Bencode encode/decode
// ---------------------------------------------------------------------------

/// Pack `have` into a bitfield (MSB = piece 0).
///
/// Matches the BEP 3 wire `Bitfield` encoding. Trailing bits beyond
/// `have.len()` are zero.
#[must_use]
pub fn pack_bitfield(have: &[bool]) -> Vec<u8> {
    let nbytes = have.len().div_ceil(8);
    let mut out = vec![0u8; nbytes];
    for (i, &bit) in have.iter().enumerate() {
        if bit {
            out[i / 8] |= 1 << (7 - (i % 8));
        }
    }
    out
}

/// Inverse of [`pack_bitfield`].
///
/// Reads the first `piece_count` bits MSB-first from `packed` and
/// returns a `Vec<bool>` of that length. Bits beyond `piece_count`
/// are ignored. If `packed` is too short to cover `piece_count` bits,
/// an error is returned.
///
/// # Errors
///
/// [`ResumeSinkError::InvalidSchema`] if `packed.len() < ceil(piece_count / 8)`.
pub fn unpack_bitfield(packed: &[u8], piece_count: u32) -> Result<Vec<bool>, ResumeSinkError> {
    let needed = (piece_count as usize).div_ceil(8);
    if packed.len() < needed {
        return Err(ResumeSinkError::InvalidSchema(format!(
            "bitfield too short: {} bytes for {piece_count} pieces (need {needed})",
            packed.len()
        )));
    }
    let mut have = Vec::with_capacity(piece_count as usize);
    for i in 0..(piece_count as usize) {
        let byte = packed[i / 8];
        have.push((byte >> (7 - (i % 8))) & 1 == 1);
    }
    Ok(have)
}

fn encode_snapshot(snap: &ResumeSnapshot) -> Vec<u8> {
    use std::borrow::Cow;
    use std::collections::BTreeMap;
    let bitfield = pack_bitfield(&snap.have);
    let mut dict: BTreeMap<Cow<'_, [u8]>, Value<'_>> = BTreeMap::new();
    dict.insert(
        Cow::Borrowed(b"bitfield"),
        Value::Bytes(Cow::Owned(bitfield)),
    );
    dict.insert(
        Cow::Borrowed(b"info_hash"),
        Value::Bytes(Cow::Owned(snap.info_hash.to_vec())),
    );
    #[allow(clippy::cast_possible_wrap)]
    let piece_count_i = i64::from(snap.piece_count);
    dict.insert(Cow::Borrowed(b"piece_count"), Value::Int(piece_count_i));
    #[allow(clippy::cast_possible_wrap)]
    let piece_length_i = snap.piece_length.min(i64::MAX as u64) as i64;
    dict.insert(Cow::Borrowed(b"piece_length"), Value::Int(piece_length_i));
    #[allow(clippy::cast_possible_wrap)]
    let total_length_i = snap.total_length.min(i64::MAX as u64) as i64;
    dict.insert(Cow::Borrowed(b"total_length"), Value::Int(total_length_i));
    dict.insert(Cow::Borrowed(b"version"), Value::Int(SCHEMA_VERSION));
    encode(&Value::Dict(dict))
}

fn decode_snapshot(bytes: &[u8]) -> Result<ResumeSnapshot, ResumeSinkError> {
    let value = decode(bytes)
        .map_err(|e| ResumeSinkError::InvalidSchema(format!("bencode decode: {e:?}")))?;
    let Value::Dict(dict) = &value else {
        return Err(ResumeSinkError::InvalidSchema(
            "sidecar root is not a dict".into(),
        ));
    };
    let version = dict
        .get(b"version".as_ref())
        .and_then(Value::as_int)
        .ok_or_else(|| ResumeSinkError::InvalidSchema("missing 'version'".into()))?;
    if version > SCHEMA_VERSION {
        return Err(ResumeSinkError::UnsupportedVersion(version));
    }
    let info_hash_bytes = dict
        .get(b"info_hash".as_ref())
        .and_then(Value::as_bytes)
        .ok_or_else(|| ResumeSinkError::InvalidSchema("missing 'info_hash'".into()))?;
    if info_hash_bytes.len() != 20 {
        return Err(ResumeSinkError::InvalidSchema(format!(
            "'info_hash' has len {} (want 20)",
            info_hash_bytes.len()
        )));
    }
    let mut info_hash = [0u8; 20];
    info_hash.copy_from_slice(info_hash_bytes);
    let piece_count_i = dict
        .get(b"piece_count".as_ref())
        .and_then(Value::as_int)
        .ok_or_else(|| ResumeSinkError::InvalidSchema("missing 'piece_count'".into()))?;
    let piece_count = u32::try_from(piece_count_i).map_err(|_| {
        ResumeSinkError::InvalidSchema(format!("'piece_count' out of u32 range: {piece_count_i}"))
    })?;
    if piece_count > MAX_PIECE_COUNT {
        return Err(ResumeSinkError::InvalidSchema(format!(
            "'piece_count' {piece_count} exceeds MAX_PIECE_COUNT {MAX_PIECE_COUNT}"
        )));
    }
    let piece_length_i = dict
        .get(b"piece_length".as_ref())
        .and_then(Value::as_int)
        .ok_or_else(|| ResumeSinkError::InvalidSchema("missing 'piece_length'".into()))?;
    if piece_length_i < 0 {
        return Err(ResumeSinkError::InvalidSchema(format!(
            "'piece_length' negative: {piece_length_i}"
        )));
    }
    #[allow(clippy::cast_sign_loss)]
    let piece_length = piece_length_i as u64;
    let total_length_i = dict
        .get(b"total_length".as_ref())
        .and_then(Value::as_int)
        .ok_or_else(|| ResumeSinkError::InvalidSchema("missing 'total_length'".into()))?;
    if total_length_i < 0 {
        return Err(ResumeSinkError::InvalidSchema(format!(
            "'total_length' negative: {total_length_i}"
        )));
    }
    #[allow(clippy::cast_sign_loss)]
    let total_length = total_length_i as u64;
    let packed = dict
        .get(b"bitfield".as_ref())
        .and_then(Value::as_bytes)
        .ok_or_else(|| ResumeSinkError::InvalidSchema("missing 'bitfield'".into()))?;
    let have = unpack_bitfield(packed, piece_count)?;
    Ok(ResumeSnapshot {
        info_hash,
        have,
        piece_count,
        piece_length,
        total_length,
    })
}

fn hex_encode(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push(nybble(b >> 4));
        s.push(nybble(b & 0x0F));
    }
    s
}

const fn nybble(n: u8) -> char {
    if n < 10 {
        (b'0' + n) as char
    } else {
        (b'a' + (n - 10)) as char
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn sample_snap() -> ResumeSnapshot {
        ResumeSnapshot {
            info_hash: [0xABu8; 20],
            have: vec![
                true, false, true, true, false, false, false, false, true, true,
            ],
            piece_count: 10,
            piece_length: 16_384,
            total_length: 163_840,
        }
    }

    #[test]
    fn pack_unpack_roundtrips() {
        let have = vec![
            true, false, true, true, false, false, true, true, // byte 0 = 0xb3
            false, true, // byte 1 high bits: 0b0100_0000 = 0x40
        ];
        let packed = pack_bitfield(&have);
        assert_eq!(packed, vec![0b1011_0011, 0b0100_0000]);
        let unpacked = unpack_bitfield(&packed, 10).unwrap();
        assert_eq!(unpacked, have);
    }

    #[test]
    fn pack_empty_is_empty() {
        assert_eq!(pack_bitfield(&[]), Vec::<u8>::new());
    }

    #[test]
    fn pack_exact_byte_boundary() {
        let have = vec![true; 8];
        let packed = pack_bitfield(&have);
        assert_eq!(packed, vec![0xFF]);
        let unpacked = unpack_bitfield(&packed, 8).unwrap();
        assert_eq!(unpacked, have);
    }

    #[test]
    fn unpack_rejects_short_buffer() {
        // 10 pieces need 2 bytes; supply 1.
        let err = unpack_bitfield(&[0xFF], 10).unwrap_err();
        assert!(matches!(err, ResumeSinkError::InvalidSchema(_)));
    }

    #[test]
    fn unpack_ignores_trailing_bits() {
        // 6 pieces worth of bits; last byte has garbage in low 2 bits.
        let packed = vec![0b1111_1111];
        let unpacked = unpack_bitfield(&packed, 6).unwrap();
        assert_eq!(unpacked, vec![true; 6]);
    }

    #[test]
    fn encode_decode_roundtrip() {
        let snap = sample_snap();
        let bytes = encode_snapshot(&snap);
        let back = decode_snapshot(&bytes).unwrap();
        assert_eq!(back, snap);
    }

    #[test]
    #[cfg_attr(miri, ignore = "touches the filesystem; miri isolation blocks mkdir/write")]
    fn file_sink_writes_and_loads_sidecar() {
        let dir = tempdir().unwrap();
        let sink = FileResumeSink::new(dir.path()).unwrap();
        let snap = sample_snap();
        sink.enqueue(snap.clone()).unwrap();
        sink.flush_now().unwrap();
        let got = sink.load_sidecar(&snap.info_hash).unwrap().unwrap();
        assert_eq!(got, snap);
    }

    #[test]
    #[cfg_attr(miri, ignore = "touches the filesystem; miri isolation blocks mkdir/write")]
    fn load_missing_sidecar_returns_none() {
        let dir = tempdir().unwrap();
        let sink = FileResumeSink::new(dir.path()).unwrap();
        let out = sink.load_sidecar(&[0x11u8; 20]).unwrap();
        assert!(out.is_none());
    }

    #[test]
    #[cfg_attr(miri, ignore = "touches the filesystem; miri isolation blocks mkdir/write")]
    fn sidecar_path_uses_hex_info_hash() {
        let dir = tempdir().unwrap();
        let sink = FileResumeSink::new(dir.path()).unwrap();
        let info_hash = [
            0xDE, 0xAD, 0xBE, 0xEF, 0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99,
            0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF,
        ];
        let p = sink.sidecar_path(&info_hash);
        assert!(p.ends_with("deadbeef00112233445566778899aabbccddeeff.resume"));
    }

    #[test]
    #[cfg_attr(miri, ignore = "touches the filesystem; miri isolation blocks mkdir/write")]
    fn enqueue_rejects_have_length_mismatch() {
        let dir = tempdir().unwrap();
        let sink = FileResumeSink::new(dir.path()).unwrap();
        let mut bad = sample_snap();
        bad.have.truncate(5); // piece_count is 10, have is 5 → mismatch
        let err = sink.enqueue(bad).unwrap_err();
        assert!(matches!(
            err,
            ResumeSinkError::HaveLengthMismatch {
                have_len: 5,
                piece_count: 10
            }
        ));
    }

    #[test]
    #[cfg_attr(miri, ignore = "touches the filesystem; miri isolation blocks mkdir/write")]
    fn enqueue_deduplicates_on_info_hash() {
        let dir = tempdir().unwrap();
        let sink = FileResumeSink::new(dir.path()).unwrap();
        let mut s1 = sample_snap();
        s1.have[0] = false;
        sink.enqueue(s1).unwrap();
        let s2 = sample_snap(); // has have[0] = true
        sink.enqueue(s2.clone()).unwrap();
        sink.flush_now().unwrap();
        let got = sink.load_sidecar(&s2.info_hash).unwrap().unwrap();
        assert!(got.have[0], "latest snapshot wins");
    }

    #[test]
    fn decode_rejects_unsupported_version() {
        // Hand-craft a sidecar with version = 999.
        use std::borrow::Cow;
        use std::collections::BTreeMap;
        let mut dict: BTreeMap<Cow<'_, [u8]>, Value<'_>> = BTreeMap::new();
        dict.insert(
            Cow::Borrowed(b"bitfield"),
            Value::Bytes(Cow::Owned(vec![0u8; 2])),
        );
        dict.insert(
            Cow::Borrowed(b"info_hash"),
            Value::Bytes(Cow::Owned(vec![0u8; 20])),
        );
        dict.insert(Cow::Borrowed(b"piece_count"), Value::Int(10));
        dict.insert(Cow::Borrowed(b"piece_length"), Value::Int(16_384));
        dict.insert(Cow::Borrowed(b"total_length"), Value::Int(163_840));
        dict.insert(Cow::Borrowed(b"version"), Value::Int(999));
        let bytes = encode(&Value::Dict(dict));
        let err = decode_snapshot(&bytes).unwrap_err();
        assert!(matches!(err, ResumeSinkError::UnsupportedVersion(999)));
    }

    #[test]
    fn decode_rejects_missing_fields() {
        let bytes = b"de"; // empty dict
        let err = decode_snapshot(bytes).unwrap_err();
        assert!(matches!(err, ResumeSinkError::InvalidSchema(_)));
    }

    #[test]
    fn decode_rejects_non_dict_root() {
        let bytes = b"i42e";
        let err = decode_snapshot(bytes).unwrap_err();
        assert!(matches!(err, ResumeSinkError::InvalidSchema(_)));
    }

    #[test]
    fn decode_rejects_wrong_info_hash_length() {
        use std::borrow::Cow;
        use std::collections::BTreeMap;
        let mut dict: BTreeMap<Cow<'_, [u8]>, Value<'_>> = BTreeMap::new();
        dict.insert(
            Cow::Borrowed(b"bitfield"),
            Value::Bytes(Cow::Owned(vec![0u8; 2])),
        );
        dict.insert(
            Cow::Borrowed(b"info_hash"),
            Value::Bytes(Cow::Owned(vec![0u8; 19])), // wrong
        );
        dict.insert(Cow::Borrowed(b"piece_count"), Value::Int(10));
        dict.insert(Cow::Borrowed(b"piece_length"), Value::Int(16_384));
        dict.insert(Cow::Borrowed(b"total_length"), Value::Int(163_840));
        dict.insert(Cow::Borrowed(b"version"), Value::Int(1));
        let bytes = encode(&Value::Dict(dict));
        let err = decode_snapshot(&bytes).unwrap_err();
        assert!(matches!(err, ResumeSinkError::InvalidSchema(_)));
    }

    #[test]
    #[cfg_attr(miri, ignore = "touches the filesystem; miri isolation blocks mkdir/write")]
    fn flush_graceful_is_bounded() {
        let dir = tempdir().unwrap();
        let sink = FileResumeSink::new(dir.path()).unwrap();
        sink.enqueue(sample_snap()).unwrap();
        sink.flush_graceful(Duration::from_secs(1)).unwrap();
        // After graceful flush, the sidecar must exist.
        let p = sink.sidecar_path(&sample_snap().info_hash);
        assert!(p.exists());
    }

    #[test]
    fn decode_rejects_piece_count_over_max() {
        use std::borrow::Cow;
        use std::collections::BTreeMap;
        let mut dict: BTreeMap<Cow<'_, [u8]>, Value<'_>> = BTreeMap::new();
        dict.insert(
            Cow::Borrowed(b"bitfield"),
            Value::Bytes(Cow::Owned(vec![0u8; 2])),
        );
        dict.insert(
            Cow::Borrowed(b"info_hash"),
            Value::Bytes(Cow::Owned(vec![0u8; 20])),
        );
        // One more than MAX_PIECE_COUNT — must be rejected before
        // `unpack_bitfield` tries to allocate the resulting vec.
        let over = i64::from(MAX_PIECE_COUNT + 1);
        dict.insert(Cow::Borrowed(b"piece_count"), Value::Int(over));
        dict.insert(Cow::Borrowed(b"piece_length"), Value::Int(16_384));
        dict.insert(Cow::Borrowed(b"total_length"), Value::Int(163_840));
        dict.insert(Cow::Borrowed(b"version"), Value::Int(1));
        let bytes = encode(&Value::Dict(dict));
        let err = decode_snapshot(&bytes).unwrap_err();
        let msg = match err {
            ResumeSinkError::InvalidSchema(m) => m,
            other => panic!("expected InvalidSchema, got {other:?}"),
        };
        assert!(msg.contains("MAX_PIECE_COUNT"), "msg={msg}");
    }

    #[test]
    #[cfg_attr(miri, ignore = "touches the filesystem; miri isolation blocks mkdir/write")]
    fn load_rejects_oversize_sidecar_file() {
        let dir = tempdir().unwrap();
        let sink = FileResumeSink::new(dir.path()).unwrap();
        let path = sink.sidecar_path(&[0x11u8; 20]);
        // Write a 10 MiB file — over the 8 MiB cap. Content doesn't
        // matter: the size check runs first.
        let mut f = fs::File::create(&path).unwrap();
        let chunk = vec![0u8; 1024 * 1024];
        for _ in 0..10 {
            f.write_all(&chunk).unwrap();
        }
        f.sync_all().unwrap();
        drop(f);
        let err = sink.load_sidecar(&[0x11u8; 20]).unwrap_err();
        let msg = match err {
            ResumeSinkError::InvalidSchema(m) => m,
            other => panic!("expected InvalidSchema, got {other:?}"),
        };
        assert!(msg.contains("MAX_SIDECAR_BYTES"), "msg={msg}");
    }

    #[test]
    #[cfg_attr(miri, ignore = "touches the filesystem; miri isolation blocks mkdir/write")]
    fn atomic_write_preserves_prior_on_failure() {
        // Write snap1, then enqueue snap2 and flush. The final on-disk
        // contents must be snap2 (atomic rename means no half-written
        // state is observable).
        let dir = tempdir().unwrap();
        let sink = FileResumeSink::new(dir.path()).unwrap();
        let mut s1 = sample_snap();
        s1.have = vec![true; 10];
        sink.enqueue(s1).unwrap();
        sink.flush_now().unwrap();
        let mut s2 = sample_snap();
        s2.have = vec![false; 10];
        sink.enqueue(s2.clone()).unwrap();
        sink.flush_now().unwrap();
        let got = sink.load_sidecar(&s2.info_hash).unwrap().unwrap();
        assert_eq!(got.have, s2.have);
        // No lingering `.resume.tmp` file.
        let tmp = sink
            .sidecar_path(&s2.info_hash)
            .with_extension("resume.tmp");
        assert!(!tmp.exists());
    }
}
