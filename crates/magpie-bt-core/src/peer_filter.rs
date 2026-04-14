//! SSRF-resistant filter for tracker-supplied peer addresses (T3 hardening).
//!
//! A malicious or compromised tracker can return peers pointing at the
//! leecher's own loopback, the corporate intranet, or link-local services.
//! Magpie must not blindly TCP-connect to whatever the tracker hands us.
//!
//! Consumers implement [`PeerFilter`] (or use [`DefaultPeerFilter`]) and
//! invoke it on every [`std::net::SocketAddr`] before adding the peer to a
//! [`TorrentSession`](crate::session::TorrentSession). The future `Engine`
//! API will call this at the seam between the tracker module and the session.

use std::net::{IpAddr, SocketAddr};

/// Decide whether magpie should attempt a connection to `addr`.
///
/// Implementations must be `Send + Sync`; magpie may call `allow` from any
/// task. Implementations must be cheap — they're invoked in the announce
/// processing path.
pub trait PeerFilter: Send + Sync {
    /// `true` if `addr` is acceptable as a peer to connect to.
    fn allow(&self, addr: SocketAddr) -> bool;
}

/// Default filter that rejects peer addresses likely to be SSRF or
/// misconfiguration vectors.
///
/// Always rejected:
///
/// - Unspecified (`0.0.0.0`, `::`)
/// - Multicast
/// - IPv4 link-local (`169.254.0.0/16`)
/// - IPv4 broadcast (`255.255.255.255`)
/// - IPv6 link-local (`fe80::/10`)
/// - Port 0
///
/// Configurable:
///
/// - **Loopback** (`127.0.0.0/8`, `::1`): rejected unless `allow_loopback`.
/// - **Private / unique-local** (`10/8`, `172.16/12`, `192.168/16`,
///   `fc00::/7`): rejected unless `allow_private`. Defaults to permitting
///   private addresses so LAN-seeding workflows continue to work; flip to
///   [`DefaultPeerFilter::strict`] for hostile-tracker scenarios.
///
/// # Common pitfall (E10)
///
/// `DefaultPeerFilter::default()` rejects loopback, so any `add_peer` call
/// targeting `127.0.0.1` returns `AddPeerError::Filtered(_)`. Tests that
/// connect over loopback should use [`DefaultPeerFilter::permissive_for_tests`]:
///
/// ```
/// use std::sync::Arc;
/// use magpie_bt_core::peer_filter::DefaultPeerFilter;
///
/// let _filter = Arc::new(DefaultPeerFilter::permissive_for_tests());
/// ```
#[derive(Debug, Clone, Copy)]
pub struct DefaultPeerFilter {
    /// Whether to allow RFC 1918 / unique-local v6 addresses. Default: `true`.
    pub allow_private: bool,
    /// Whether to allow loopback addresses. Default: `false`. Tests typically
    /// flip this to `true` via [`Self::permissive_for_tests`].
    pub allow_loopback: bool,
}

impl Default for DefaultPeerFilter {
    fn default() -> Self {
        Self { allow_private: true, allow_loopback: false }
    }
}

impl DefaultPeerFilter {
    /// Strict variant — denies loopback *and* RFC 1918 / ULA. Use when the
    /// tracker is untrusted and the host has no LAN seeding requirements.
    #[must_use]
    pub const fn strict() -> Self {
        Self { allow_private: false, allow_loopback: false }
    }

    /// Permissive variant for tests — also allows loopback.
    #[must_use]
    pub const fn permissive_for_tests() -> Self {
        Self { allow_private: true, allow_loopback: true }
    }
}

impl PeerFilter for DefaultPeerFilter {
    fn allow(&self, addr: SocketAddr) -> bool {
        if addr.port() == 0 {
            return false;
        }
        let ip = addr.ip();
        if ip.is_unspecified() || ip.is_multicast() {
            return false;
        }
        if !self.allow_loopback && ip.is_loopback() {
            return false;
        }
        match ip {
            IpAddr::V4(v4) => {
                if v4.is_link_local() || v4.is_broadcast() {
                    return false;
                }
                if !self.allow_private && v4.is_private() {
                    return false;
                }
            }
            IpAddr::V6(v6) => {
                // `Ipv6Addr::is_unicast_link_local` is unstable, so test the
                // `fe80::/10` prefix manually.
                let octets = v6.octets();
                if octets[0] == 0xFE && (octets[1] & 0xC0) == 0x80 {
                    return false;
                }
                // `fc00::/7` = unique-local addresses (the v6 equivalent of
                // RFC 1918).
                if !self.allow_private && (octets[0] & 0xFE) == 0xFC {
                    return false;
                }
            }
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v4(s: &str) -> SocketAddr {
        s.parse().unwrap()
    }
    fn v6(s: &str) -> SocketAddr {
        s.parse().unwrap()
    }

    #[test]
    fn default_rejects_loopback() {
        let f = DefaultPeerFilter::default();
        assert!(!f.allow(v4("127.0.0.1:6881")));
        assert!(!f.allow(v6("[::1]:6881")));
    }

    #[test]
    fn default_rejects_link_local() {
        let f = DefaultPeerFilter::default();
        assert!(!f.allow(v4("169.254.1.1:6881")));
        assert!(!f.allow(v6("[fe80::1]:6881")));
    }

    #[test]
    fn default_rejects_unspecified_and_multicast_and_broadcast() {
        let f = DefaultPeerFilter::default();
        assert!(!f.allow(v4("0.0.0.0:6881")));
        assert!(!f.allow(v6("[::]:6881")));
        assert!(!f.allow(v4("224.0.0.1:6881")));
        assert!(!f.allow(v6("[ff02::1]:6881")));
        assert!(!f.allow(v4("255.255.255.255:6881")));
    }

    #[test]
    fn default_rejects_zero_port() {
        let f = DefaultPeerFilter::default();
        assert!(!f.allow(v4("8.8.8.8:0")));
    }

    #[test]
    fn default_allows_private_by_default() {
        let f = DefaultPeerFilter::default();
        assert!(f.allow(v4("10.0.0.1:6881")));
        assert!(f.allow(v4("172.16.0.1:6881")));
        assert!(f.allow(v4("192.168.1.1:6881")));
        assert!(f.allow(v6("[fc00::1]:6881")));
        assert!(f.allow(v6("[fd00::1]:6881")));
    }

    #[test]
    fn strict_rejects_private() {
        let f = DefaultPeerFilter::strict();
        assert!(!f.allow(v4("10.0.0.1:6881")));
        assert!(!f.allow(v4("192.168.1.1:6881")));
        assert!(!f.allow(v6("[fc00::1]:6881")));
    }

    #[test]
    fn strict_still_rejects_loopback() {
        let f = DefaultPeerFilter::strict();
        assert!(!f.allow(v4("127.0.0.1:6881")));
    }

    #[test]
    fn permissive_allows_loopback() {
        let f = DefaultPeerFilter::permissive_for_tests();
        assert!(f.allow(v4("127.0.0.1:6881")));
        assert!(f.allow(v6("[::1]:6881")));
    }

    #[test]
    fn allows_global_unicast() {
        let f = DefaultPeerFilter::default();
        assert!(f.allow(v4("8.8.8.8:6881")));
        assert!(f.allow(v6("[2001:db8::1]:6881")));
    }
}
