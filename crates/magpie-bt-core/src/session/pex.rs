//! BEP 11 Peer Exchange session state.
//!
//! Tracks the diff between PEX rounds (rakshasa-style change-tracking) and
//! enforces rate limiting (one PEX round per minute per peer, max 50 peers
//! per message).

use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;

use tokio::time::Instant;

use magpie_bt_wire::pex::{PexFlags, PexMessage, PexPeer};

use crate::session::messages::PeerSlot;

/// Maximum peers we advertise per outbound PEX message (added or dropped).
/// We cap our own advertisements conservatively at 50, but accept larger
/// inbound messages from peers (the wire codec imposes its own limit of ~200
/// peers per compact-address field).
const MAX_PEX_ADVERTISE: usize = 50;

/// Minimum interval between outbound PEX sends to the same peer.
pub const PEX_INTERVAL: std::time::Duration = std::time::Duration::from_mins(1);

/// Minimum interval between accepting inbound PEX messages from the same
/// peer. Messages arriving faster than this are silently dropped to prevent
/// a peer from flooding us with discovery addresses.
pub const PEX_INBOUND_INTERVAL: std::time::Duration = std::time::Duration::from_secs(10);

/// Tracks PEX state for a single torrent session.
pub struct PexState {
    /// Peers we last advertised in our PEX message. Used to compute
    /// the diff (added/dropped) for the next round.
    last_advertised: HashSet<SocketAddr>,
    /// When we last sent a PEX message to each peer.
    last_sent: HashMap<PeerSlot, Instant>,
    /// When we last accepted an inbound PEX message from each peer.
    /// Used to enforce [`PEX_INBOUND_INTERVAL`] rate limiting.
    last_received: HashMap<PeerSlot, Instant>,
    /// Whether PEX is enabled for this torrent (disabled for private).
    enabled: bool,
}

impl PexState {
    /// Create a new PEX state. Disabled if the torrent is private (BEP 27).
    #[must_use]
    pub fn new(private: bool) -> Self {
        Self {
            last_advertised: HashSet::new(),
            last_sent: HashMap::new(),
            last_received: HashMap::new(),
            enabled: !private,
        }
    }

    /// Whether PEX is enabled for this torrent.
    #[must_use]
    pub const fn is_enabled(&self) -> bool {
        self.enabled
    }

    /// Returns `true` if at least 60 seconds have elapsed since the last
    /// PEX message was sent to this peer (or if we've never sent one).
    #[must_use]
    pub fn should_send_to(&self, slot: PeerSlot, now: Instant) -> bool {
        self.last_sent
            .get(&slot)
            .is_none_or(|last| now.duration_since(*last) >= PEX_INTERVAL)
    }

    /// Compute the diff between the current connected peers and what we last
    /// advertised. Updates `last_advertised` and returns a [`PexMessage`] if
    /// non-empty. Caps at 50 added + 50 dropped per message.
    pub fn build_message(
        &mut self,
        current_peers: &HashMap<PeerSlot, SocketAddr>,
    ) -> Option<PexMessage> {
        let current_addrs: HashSet<SocketAddr> = current_peers.values().copied().collect();

        let added: Vec<PexPeer> = current_addrs
            .difference(&self.last_advertised)
            .take(MAX_PEX_ADVERTISE)
            .map(|addr| PexPeer {
                addr: *addr,
                flags: PexFlags::default(),
            })
            .collect();

        let dropped: Vec<SocketAddr> = self
            .last_advertised
            .difference(&current_addrs)
            .take(MAX_PEX_ADVERTISE)
            .copied()
            .collect();

        if added.is_empty() && dropped.is_empty() {
            return None;
        }

        // Update last_advertised to reflect what we're sending.
        self.last_advertised = current_addrs;

        Some(PexMessage { added, dropped })
    }

    /// Record that we sent a PEX message to this peer at the given time.
    pub fn record_sent(&mut self, slot: PeerSlot, now: Instant) {
        self.last_sent.insert(slot, now);
    }

