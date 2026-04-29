//! BEP 10 extension handshake and per-peer extension ID registry.
//!
//! The extension handshake is a bencoded dictionary exchanged as
//! `Message::Extended { id: 0, .. }` immediately after the BEP 3 handshake
//! when both sides advertise BEP 10 support. It negotiates which extension
//! message IDs each peer uses and carries metadata such as `metadata_size`
//! (BEP 9) and the client name.

use std::borrow::Cow;
use std::collections::{BTreeMap, HashMap};

use thiserror::Error;

use magpie_bt_bencode::{self as bencode, Value};

use crate::metadata::MAX_METADATA_SIZE;

/// Maximum number of entries allowed in the `m` dictionary.
///
/// Bounds decoder memory: a malicious peer cannot force us to allocate an
/// arbitrarily large `HashMap` by advertising thousands of extensions.
pub const MAX_EXTENSIONS: usize = 128;

/// Errors produced when decoding a BEP 10 extension handshake.
#[derive(Debug, Error)]
pub enum ExtensionError {
    /// The bencode payload could not be parsed or has the wrong structure.
    #[error("bencode decode error: {0}")]
    Decode(String),
    /// An extension id in the `m` dict was outside the valid `0..=255` range.
    #[error("extension {name:?} has invalid id {id} (must be 0..=255)")]
    InvalidExtensionId {
        /// The extension name whose id was out of range.
        name: String,
        /// The offending id value from the handshake.
        id: i64,
    },
    /// The `m` dict contained more entries than `MAX_EXTENSIONS`.
    #[error("too many extensions: {0} (max {MAX_EXTENSIONS})")]
    TooManyExtensions(usize),
    /// The `metadata_size` field exceeded `MAX_METADATA_SIZE`.
    #[error("metadata_size {0} exceeds maximum {MAX_METADATA_SIZE}")]
    MetadataSizeTooLarge(u64),
    /// An extension key in the `m` dict was not valid UTF-8.
    #[error("extension name is not valid UTF-8")]
    NonUtf8ExtensionName,
}

/// A decoded BEP 10 extension handshake payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtensionHandshake {
    /// The `m` dictionary: extension name -> local message ID.
    pub extensions: HashMap<String, u8>,
    /// BEP 9: size of the torrent's info dictionary in bytes.
    pub metadata_size: Option<u64>,
    /// Informational client name (e.g. "qBittorrent 4.6.0").
    pub client: Option<String>,
    /// BEP 10: peer's listen port.
    pub listen_port: Option<u16>,
    /// BEP 10: our external IP as seen by the peer.
    pub yourip: Option<Vec<u8>>,
    /// BEP 10: maximum outstanding request limit advertised by the peer.
    pub reqq: Option<u32>,
}

