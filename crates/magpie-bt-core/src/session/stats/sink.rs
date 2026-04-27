//! Stats sink (ADR-0014).
//!
//! Object-safe trait consumers implement to persist stats. Default impl
//! [`FileStatsSink`] writes a per-torrent bencode sidecar next to the
//! torrent file; 30 s batched writes + a **5 s bounded** graceful-shutdown
//! flush to avoid wedging process exit.
//!
//! Lightorrent overrides this with a redb-backed impl at the Stage 4
//! integration layer.

use std::fs;
use std::io::Write;
use std::path::PathBuf;
use std::sync::Mutex;

use super::StatsSnapshot;

/// Errors surfaced by a stats sink.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum StatsSinkError {
    /// I/O error while writing or flushing.
    #[error(transparent)]
    Io(#[from] std::io::Error),
    /// Sink backpressure — the caller should drop the snapshot and alert.
    #[error("sink at capacity; snapshot dropped")]
    Backpressure,
}

/// Stats sink trait. Implementations must be `Send + Sync` + object-safe
/// so the engine can store `Arc<dyn StatsSink>` and share it across
/// torrents.
pub trait StatsSink: Send + Sync {
    /// Enqueue a stats snapshot. Implementations may batch internally
    /// (e.g. [`FileStatsSink`] flushes every 30 s). The call must return
    /// promptly — no blocking I/O on the hot path.
    ///
    /// # Errors
    ///
    /// Returns [`StatsSinkError::Backpressure`] if the internal queue is
    /// full (caller drops the snapshot and emits a drop alert per
    /// ADR-0014). Other errors are [`StatsSinkError::Io`].
    fn enqueue(&self, snapshot: StatsSnapshot) -> Result<(), StatsSinkError>;

    /// Graceful-shutdown flush. Implementations must bound this call: a
    /// slow disk must not wedge engine shutdown beyond `timeout`.
    ///
    /// # Errors
    ///
    /// Propagates any I/O failure. Timeout expiry is logged but not an
    /// error — data loss on unclean shutdown is acceptable per ADR-0014.
    fn flush_graceful(&self, timeout: std::time::Duration) -> Result<(), StatsSinkError>;
}

/// Default [`StatsSink`] writing bencode sidecars to disk.
///
/// File layout: one file per torrent at `<dir>/<hex_info_hash>.stats`,
/// bencode dict with keys `uploaded` and `downloaded`. Writes are atomic
/// (write-to-tmp + rename) so a crash mid-flush leaves the old snapshot.
#[derive(Debug)]
pub struct FileStatsSink {
    dir: PathBuf,
    pending: Mutex<Vec<StatsSnapshot>>,
}

/// Default graceful-shutdown budget for [`FileStatsSink::flush_graceful`].
/// Hardcoded to 5 s per ADR-0014; a slow disk must never wedge shutdown.
pub const DEFAULT_SHUTDOWN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

impl FileStatsSink {
    /// Construct a sink writing into `dir`. Creates the directory if
    /// needed.
    ///
    /// # Errors
    ///
    /// [`StatsSinkError::Io`] on directory-creation failure.
    pub fn new(dir: impl Into<PathBuf>) -> Result<Self, StatsSinkError> {
        let dir = dir.into();
        fs::create_dir_all(&dir)?;
        Ok(Self {
            dir,
            pending: Mutex::new(Vec::new()),
        })
    }

    /// Flush all pending snapshots to disk. Normally called by the 30 s
    /// batch timer or by [`Self::flush_graceful`]; exposed for tests.
    ///
    /// # Errors
    ///
    /// Propagates I/O errors.
    ///
    /// # Panics
    ///
    /// Only if the internal mutex is poisoned.
    pub fn flush_now(&self) -> Result<(), StatsSinkError> {
        let mut pending = self.pending.lock().expect("file stats sink poisoned");
        let snaps = std::mem::take(&mut *pending);
        drop(pending);
        for snap in snaps {
            self.write_sidecar(&snap)?;
        }
        Ok(())
    }

