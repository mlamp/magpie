//! Persistent DHT contact cache (ADR-0025).
//!
//! Save up to 64 good nodes on shutdown; load them on the next
//! cold-start so bootstrap has a warm head-start before the DNS
//! hostnames resolve. Schema is a tiny bencode sidecar with the
//! same atomic-write pattern as `FileResumeSink` (ADR-0022).
//!
//! v1 scope: IPv4 only. IPv6 contacts are silently skipped on save
//! and error on load — matches the DHT's IPv4-only bias throughout
//! M4. Extending is a schema bump.

use std::fs;
use std::io;
use std::net::{SocketAddr, SocketAddrV4};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use thiserror::Error;

use magpie_bt_bencode::{Value, decode, encode};

use crate::krpc::{COMPACT_NODE_V4_LEN, CompactNode};
use crate::node_id::NodeId;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Cache schema version. Bumped on on-disk format changes.
pub const CACHE_SCHEMA_VERSION: i64 = 1;

/// Maximum contacts persisted. Matches ADR-0025.
pub const CACHE_MAX_CONTACTS: usize = 64;

/// Hard cap on sidecar file size. 64 contacts × (20-byte id +
/// 6-byte v4 + overhead) ≈ 2 KB — 1 MiB leaves three orders of
/// magnitude of headroom while refusing multi-GB crafted files.
pub const CACHE_MAX_FILE_BYTES: u64 = 1024 * 1024;

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Failures produced by [`FileContactCache`].
#[derive(Debug, Error)]
pub enum CacheError {
    /// Filesystem I/O failure.
    #[error(transparent)]
    Io(#[from] io::Error),
    /// File exceeded [`CACHE_MAX_FILE_BYTES`].
    #[error("cache file oversize: {0} bytes > {CACHE_MAX_FILE_BYTES}")]
    Oversize(u64),
    /// Bencode decode failed or the payload shape is wrong.
    #[error("cache decode error: {0}")]
    Decode(String),
    /// Schema version beyond what we understand.
    #[error("unsupported cache version {0}; expected {CACHE_SCHEMA_VERSION}")]
    UnsupportedVersion(i64),
}

// ---------------------------------------------------------------------------
// Contact + cache API
// ---------------------------------------------------------------------------

/// A persisted (id, addr) cache entry.
///
/// v1 is v4-only; `addr` is always a [`SocketAddrV4`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CachedContact {
    /// Node id.
    pub id: NodeId,
    /// Node UDP address.
    pub addr: SocketAddrV4,
}

/// Bencode-sidecar contact cache.
#[derive(Debug, Clone)]
pub struct FileContactCache {
    path: PathBuf,
}

impl FileContactCache {
    /// Create a handle anchored at `path`. The path is lazy — it's
    /// neither read nor created until [`Self::load`] or
    /// [`Self::save`] is called.
    #[must_use]
    pub const fn new(path: PathBuf) -> Self {
        Self { path }
    }

    /// Absolute path of the sidecar file.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Load cached contacts.
    ///
    /// Returns an empty vec when the file does not exist — cold
    /// start is not an error.
    ///
    /// # Errors
    ///
    /// [`CacheError::Io`] for filesystem failures; [`CacheError::Oversize`]
    /// when the file exceeds [`CACHE_MAX_FILE_BYTES`];
    /// [`CacheError::Decode`] on malformed bencode; or
    /// [`CacheError::UnsupportedVersion`] for a schema we don't
    /// understand.
    pub fn load(&self) -> Result<Vec<CachedContact>, CacheError> {
        let meta = match fs::metadata(&self.path) {
            Ok(m) => m,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(e.into()),
        };
        if meta.len() > CACHE_MAX_FILE_BYTES {
            return Err(CacheError::Oversize(meta.len()));
        }
        let bytes = fs::read(&self.path)?;
        decode_cache(&bytes)
    }