impl ExtensionHandshake {
    /// Decode a bencoded extension handshake payload.
    ///
    /// Strict at the structural level (rejects malformed dicts, out-of-range
    /// bounds, non-UTF-8 extension names) but lenient per-entry for unknown
    /// scalar fields — any key we don't recognise is silently ignored so we
    /// stay forward-compatible.
    ///
    /// # Errors
    ///
    /// Returns [`ExtensionError`] when the payload is not a valid bencode
    /// dictionary, contains an extension id outside `0..=255`, exceeds
    /// [`MAX_EXTENSIONS`] entries, advertises a `metadata_size` larger than
    /// [`MAX_METADATA_SIZE`], or has a non-UTF-8 extension name.
    pub fn decode(data: &[u8]) -> Result<Self, ExtensionError> {
        let val = bencode::decode(data).map_err(|e| ExtensionError::Decode(e.to_string()))?;
        let dict = val
            .as_dict()
            .ok_or_else(|| ExtensionError::Decode("top-level value is not a dict".into()))?;

        let extensions = match dict.get(b"m".as_slice()) {
            Some(m_val) => {
                let m_dict = m_val
                    .as_dict()
                    .ok_or_else(|| ExtensionError::Decode("\"m\" is not a dict".into()))?;
                if m_dict.len() > MAX_EXTENSIONS {
                    return Err(ExtensionError::TooManyExtensions(m_dict.len()));
                }
                let mut map = HashMap::with_capacity(m_dict.len());
                for (k, v) in m_dict {
                    let name =
                        std::str::from_utf8(k).map_err(|_| ExtensionError::NonUtf8ExtensionName)?;
                    let id_i64 = v.as_int().ok_or_else(|| {
                        ExtensionError::Decode(format!("extension {name:?} id is not an integer"))
                    })?;
                    let id =
                        u8::try_from(id_i64).map_err(|_| ExtensionError::InvalidExtensionId {
                            name: name.to_owned(),
                            id: id_i64,
                        })?;
                    // BEP 10: id 0 means "extension disabled" — skip it.
                    if id == 0 {
                        continue;
                    }
                    map.insert(name.to_owned(), id);
                }
                map
            }
            None => HashMap::new(),
        };

        let metadata_size = match dict
            .get(b"metadata_size".as_slice())
            .and_then(Value::as_int)
        {
            Some(i) if i < 0 => None,
            Some(i) => {
                #[allow(clippy::cast_sign_loss)]
                let size = i as u64;
                if size > MAX_METADATA_SIZE {
                    return Err(ExtensionError::MetadataSizeTooLarge(size));
                }
                Some(size)
            }
            None => None,
        };

        let client = dict
            .get(b"v".as_slice())
            .and_then(Value::as_bytes)
            .map(|b| String::from_utf8_lossy(b).into_owned());

        let listen_port = dict
            .get(b"p".as_slice())
            .and_then(Value::as_int)
            .and_then(|i| u16::try_from(i).ok());

        let yourip = dict
            .get(b"yourip".as_slice())
            .and_then(Value::as_bytes)
            .map(<[u8]>::to_vec);

        let reqq = dict
            .get(b"reqq".as_slice())
            .and_then(Value::as_int)
            .and_then(|i| u32::try_from(i).ok());

        Ok(Self {
            extensions,
            metadata_size,
            client,
            listen_port,
            yourip,
            reqq,
        })
    }

    /// Encode this handshake into a bencoded byte vector.
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let mut dict: BTreeMap<Cow<'_, [u8]>, Value<'_>> = BTreeMap::new();

        let mut m_dict: BTreeMap<Cow<'_, [u8]>, Value<'_>> = BTreeMap::new();
        for (name, &id) in &self.extensions {
            m_dict.insert(
                Cow::Owned(name.as_bytes().to_vec()),
                Value::Int(i64::from(id)),
            );
        }
        dict.insert(Cow::Borrowed(b"m"), Value::Dict(m_dict));

        if let Some(ms) = self.metadata_size {
            #[allow(clippy::cast_possible_wrap)]
            let clamped = ms.min(i64::MAX as u64) as i64;
            dict.insert(Cow::Borrowed(b"metadata_size"), Value::Int(clamped));
        }

        if let Some(port) = self.listen_port {
            dict.insert(Cow::Borrowed(b"p"), Value::Int(i64::from(port)));
        }

        if let Some(ref ip) = self.yourip {
            dict.insert(
                Cow::Borrowed(b"yourip"),
                Value::Bytes(Cow::Owned(ip.clone())),
            );
        }

        if let Some(r) = self.reqq {
            dict.insert(Cow::Borrowed(b"reqq"), Value::Int(i64::from(r)));
        }

        if let Some(ref v) = self.client {
            dict.insert(
                Cow::Borrowed(b"v"),
                Value::Bytes(Cow::Owned(v.as_bytes().to_vec())),
            );
        }

        bencode::encode(&Value::Dict(dict))
    }
}

/// Per-peer extension ID registry (BEP 10).
///
/// Tracks the mapping between canonical extension names and the local/remote
/// message IDs negotiated during the extension handshake.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ExtensionRegistry {
    /// Our local extension name -> local ID mapping.
    local: HashMap<String, u8>,
    /// Remote extension name -> remote ID mapping (from the peer's handshake).
    remote: HashMap<String, u8>,
}