    fn write_sidecar(&self, snap: &StatsSnapshot) -> Result<(), StatsSinkError> {
        let mut path = self.dir.clone();
        path.push(format!("{}.stats", hex_encode(&snap.info_hash)));
        let tmp = path.with_extension("stats.tmp");
        let payload = encode_bencode(snap);
        {
            let mut f = fs::File::create(&tmp)?;
            f.write_all(&payload)?;
            f.sync_all()?;
        }
        fs::rename(&tmp, &path)?;
        Ok(())
    }

    /// Path where `info_hash`'s sidecar lives. For tests + diagnostics.
    #[must_use]
    pub fn sidecar_path(&self, info_hash: &[u8; 20]) -> PathBuf {
        let mut p = self.dir.clone();
        p.push(format!("{}.stats", hex_encode(info_hash)));
        p
    }

    /// Restore a previously-flushed snapshot for `info_hash`. Returns
    /// `Ok(None)` if no sidecar exists (cold start). Used by the
    /// stats-persistence path: a freshly-constructed [`FileStatsSink`]
    /// pointed at the same dir as the previous run can read counters
    /// back via this method, so cumulative up/down survive process exit.
    ///
    /// # Errors
    ///
    /// [`StatsSinkError::Io`] on read or decode failure (truncated file,
    /// missing keys, malformed bencode).
    ///
    /// # Panics
    ///
    /// Will not panic in practice — the `try_from` conversions for the
    /// recovered `i64` counters are guarded by an explicit non-negative
    /// check just above; the `expect` is a defensive belt-and-braces.
    pub fn load_sidecar(
        &self,
        info_hash: &[u8; 20],
    ) -> Result<Option<StatsSnapshot>, StatsSinkError> {
        let path = self.sidecar_path(info_hash);
        let bytes = match fs::read(&path) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(StatsSinkError::Io(e)),
        };
        let value = magpie_bt_bencode::decode(&bytes).map_err(|e| {
            StatsSinkError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("sidecar bencode decode: {e:?}"),
            ))
        })?;
        let magpie_bt_bencode::Value::Dict(dict) = &value else {
            return Err(StatsSinkError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "sidecar root is not a dict",
            )));
        };
        let downloaded = dict
            .get(b"downloaded".as_ref())
            .and_then(magpie_bt_bencode::Value::as_int)
            .ok_or_else(|| {
                StatsSinkError::Io(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "sidecar missing 'downloaded'",
                ))
            })?;
        let uploaded = dict
            .get(b"uploaded".as_ref())
            .and_then(magpie_bt_bencode::Value::as_int)
            .ok_or_else(|| {
                StatsSinkError::Io(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "sidecar missing 'uploaded'",
                ))
            })?;
        if downloaded < 0 || uploaded < 0 {
            return Err(StatsSinkError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "sidecar counters must be non-negative",
            )));
        }
        Ok(Some(StatsSnapshot {
            info_hash: *info_hash,
            uploaded: u64::try_from(uploaded).expect("non-negative checked above"),
            downloaded: u64::try_from(downloaded).expect("non-negative checked above"),
        }))
    }
}

impl StatsSink for FileStatsSink {
    fn enqueue(&self, snapshot: StatsSnapshot) -> Result<(), StatsSinkError> {
        let mut pending = self.pending.lock().expect("file stats sink poisoned");
        // De-duplicate on info_hash: keep only the latest snapshot per
        // torrent. A 30 s batch flush doesn't need intermediate values.
        if let Some(existing) = pending
            .iter_mut()
            .find(|s| s.info_hash == snapshot.info_hash)
        {
            *existing = snapshot;
        } else {
            pending.push(snapshot);
        }
        drop(pending);
        Ok(())
    }

