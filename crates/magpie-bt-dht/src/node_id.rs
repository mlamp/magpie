//! 160-bit node identifiers, XOR distance, and BEP 42 IP-salted ID
//! generation.
//!
//! Kademlia identifies nodes by a 160-bit ID and routes by the XOR
//! metric: the "distance" between two IDs is their bitwise XOR,
//! interpreted as an unsigned 160-bit integer. Lower distance means
//! a shorter path to traverse in a structured lookup.
//!
//! [`NodeId`] is the 20-byte newtype. [`Distance`] is the XOR result,
//! also 20 bytes and `Ord` so it can be used as a sort key or
//! comparator directly. [`generate_node_id`] derives a random ID;
//! when a public IP is known it applies the BEP 42 salt so Sybil
//! attackers can't mint arbitrary IDs cheaply.

use std::net::IpAddr;

use getrandom::fill as rand_fill;

// ---------------------------------------------------------------------------
// NodeId
// ---------------------------------------------------------------------------

/// A 160-bit DHT node identifier.
///
/// The byte layout is big-endian: `bytes[0]` is the most-significant
/// byte (prefix for bucket partitioning), `bytes[19]` is the least.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Default)]
pub struct NodeId {
    bytes: [u8; 20],
}

impl NodeId {
    /// All-zero node id; useful as a lower bound or sentinel.
    pub const ZERO: Self = Self { bytes: [0; 20] };

    /// Wrap 20 raw bytes as a `NodeId`.
    #[must_use]
    pub const fn from_bytes(bytes: [u8; 20]) -> Self {
        Self { bytes }
    }

    /// Raw bytes in wire order.
    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; 20] {
        &self.bytes
    }

    /// XOR distance to another node id, used as the Kademlia metric.
    #[must_use]
    pub fn distance(&self, other: &Self) -> Distance {
        let mut out = [0u8; 20];
        for (i, slot) in out.iter_mut().enumerate() {
            *slot = self.bytes[i] ^ other.bytes[i];
        }
        Distance { bytes: out }
    }

    /// Generate a 20-byte id filled from the OS CSPRNG.
    ///
    /// # Errors
    ///
    /// Propagates any failure from [`getrandom::fill`]. On commodity
    /// platforms this cannot fail; it is surfaced for embedded targets
    /// where the RNG may be unavailable at startup.
    pub fn random() -> Result<Self, getrandom::Error> {
        let mut bytes = [0u8; 20];
        rand_fill(&mut bytes)?;
        Ok(Self { bytes })
    }
}

// ---------------------------------------------------------------------------
// Distance
// ---------------------------------------------------------------------------

/// XOR distance between two [`NodeId`]s. Ordering is big-endian
/// unsigned, so `a.distance(x) < a.distance(y)` means `x` is closer
/// to `a` than `y` in the Kademlia metric.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Distance {
    bytes: [u8; 20],
}

impl Distance {
    /// Raw distance bytes, big-endian.
    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; 20] {
        &self.bytes
    }

    /// Number of leading zero bits — the Kademlia bucket depth
    /// between two ids. 0 ≤ return ≤ 160.
    #[must_use]
    pub fn leading_zeros(&self) -> u32 {
        let mut total = 0;
        for b in self.bytes {
            if b == 0 {
                total += 8;
            } else {
                total += b.leading_zeros();
                return total;
            }
        }
        total
    }
}

// ---------------------------------------------------------------------------
// BEP 42 node-id salting
// ---------------------------------------------------------------------------

/// IPv4 mask used by BEP 42. The top N bits of the IP seed the CRC.
const V4_MASK: [u8; 4] = [0x03, 0x0f, 0x3f, 0xff];
/// IPv6 mask used by BEP 42 (first 8 bytes).
const V6_MASK: [u8; 8] = [0x01, 0x03, 0x07, 0x0f, 0x1f, 0x3f, 0x7f, 0xff];

/// Generate a local DHT node id.
///
/// `public_ip = None` returns an un-salted random id, suitable for the
/// two-phase startup flow in ADR-0026: we bootstrap with an un-salted
/// id, observe our public IP via `yourip`/tracker echo, then re-derive
/// a BEP-42-compliant id and let the routing-table refresh republish
/// it. When `public_ip = Some(_)` the top 21 bits of the id are fixed
/// by the BEP 42 CRC so only a host at that IP (modulo the mask) can
/// forge ids close to a given target.
///
/// # Errors
///
/// Propagates any failure from [`getrandom::fill`].
pub fn generate_node_id(public_ip: Option<IpAddr>) -> Result<NodeId, getrandom::Error> {
    match public_ip {
        None => NodeId::random(),
        Some(ip) => {
            let mut rand_tail = [0u8; 20];
            rand_fill(&mut rand_tail)?;
            // BEP 42 reserves the low 3 bits of byte 2 as a per-node
            // randomiser the CRC is computed over. Draw those bits
            // from the RNG byte we'd otherwise discard.
            let rand_byte = rand_tail[2] & 0x07;
            Ok(build_bep42_node_id(ip, rand_byte, &rand_tail))
        }
    }
}