    /// Returns `true` if we should accept an inbound PEX message from this
    /// peer (at least [`PEX_INBOUND_INTERVAL`] since the last one, or first
    /// message from this peer).
    #[must_use]
    pub fn should_accept_from(&self, slot: PeerSlot, now: Instant) -> bool {
        self.last_received
            .get(&slot)
            .is_none_or(|last| now.duration_since(*last) >= PEX_INBOUND_INTERVAL)
    }

    /// Record that we accepted an inbound PEX message from this peer.
    pub fn record_received(&mut self, slot: PeerSlot, now: Instant) {
        self.last_received.insert(slot, now);
    }

    /// Remove tracking state for a disconnected peer.
    pub fn peer_disconnected(&mut self, slot: PeerSlot) {
        self.last_sent.remove(&slot);
        self.last_received.remove(&slot);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn addr(port: u16) -> SocketAddr {
        use std::net::{Ipv4Addr, SocketAddrV4};
        SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(10, 0, 0, 1), port))
    }

    #[test]
    fn private_torrent_disables_pex() {
        let pex = PexState::new(true);
        assert!(!pex.is_enabled());
    }

    #[test]
    fn public_torrent_enables_pex() {
        let pex = PexState::new(false);
        assert!(pex.is_enabled());
    }

    #[test]
    fn empty_diff_returns_none() {
        let mut pex = PexState::new(false);
        let peers = HashMap::new();
        assert!(pex.build_message(&peers).is_none());
    }

    #[test]
    fn build_message_added() {
        let mut pex = PexState::new(false);
        let mut peers = HashMap::new();
        peers.insert(PeerSlot(0), addr(6881));
        peers.insert(PeerSlot(1), addr(6882));

        let msg = pex.build_message(&peers).unwrap();
        assert_eq!(msg.added.len(), 2);
        assert!(msg.dropped.is_empty());

        // Second call with same peers should return None.
        assert!(pex.build_message(&peers).is_none());
    }

    #[test]
    fn build_message_dropped() {
        let mut pex = PexState::new(false);
        let mut peers = HashMap::new();
        peers.insert(PeerSlot(0), addr(6881));
        peers.insert(PeerSlot(1), addr(6882));

        // First round: both added.
        let _ = pex.build_message(&peers);

        // Remove one peer.
        peers.remove(&PeerSlot(1));
        let msg = pex.build_message(&peers).unwrap();
        assert!(msg.added.is_empty());
        assert_eq!(msg.dropped.len(), 1);
        assert_eq!(msg.dropped[0], addr(6882));
    }

    #[test]
    fn build_message_caps_at_50() {
        let mut pex = PexState::new(false);
        let mut peers = HashMap::new();
        for i in 0..100u16 {
            peers.insert(PeerSlot(u64::from(i)), addr(6000 + i));
        }

        let msg = pex.build_message(&peers).unwrap();
        assert!(msg.added.len() <= MAX_PEX_ADVERTISE);
    }

    #[test]
    fn should_send_to_respects_interval() {
        let mut pex = PexState::new(false);
        let slot = PeerSlot(0);
        let now = Instant::now();

        // Never sent before — should send.
        assert!(pex.should_send_to(slot, now));

        // Record a send.
        pex.record_sent(slot, now);

        // Immediately after — should not send.
        assert!(!pex.should_send_to(slot, now));

        // After 60 seconds — should send.
        let later = now + PEX_INTERVAL;
        assert!(pex.should_send_to(slot, later));
    }

    #[test]
    fn peer_disconnected_clears_tracking() {
        let mut pex = PexState::new(false);
        let slot = PeerSlot(0);
        pex.record_sent(slot, Instant::now());
        pex.peer_disconnected(slot);
        // After disconnect, should_send_to returns true (no record).
        assert!(pex.should_send_to(slot, Instant::now()));
    }
}
