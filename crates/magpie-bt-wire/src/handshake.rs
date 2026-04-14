//! BEP 3 peer handshake: 68-byte fixed-size frame exchanged once per
//! connection before any [`Message`](crate::Message) flows.
//!
//! Layout (per BEP 3):
//!
//! ```text
//! offset  size  field
//! 0       1     pstrlen = 19
//! 1       19    pstr    = b"BitTorrent protocol"
//! 20      8     reserved (extension bits, see BEP 4)
//! 28      20    info_hash
//! 48      20    peer_id
//! ```

use crate::WireError;

/// BEP 3 protocol string.
pub const PSTR: &[u8; 19] = b"BitTorrent protocol";

/// BEP 3 protocol string length byte value.
pub const PSTRLEN: u8 = 19;

/// Total handshake size in bytes.
pub const HANDSHAKE_LEN: usize = 1 + 19 + 8 + 20 + 20;

/// BEP 6 Fast extension reserved bit: byte 7, mask `0x04`.
const FAST_EXT_BYTE: usize = 7;
const FAST_EXT_MASK: u8 = 0x04;

/// BEP 10 extension protocol bit: byte 5, mask `0x10`.
const EXTENSION_PROTOCOL_BYTE: usize = 5;
const EXTENSION_PROTOCOL_MASK: u8 = 0x10;

/// A parsed BEP 3 handshake frame.
///
/// Construct one with [`Handshake::new`] and toggle reserved-bit features
/// with [`Handshake::with_fast_ext`] / [`Handshake::with_extension_protocol`]
/// before encoding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Handshake {
    /// Reserved 8-byte field carrying extension capability bits (BEP 4).
    pub reserved: [u8; 8],
    /// Torrent info-hash (SHA-1 for v1, truncated SHA-256 for v2).
    pub info_hash: [u8; 20],
    /// Sender's 20-byte peer id.
    pub peer_id: [u8; 20],
}

impl Handshake {
    /// Construct a handshake with all reserved bits cleared.
    #[must_use]
    pub const fn new(info_hash: [u8; 20], peer_id: [u8; 20]) -> Self {
        Self {
            reserved: [0; 8],
            info_hash,
            peer_id,
        }
    }

    /// Set the BEP 6 Fast extension capability bit.
    #[must_use]
    pub const fn with_fast_ext(mut self) -> Self {
        self.reserved[FAST_EXT_BYTE] |= FAST_EXT_MASK;
        self
    }

    /// Set the BEP 10 extension-protocol capability bit.
    #[must_use]
    pub const fn with_extension_protocol(mut self) -> Self {
        self.reserved[EXTENSION_PROTOCOL_BYTE] |= EXTENSION_PROTOCOL_MASK;
        self
    }

    /// Whether the peer signalled BEP 6 Fast extension support.
    #[must_use]
    pub const fn supports_fast_ext(&self) -> bool {
        self.reserved[FAST_EXT_BYTE] & FAST_EXT_MASK != 0
    }

    /// Whether the peer signalled BEP 10 extension-protocol support.
    #[must_use]
    pub const fn supports_extension_protocol(&self) -> bool {
        self.reserved[EXTENSION_PROTOCOL_BYTE] & EXTENSION_PROTOCOL_MASK != 0
    }

    /// Encode into the provided 68-byte buffer.
    pub fn encode(&self, out: &mut [u8; HANDSHAKE_LEN]) {
        out[0] = PSTRLEN;
        out[1..20].copy_from_slice(PSTR);
        out[20..28].copy_from_slice(&self.reserved);
        out[28..48].copy_from_slice(&self.info_hash);
        out[48..68].copy_from_slice(&self.peer_id);
    }

    /// Encode into a freshly-allocated 68-byte array.
    #[must_use]
    pub fn to_bytes(&self) -> [u8; HANDSHAKE_LEN] {
        let mut out = [0u8; HANDSHAKE_LEN];
        self.encode(&mut out);
        out
    }

    /// Decode a 68-byte handshake frame.
    ///
    /// # Errors
    ///
    /// Returns [`WireError::BadHandshakePstrlen`] if the leading length byte
    /// is not 19, or [`WireError::BadHandshakePstr`] if the protocol string
    /// does not match the BEP 3 literal.
    pub fn decode(buf: &[u8; HANDSHAKE_LEN]) -> Result<Self, WireError> {
        if buf[0] != PSTRLEN {
            return Err(WireError::BadHandshakePstrlen(buf[0]));
        }
        if &buf[1..20] != PSTR.as_slice() {
            return Err(WireError::BadHandshakePstr);
        }
        let mut reserved = [0u8; 8];
        reserved.copy_from_slice(&buf[20..28]);
        let mut info_hash = [0u8; 20];
        info_hash.copy_from_slice(&buf[28..48]);
        let mut peer_id = [0u8; 20];
        peer_id.copy_from_slice(&buf[48..68]);
        Ok(Self {
            reserved,
            info_hash,
            peer_id,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Handshake {
        Handshake::new([0xAA; 20], [0xBB; 20])
            .with_fast_ext()
            .with_extension_protocol()
    }

    #[test]
    fn roundtrip() {
        let h = sample();
        let bytes = h.to_bytes();
        let back = Handshake::decode(&bytes).unwrap();
        assert_eq!(h, back);
        assert!(back.supports_fast_ext());
        assert!(back.supports_extension_protocol());
    }

    #[test]
    fn rejects_bad_pstrlen() {
        let mut bytes = sample().to_bytes();
        bytes[0] = 18;
        assert!(matches!(
            Handshake::decode(&bytes),
            Err(WireError::BadHandshakePstrlen(18))
        ));
    }

    #[test]
    fn rejects_bad_pstr() {
        let mut bytes = sample().to_bytes();
        bytes[1] = b'X';
        assert!(matches!(
            Handshake::decode(&bytes),
            Err(WireError::BadHandshakePstr)
        ));
    }

    #[test]
    fn fast_ext_bit_layout() {
        let h = Handshake::new([0; 20], [0; 20]).with_fast_ext();
        assert_eq!(h.reserved[7], 0x04);
    }

    #[test]
    fn extension_protocol_bit_layout() {
        let h = Handshake::new([0; 20], [0; 20]).with_extension_protocol();
        assert_eq!(h.reserved[5], 0x10);
    }
}