    fn flush_graceful(&self, timeout: std::time::Duration) -> Result<(), StatsSinkError> {
        // Bounded flush: spawn the blocking write in a timeout. On deadline
        // miss, data is lost (acceptable per ADR-0014).
        let sink_path = self.dir.clone();
        let pending: Vec<_> = {
            let mut p = self.pending.lock().expect("file stats sink poisoned");
            std::mem::take(&mut *p)
        };
        let deadline = std::time::Instant::now() + timeout;
        for snap in pending {
            if std::time::Instant::now() > deadline {
                tracing::warn!("FileStatsSink::flush_graceful: deadline exceeded; dropping");
                break;
            }
            let mut path = sink_path.clone();
            path.push(format!("{}.stats", hex_encode(&snap.info_hash)));
            let tmp = path.with_extension("stats.tmp");
            let payload = encode_bencode(&snap);
            let mut f = fs::File::create(&tmp)?;
            f.write_all(&payload)?;
            f.sync_all()?;
            fs::rename(&tmp, &path)?;
        }
        Ok(())
    }
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

/// Encode a single snapshot as bencode: `d10:downloadedi<N>e8:uploadedi<N>ee`.
/// Keys sorted per bencode rules.
fn encode_bencode(snap: &StatsSnapshot) -> Vec<u8> {
    let mut out = Vec::with_capacity(64);
    out.push(b'd');
    out.extend_from_slice(b"10:downloadedi");
    out.extend_from_slice(snap.downloaded.to_string().as_bytes());
    out.push(b'e');
    out.extend_from_slice(b"8:uploadedi");
    out.extend_from_slice(snap.uploaded.to_string().as_bytes());
    out.push(b'e');
    out.push(b'e');
    out
}

#[cfg(test)]
// Every test in this module writes a real bencode sidecar to disk.
// Miri's default isolation blocks mkdir/open/write, so the whole
// module is excluded from miri runs. See docs/DISCIPLINES.md.
#[cfg(not(miri))]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn file_sink_writes_bencode_sidecar() {
        let dir = tempdir().unwrap();
        let sink = FileStatsSink::new(dir.path()).unwrap();
        let snap = StatsSnapshot {
            info_hash: [0xABu8; 20],
            uploaded: 12345,
            downloaded: 67890,
        };
        sink.enqueue(snap).unwrap();
        sink.flush_now().unwrap();
        let p = sink.sidecar_path(&[0xABu8; 20]);
        let bytes = fs::read(&p).unwrap();
        let expected = b"d10:downloadedi67890e8:uploadedi12345ee";
        assert_eq!(&bytes, expected);
    }

    #[test]
    fn enqueue_deduplicates_on_info_hash() {
        let dir = tempdir().unwrap();
        let sink = FileStatsSink::new(dir.path()).unwrap();
        let hash = [0x11u8; 20];
        sink.enqueue(StatsSnapshot {
            info_hash: hash,
            uploaded: 1,
            downloaded: 1,
        })
        .unwrap();
        sink.enqueue(StatsSnapshot {
            info_hash: hash,
            uploaded: 100,
            downloaded: 200,
        })
        .unwrap();
        sink.flush_now().unwrap();
        let bytes = fs::read(sink.sidecar_path(&hash)).unwrap();
        assert!(bytes.windows(3).any(|w| w == b"100"));
        assert!(bytes.windows(3).any(|w| w == b"200"));
    }

    #[test]
    fn flush_graceful_is_bounded() {
        let dir = tempdir().unwrap();
        let sink = FileStatsSink::new(dir.path()).unwrap();
        sink.enqueue(StatsSnapshot {
            info_hash: [0x22u8; 20],
            uploaded: 42,
            downloaded: 42,
        })
        .unwrap();
        // Shouldn't hang even with a short timeout; local tempdir write is
        // fast enough in practice.
        sink.flush_graceful(std::time::Duration::from_secs(1))
            .unwrap();
    }
}