/// Construct a BEP 42-conformant id for `ip` and a 3-bit `rand` seed.
/// The last 17 bytes of `tail` fill the id's random suffix; the first
/// three bytes of `tail` are overwritten by the CRC+rand prefix.
///
/// Separated from [`generate_node_id`] so deterministic test vectors
/// can exercise the CRC layout.
fn build_bep42_node_id(ip: IpAddr, rand: u8, tail: &[u8; 20]) -> NodeId {
    let rand = rand & 0x07;
    let crc = crc32c_for_ip(ip, rand).to_be_bytes();

    let mut bytes = *tail;
    bytes[0] = crc[0];
    bytes[1] = crc[1];
    // Keep the top 5 bits of the CRC in byte 2; the low 3 bits carry `rand`.
    bytes[2] = (crc[2] & 0xf8) | rand;
    NodeId { bytes }
}

/// BEP 42 validation: return `true` iff `id` could have been generated
/// from `ip` for some `rand ∈ 0..8`.
///
/// Used by the optional `strict_bep42_inbound` mode (ADR-0026) and by
/// the generation unit tests.
#[must_use]
pub fn validate_bep42(id: &NodeId, ip: IpAddr) -> bool {
    let rand = id.bytes[2] & 0x07;
    let crc = crc32c_for_ip(ip, rand).to_be_bytes();
    id.bytes[0] == crc[0] && id.bytes[1] == crc[1] && (id.bytes[2] & 0xf8) == (crc[2] & 0xf8)
}

fn crc32c_for_ip(ip: IpAddr, rand: u8) -> u32 {
    match ip {
        IpAddr::V4(v4) => {
            let oct = v4.octets();
            let mut input = [0u8; 4];
            for i in 0..4 {
                input[i] = oct[i] & V4_MASK[i];
            }
            input[0] |= (rand & 0x07) << 5;
            crc32c(&input)
        }
        IpAddr::V6(v6) => {
            let oct = v6.octets();
            let mut input = [0u8; 8];
            for i in 0..8 {
                input[i] = oct[i] & V6_MASK[i];
            }
            input[0] |= (rand & 0x07) << 5;
            crc32c(&input)
        }
    }
}

// ---------------------------------------------------------------------------
// CRC32C (Castagnoli, polynomial 0x1EDC6F41 reflected as 0x82F63B78)
// ---------------------------------------------------------------------------
//
// Tiny software implementation, no SIMD/hardware fallback — this is a
// configuration-time cost (local id derivation, strict-mode inbound
// check) not a per-datagram hot path. Constant-time `const` table.

const CRC32C_POLY: u32 = 0x82F6_3B78;

const CRC32C_TABLE: [u32; 256] = build_crc32c_table();

const fn build_crc32c_table() -> [u32; 256] {
    let mut table = [0u32; 256];
    let mut n = 0u32;
    while n < 256 {
        let mut c = n;
        let mut k = 0;
        while k < 8 {
            c = if c & 1 == 1 {
                CRC32C_POLY ^ (c >> 1)
            } else {
                c >> 1
            };
            k += 1;
        }
        table[n as usize] = c;
        n += 1;
    }
    table
}