    /// Persist `contacts` atomically (write-to-tmp + rename).
    /// Truncated to [`CACHE_MAX_CONTACTS`] at the head.
    ///
    /// # Errors
    ///
    /// [`CacheError::Io`] for filesystem failures.
    pub fn save(&self, contacts: &[CachedContact]) -> Result<(), CacheError> {
        let bytes = encode_cache(contacts);
        let tmp = self.path.with_extension("tmp");
        fs::write(&tmp, &bytes)?;
        fs::rename(&tmp, &self.path)?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Encode / decode
// ---------------------------------------------------------------------------

fn encode_cache(contacts: &[CachedContact]) -> Vec<u8> {
    use std::borrow::Cow;
    use std::collections::BTreeMap;

    let mut root: BTreeMap<Cow<'_, [u8]>, Value<'_>> = BTreeMap::new();
    root.insert(Cow::Borrowed(b"v"), Value::Int(CACHE_SCHEMA_VERSION));

    let saved_at_secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    root.insert(
        Cow::Borrowed(b"saved_at"),
        Value::Int(i64::try_from(saved_at_secs).unwrap_or(i64::MAX)),
    );

    let mut compact =
        Vec::with_capacity(contacts.len().min(CACHE_MAX_CONTACTS) * COMPACT_NODE_V4_LEN);
    for c in contacts.iter().take(CACHE_MAX_CONTACTS) {
        let node = CompactNode {
            id: c.id,
            addr: c.addr,
        };
        compact.extend_from_slice(&node.to_bytes());
    }
    root.insert(Cow::Borrowed(b"nodes"), Value::Bytes(Cow::Owned(compact)));

    encode(&Value::Dict(root))
}

fn decode_cache(data: &[u8]) -> Result<Vec<CachedContact>, CacheError> {
    let val = decode(data).map_err(|e| CacheError::Decode(e.to_string()))?;
    let dict = val
        .as_dict()
        .ok_or_else(|| CacheError::Decode("top-level is not a dict".into()))?;

    let version = dict
        .get(b"v".as_slice())
        .and_then(Value::as_int)
        .ok_or_else(|| CacheError::Decode("missing or non-int `v`".into()))?;
    if version != CACHE_SCHEMA_VERSION {
        return Err(CacheError::UnsupportedVersion(version));
    }

    let nodes_bytes = dict
        .get(b"nodes".as_slice())
        .and_then(Value::as_bytes)
        .ok_or_else(|| CacheError::Decode("missing or non-bytes `nodes`".into()))?;
    if !nodes_bytes.len().is_multiple_of(COMPACT_NODE_V4_LEN) {
        return Err(CacheError::Decode(format!(
            "nodes length {} not a multiple of {COMPACT_NODE_V4_LEN}",
            nodes_bytes.len()
        )));
    }

    let mut out = Vec::with_capacity(nodes_bytes.len() / COMPACT_NODE_V4_LEN);
    for chunk in nodes_bytes.chunks_exact(COMPACT_NODE_V4_LEN) {
        let Some(node) = CompactNode::from_bytes(chunk) else {
            return Err(CacheError::Decode("malformed compact-node block".into()));
        };
        out.push(CachedContact {
            id: node.id,
            addr: node.addr,
        });
        if out.len() >= CACHE_MAX_CONTACTS {
            break;
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Saved-at introspection
// ---------------------------------------------------------------------------

/// Age of a cache file since its last save, via its `saved_at`
/// field. Returns `None` if the file is missing, unreadable, or
/// malformed.
#[must_use]
pub fn cache_age(path: &Path) -> Option<Duration> {
    let bytes = fs::read(path).ok()?;
    let val = decode(&bytes).ok()?;
    let saved_at_secs = val.as_dict()?.get(b"saved_at".as_slice())?.as_int()?;
    let now_secs = SystemTime::now().duration_since(UNIX_EPOCH).ok()?.as_secs();
    let saved_at_secs = u64::try_from(saved_at_secs).ok()?;
    now_secs.checked_sub(saved_at_secs).map(Duration::from_secs)
}

// Silence unused-field warning on Ipv4Addr when cache is v4-only
// and callers can convert to the general `SocketAddr`.
impl CachedContact {
    /// Convert to a generic [`SocketAddr`].
    #[must_use]
    pub const fn socket_addr(&self) -> SocketAddr {
        SocketAddr::V4(self.addr)
    }
}

// Also let callers build a CachedContact from any SocketAddr and
// skip v6 at their own layer.
impl CachedContact {
    /// Build from an `(id, addr)` pair, dropping v6 (returns `None`).
    #[must_use]
    pub const fn from_socket(id: NodeId, addr: SocketAddr) -> Option<Self> {
        match addr {
            SocketAddr::V4(v4) => Some(Self { id, addr: v4 }),
            SocketAddr::V6(_) => None,
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

    fn contact(byte: u8, port: u16) -> CachedContact {
        CachedContact {
            id: NodeId::from_bytes([byte; 20]),
            addr: SocketAddrV4::new(Ipv4Addr::new(10, 0, 0, byte), port),
        }
    }

    #[test]
    fn encode_decode_roundtrip() {
        let contacts = vec![contact(1, 6881), contact(2, 51413), contact(3, 7777)];
        let bytes = encode_cache(&contacts);
        let got = decode_cache(&bytes).unwrap();
        assert_eq!(got, contacts);
    }

    #[test]
    fn empty_cache_roundtrips() {
        let bytes = encode_cache(&[]);
        let got = decode_cache(&bytes).unwrap();
        assert!(got.is_empty());
    }

    #[test]
    fn decode_rejects_non_dict_root() {
        let err = decode_cache(b"i0e").unwrap_err();
        assert!(matches!(err, CacheError::Decode(_)));
    }

    #[test]
    fn decode_rejects_future_schema_version() {
        use std::borrow::Cow;
        use std::collections::BTreeMap;
        let mut root: BTreeMap<Cow<'_, [u8]>, Value<'_>> = BTreeMap::new();
        root.insert(Cow::Borrowed(b"v"), Value::Int(CACHE_SCHEMA_VERSION + 1));
        root.insert(Cow::Borrowed(b"saved_at"), Value::Int(0));
        root.insert(
            Cow::Borrowed(b"nodes"),
            Value::Bytes(Cow::Owned(Vec::new())),
        );
        let bytes = encode(&Value::Dict(root));
        let err = decode_cache(&bytes).unwrap_err();
        assert!(matches!(err, CacheError::UnsupportedVersion(_)));
    }

    #[test]
    fn decode_rejects_misaligned_nodes_bytes() {
        use std::borrow::Cow;
        use std::collections::BTreeMap;
        let mut root: BTreeMap<Cow<'_, [u8]>, Value<'_>> = BTreeMap::new();
        root.insert(Cow::Borrowed(b"v"), Value::Int(CACHE_SCHEMA_VERSION));
        root.insert(Cow::Borrowed(b"saved_at"), Value::Int(0));
        root.insert(
            Cow::Borrowed(b"nodes"),
            Value::Bytes(Cow::Owned(vec![0u8; COMPACT_NODE_V4_LEN + 3])),
        );
        let bytes = encode(&Value::Dict(root));
        let err = decode_cache(&bytes).unwrap_err();
        assert!(matches!(err, CacheError::Decode(_)));
    }

    #[test]
    fn encode_truncates_to_max_contacts() {
        let cap = u8::try_from(CACHE_MAX_CONTACTS).unwrap();
        let contacts: Vec<CachedContact> = (0..(cap + 20)).map(|i| contact(i, 6881)).collect();
        let bytes = encode_cache(&contacts);
        let got = decode_cache(&bytes).unwrap();
        assert_eq!(got.len(), CACHE_MAX_CONTACTS);
    }

    #[test]
    fn file_cache_save_load_roundtrip() {
        let tmp =
            std::env::temp_dir().join(format!("magpie-dht-cache-{}.bencode", std::process::id()));
        let _ = fs::remove_file(&tmp);
        let cache = FileContactCache::new(tmp.clone());
        let contacts = vec![contact(1, 6881), contact(2, 51413)];
        cache.save(&contacts).unwrap();
        let loaded = cache.load().unwrap();
        assert_eq!(loaded, contacts);
        fs::remove_file(&tmp).ok();
    }

    #[test]
    fn file_cache_missing_file_returns_empty() {
        let cache = FileContactCache::new(std::env::temp_dir().join(format!(
            "magpie-dht-cache-missing-{}.bencode",
            std::process::id()
        )));
        assert!(cache.load().unwrap().is_empty());
    }

    #[test]
    fn from_socket_rejects_v6() {
        let v6: SocketAddr = "[::1]:6881".parse().unwrap();
        assert!(CachedContact::from_socket(NodeId::ZERO, v6).is_none());
    }
}