impl ExtensionRegistry {
    /// Create a new registry with the given local extension ID assignments.
    #[must_use]
    pub fn new(local: HashMap<String, u8>) -> Self {
        Self {
            local,
            remote: HashMap::new(),
        }
    }

    /// Build our extension handshake message from our local IDs.
    #[must_use]
    pub fn our_handshake(&self) -> ExtensionHandshake {
        ExtensionHandshake {
            extensions: self.local.clone(),
            metadata_size: None,
            client: Some("magpie".to_owned()),
            listen_port: None,
            yourip: None,
            reqq: None,
        }
    }

    /// Record the peer's extension ID mapping from their handshake.
    pub fn set_remote(&mut self, hs: &ExtensionHandshake) {
        self.remote.clone_from(&hs.extensions);
    }

    /// Look up the local message ID we assigned for the named extension.
    #[must_use]
    pub fn local_id(&self, name: &str) -> Option<u8> {
        self.local.get(name).copied()
    }

    /// Look up the remote peer's message ID for the named extension.
    #[must_use]
    pub fn remote_id(&self, name: &str) -> Option<u8> {
        self.remote.get(name).copied()
    }

    /// Reverse-lookup: find the canonical extension name for one of our local
    /// IDs. Returns `None` if `id` is not in our local mapping.
    #[must_use]
    pub fn local_name_for_id(&self, id: u8) -> Option<&str> {
        self.local
            .iter()
            .find(|&(_, &v)| v == id)
            .map(|(k, _)| k.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn qbittorrent_handshake_bytes() -> Vec<u8> {
        let hs = ExtensionHandshake {
            extensions: HashMap::from([("ut_metadata".into(), 1), ("ut_pex".into(), 2)]),
            metadata_size: Some(31235),
            client: Some("qBittorrent/4.5.5".into()),
            listen_port: Some(6881),
            yourip: Some(vec![127, 0, 0, 1]),
            reqq: Some(255),
        };
        hs.encode()
    }

    #[test]
    fn decode_realistic_handshake() {
        let bytes = qbittorrent_handshake_bytes();
        let hs = ExtensionHandshake::decode(&bytes).unwrap();

        assert_eq!(hs.extensions.get("ut_metadata"), Some(&1));
        assert_eq!(hs.extensions.get("ut_pex"), Some(&2));
        assert_eq!(hs.metadata_size, Some(31235));
        assert_eq!(hs.listen_port, Some(6881));
        assert_eq!(hs.client.as_deref(), Some("qBittorrent/4.5.5"));
        assert_eq!(hs.yourip.as_deref(), Some(&[127, 0, 0, 1][..]));
        assert_eq!(hs.reqq, Some(255));
    }

    #[test]
    fn decode_minimal_handshake_empty_m() {
        let hs = ExtensionHandshake::decode(b"d1:mdee").unwrap();
        assert!(hs.extensions.is_empty());
        assert_eq!(hs.metadata_size, None);
    }

    #[test]
    fn decode_minimal_handshake_missing_m() {
        // Some clients send a dict with no `m` at all.
        let hs = ExtensionHandshake::decode(b"de").unwrap();
        assert!(hs.extensions.is_empty());
    }

    #[test]
    fn decode_not_a_dict() {
        let err = ExtensionHandshake::decode(b"i42e").unwrap_err();
        assert!(matches!(err, ExtensionError::Decode(_)));
    }

    #[test]
    fn decode_invalid_bencode() {
        let err = ExtensionHandshake::decode(b"zzz").unwrap_err();
        assert!(matches!(err, ExtensionError::Decode(_)));
    }

    #[test]
    fn decode_m_not_a_dict() {
        // `m` exists but is an integer, not a dict.
        let err = ExtensionHandshake::decode(b"d1:mi1ee").unwrap_err();
        assert!(matches!(err, ExtensionError::Decode(_)));
    }

    #[test]
    fn decode_extension_id_too_large() {
        // ut_metadata with id 256 (out of u8 range).
        let err = ExtensionHandshake::decode(b"d1:md11:ut_metadatai256eee").unwrap_err();
        assert!(matches!(
            err,
            ExtensionError::InvalidExtensionId { ref name, id: 256 } if name == "ut_metadata"
        ));
    }

    #[test]
    fn decode_extension_id_negative() {
        let err = ExtensionHandshake::decode(b"d1:md11:ut_metadatai-1eee").unwrap_err();
        assert!(matches!(
            err,
            ExtensionError::InvalidExtensionId { ref name, id: -1 } if name == "ut_metadata"
        ));
    }

    #[test]
    fn decode_extension_id_zero_is_skipped() {
        // ut_metadata has id 0 (disabled), ut_pex has id 2 (enabled).
        let hs = ExtensionHandshake::decode(b"d1:md11:ut_metadatai0e6:ut_pexi2eee").unwrap();
        assert_eq!(hs.extensions.get("ut_metadata"), None);
        assert_eq!(hs.extensions.get("ut_pex"), Some(&2));
    }

    #[test]
    fn decode_extension_id_not_an_integer() {
        // id field is a string instead of an int.
        let err = ExtensionHandshake::decode(b"d1:md11:ut_metadata1:xeee").unwrap_err();
        assert!(matches!(err, ExtensionError::Decode(_)));
    }

    #[test]
    fn decode_too_many_extensions_rejected() {
        let mut exts = HashMap::new();
        for i in 0..=MAX_EXTENSIONS {
            exts.insert(format!("ext_{i}"), 1u8);
        }
        let hs = ExtensionHandshake {
            extensions: exts,
            metadata_size: None,
            client: None,
            listen_port: None,
            yourip: None,
            reqq: None,
        };
        let encoded = hs.encode();
        let err = ExtensionHandshake::decode(&encoded).unwrap_err();
        assert!(matches!(err, ExtensionError::TooManyExtensions(n) if n == MAX_EXTENSIONS + 1));
    }

    #[test]
    fn decode_exactly_max_extensions_accepted() {
        // MAX_EXTENSIONS entries must decode successfully.
        let mut exts = HashMap::new();
        for i in 0..MAX_EXTENSIONS {
            exts.insert(format!("ext_{i}"), 1u8);
        }
        let hs = ExtensionHandshake {
            extensions: exts,
            metadata_size: None,
            client: None,
            listen_port: None,
            yourip: None,
            reqq: None,
        };
        let encoded = hs.encode();
        let decoded = ExtensionHandshake::decode(&encoded).unwrap();
        assert_eq!(decoded.extensions.len(), MAX_EXTENSIONS);
    }

    #[test]
    fn decode_negative_metadata_size_treated_as_none() {
        let hs = ExtensionHandshake::decode(b"d1:mde13:metadata_sizei-1ee").unwrap();
        assert_eq!(hs.metadata_size, None);
    }

    #[test]
    fn decode_metadata_size_over_limit_rejected() {
        // 16_000_001 > MAX_METADATA_SIZE (16_000_000).
        let err = ExtensionHandshake::decode(b"d1:mde13:metadata_sizei16000001ee").unwrap_err();
        assert!(matches!(
            err,
            ExtensionError::MetadataSizeTooLarge(s) if s == MAX_METADATA_SIZE + 1
        ));
    }

    #[test]
    fn decode_metadata_size_at_limit_accepted() {
        // Exactly MAX_METADATA_SIZE must be accepted.
        let bytes = format!("d1:mde13:metadata_sizei{MAX_METADATA_SIZE}ee");
        let hs = ExtensionHandshake::decode(bytes.as_bytes()).unwrap();
        assert_eq!(hs.metadata_size, Some(MAX_METADATA_SIZE));
    }

    #[test]
    fn decode_non_utf8_extension_name_rejected() {
        // Build bencode manually: d1:md3:\xff\xfe\xfdi1eee
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"d1:md3:");
        bytes.extend_from_slice(&[0xff, 0xfe, 0xfd]);
        bytes.extend_from_slice(b"i1eee");
        let err = ExtensionHandshake::decode(&bytes).unwrap_err();
        assert!(matches!(err, ExtensionError::NonUtf8ExtensionName));
    }

    #[test]
    fn roundtrip_all_fields() {
        let original = ExtensionHandshake {
            extensions: HashMap::from([
                ("ut_metadata".into(), 1),
                ("ut_pex".into(), 2),
                ("upload_only".into(), 3),
            ]),
            metadata_size: Some(42_000),
            client: Some("Transmission/3.0".into()),
            listen_port: Some(51_413),
            yourip: Some(vec![192, 168, 1, 42]),
            reqq: Some(500),
        };

        let encoded = original.encode();
        let decoded = ExtensionHandshake::decode(&encoded).unwrap();
        assert_eq!(decoded, original);
    }

    #[test]
    fn roundtrip_minimal() {
        let original = ExtensionHandshake {
            extensions: HashMap::new(),
            metadata_size: None,
            client: None,
            listen_port: None,
            yourip: None,
            reqq: None,
        };
        let encoded = original.encode();
        let decoded = ExtensionHandshake::decode(&encoded).unwrap();
        assert_eq!(decoded, original);
    }

    #[test]
    fn registry_remote_id_lookup() {
        let local = HashMap::from([("ut_metadata".into(), 1u8)]);
        let mut reg = ExtensionRegistry::new(local);

        let peer_hs = ExtensionHandshake {
            extensions: HashMap::from([("ut_metadata".into(), 3), ("ut_pex".into(), 4)]),
            metadata_size: None,
            client: None,
            listen_port: None,
            yourip: None,
            reqq: None,
        };
        reg.set_remote(&peer_hs);

        assert_eq!(reg.remote_id("ut_metadata"), Some(3));
        assert_eq!(reg.remote_id("ut_pex"), Some(4));
        assert_eq!(reg.remote_id("nonexistent"), None);
    }

    #[test]
    fn registry_local_id_lookup() {
        let local = HashMap::from([("ut_metadata".into(), 1u8), ("ut_pex".into(), 2)]);
        let reg = ExtensionRegistry::new(local);

        assert_eq!(reg.local_id("ut_metadata"), Some(1));
        assert_eq!(reg.local_id("ut_pex"), Some(2));
        assert_eq!(reg.local_id("nonexistent"), None);
    }

    #[test]
    fn registry_our_handshake_roundtrips() {
        let local = HashMap::from([("ut_metadata".into(), 1u8), ("ut_pex".into(), 2)]);
        let reg = ExtensionRegistry::new(local.clone());

        let hs = reg.our_handshake();
        assert_eq!(hs.extensions, local);

        let encoded = hs.encode();
        let decoded = ExtensionHandshake::decode(&encoded).unwrap();
        assert_eq!(decoded.extensions, local);
    }

    #[test]
    fn registry_set_remote_replaces_previous() {
        let mut reg = ExtensionRegistry::default();

        let hs1 = ExtensionHandshake {
            extensions: HashMap::from([("ut_pex".into(), 5)]),
            metadata_size: None,
            client: None,
            listen_port: None,
            yourip: None,
            reqq: None,
        };
        reg.set_remote(&hs1);
        assert_eq!(reg.remote_id("ut_pex"), Some(5));

        let hs2 = ExtensionHandshake {
            extensions: HashMap::from([("ut_pex".into(), 9)]),
            metadata_size: None,
            client: None,
            listen_port: None,
            yourip: None,
            reqq: None,
        };
        reg.set_remote(&hs2);
        assert_eq!(reg.remote_id("ut_pex"), Some(9));
    }

    #[test]
    fn registry_default_is_empty() {
        let reg = ExtensionRegistry::default();
        assert_eq!(reg.local_id("anything"), None);
        assert_eq!(reg.remote_id("anything"), None);
    }

    #[test]
    fn local_name_for_id_found() {
        let local = HashMap::from([("ut_metadata".into(), 1u8), ("ut_pex".into(), 2)]);
        let reg = ExtensionRegistry::new(local);
        assert_eq!(reg.local_name_for_id(1), Some("ut_metadata"));
        assert_eq!(reg.local_name_for_id(2), Some("ut_pex"));
        assert_eq!(reg.local_name_for_id(99), None);
    }
}