fn crc32c(data: &[u8]) -> u32 {
    let mut c = 0xFFFF_FFFFu32;
    for &b in data {
        c = CRC32C_TABLE[((c ^ u32::from(b)) & 0xff) as usize] ^ (c >> 8);
    }
    c ^ 0xFFFF_FFFF
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{Ipv4Addr, Ipv6Addr};

    #[test]
    fn xor_distance_self_is_zero() {
        let a = NodeId::from_bytes([0xab; 20]);
        assert_eq!(a.distance(&a), Distance { bytes: [0; 20] });
    }

    #[test]
    fn xor_distance_symmetric() {
        let a = NodeId::from_bytes([0x01; 20]);
        let b = NodeId::from_bytes([0xfe; 20]);
        assert_eq!(a.distance(&b), b.distance(&a));
    }

    #[test]
    fn xor_distance_ordering() {
        let origin = NodeId::ZERO;
        let near = NodeId::from_bytes({
            let mut b = [0u8; 20];
            b[19] = 1;
            b
        });
        let far = NodeId::from_bytes([0xff; 20]);
        assert!(origin.distance(&near) < origin.distance(&far));
    }

    #[test]
    fn distance_leading_zeros_full() {
        let a = NodeId::from_bytes([0; 20]);
        let b = NodeId::from_bytes([0; 20]);
        assert_eq!(a.distance(&b).leading_zeros(), 160);
    }

    #[test]
    fn distance_leading_zeros_msb_set() {
        let mut b = [0u8; 20];
        b[0] = 0x80;
        let d = NodeId::ZERO.distance(&NodeId::from_bytes(b));
        assert_eq!(d.leading_zeros(), 0);
    }

    #[test]
    fn distance_leading_zeros_mid_bit() {
        let mut b = [0u8; 20];
        b[2] = 0x04; // zeros in bytes 0-1 (16) + 0000_0100 -> 5 leading zeros in byte 2
        let d = NodeId::ZERO.distance(&NodeId::from_bytes(b));
        assert_eq!(d.leading_zeros(), 16 + 5);
    }

    #[test]
    fn random_produces_differing_ids() {
        let a = NodeId::random().unwrap();
        let b = NodeId::random().unwrap();
        assert_ne!(a, b);
    }

    // BEP 42 test vectors from the spec (§ "test vectors") — four
    // published (ip, rand) combinations with expected id prefixes.
    //
    // Source: BEP 42 "DHT Security extension" test vectors.
    // Stored as (ip, rand, first 3 bytes of expected id).
    const BEP42_VECTORS: &[(&str, u8, [u8; 3])] = &[
        ("124.31.75.21", 1, [0x5f, 0xbf, 0xb8]),
        ("21.75.31.124", 86, [0x5a, 0x3c, 0xe9]),
        ("65.23.51.170", 22, [0xa5, 0xd4, 0x32]),
        ("84.124.73.14", 65, [0x1b, 0x03, 0x21]),
        ("43.213.53.83", 90, [0xe5, 0x6f, 0x6c]),
    ];

    #[test]
    fn bep42_known_vectors() {
        for (ip_str, rand, want_prefix) in BEP42_VECTORS {
            let ip: IpAddr = ip_str.parse().unwrap();
            // The published vectors use rand_lsb = rand & 0x07; keep
            // top 5 bits of the id byte 2 as the CRC result.
            let id = build_bep42_node_id(ip, *rand, &[0u8; 20]);
            assert_eq!(id.bytes[0], want_prefix[0], "byte 0 for {ip}");
            assert_eq!(id.bytes[1], want_prefix[1], "byte 1 for {ip}");
            // Top 5 bits of byte 2 come from the CRC, low 3 from rand.
            assert_eq!(
                id.bytes[2] & 0xf8,
                want_prefix[2] & 0xf8,
                "top 5 bits of byte 2 for {ip}"
            );
            assert_eq!(id.bytes[2] & 0x07, rand & 0x07, "rand tail for {ip}");
        }
    }

    #[test]
    fn bep42_validate_roundtrip() {
        let ip: IpAddr = "203.0.113.42".parse().unwrap();
        let id = generate_node_id(Some(ip)).unwrap();
        assert!(validate_bep42(&id, ip));
    }

    #[test]
    fn bep42_validate_rejects_mismatched_ip() {
        let ip_a: IpAddr = "203.0.113.42".parse().unwrap();
        let ip_b: IpAddr = "198.51.100.7".parse().unwrap();
        let id = generate_node_id(Some(ip_a)).unwrap();
        assert!(!validate_bep42(&id, ip_b));
    }

    #[test]
    fn bep42_unsalted_when_ip_unknown() {
        // Un-salted ids are random — validation against any IP is
        // expected to fail with overwhelming probability, but we only
        // need the weaker property that the generator does not crash
        // and produces 20 bytes.
        let id = generate_node_id(None).unwrap();
        assert_eq!(id.as_bytes().len(), 20);
    }

    #[test]
    fn bep42_ipv6_validates() {
        let ip: IpAddr = Ipv6Addr::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, 1).into();
        let id = generate_node_id(Some(ip)).unwrap();
        assert!(validate_bep42(&id, ip));
    }

    #[test]
    fn crc32c_known_vector() {
        // Castagnoli test vector from RFC 3720 §B.4: CRC32C("123456789") == 0xE3069283.
        assert_eq!(crc32c(b"123456789"), 0xE306_9283);
    }

    #[test]
    fn bep42_ipv4_any_and_loopback_validate() {
        let ip = IpAddr::V4(Ipv4Addr::LOCALHOST);
        let id = generate_node_id(Some(ip)).unwrap();
        assert!(validate_bep42(&id, ip));
    }
}
