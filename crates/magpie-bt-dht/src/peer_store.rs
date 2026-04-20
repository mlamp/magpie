//! `announce_peer` / `get_peers` backing store.
//!
//! Maps an info-hash to the peers we've most recently heard from
//! via `announce_peer`. The store caps both the number of tracked
//! torrents and the peers-per-torrent so a hostile swarm cannot
//! exhaust memory; entries also age out after a configurable
//! staleness window (30 min defaults, matching typical BEP 5
//! re-announce cadence).
//!
//! Pure in-memory state; the DHT task drives [`Self::announce`] on
//! inbound `announce_peer` handlers and [`Self::peers_for`] on
//! inbound `get_peers`.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::time::{Duration, Instant};

use crate::krpc::InfoHash;

// ---------------------------------------------------------------------------
// Defaults
// ---------------------------------------------------------------------------

/// Maximum torrents tracked simultaneously. A slot-full store evicts
/// the least-recently-announced torrent on the next `announce`.
pub const DEFAULT_MAX_TORRENTS: usize = 1_000;

/// Maximum peers tracked per torrent. A slot-full torrent evicts its
/// stalest peer on the next `announce`.
pub const DEFAULT_MAX_PEERS_PER_TORRENT: usize = 100;

/// Staleness window. Peers we haven't heard `announce_peer` from in
/// this long are pruned by [`PeerStore::sweep`]. Matches typical
/// BEP 5 re-announce cadence (≈ 30 min).
pub const DEFAULT_PEER_STALENESS: Duration = Duration::from_secs(30 * 60);

// ---------------------------------------------------------------------------
// Config
// ---------------------------------------------------------------------------

/// Runtime knobs for [`PeerStore`]. Defaults map to the
/// `DEFAULT_*` constants above.
#[derive(Debug, Clone, Copy)]
pub struct PeerStoreConfig {
    /// Cap on tracked torrents.
    pub max_torrents: usize,
    /// Cap on peers per torrent.
    pub max_peers_per_torrent: usize,
    /// Staleness window past which a peer is pruned by
    /// [`PeerStore::sweep`].
    pub peer_staleness: Duration,
}

impl Default for PeerStoreConfig {
    fn default() -> Self {
        Self {
            max_torrents: DEFAULT_MAX_TORRENTS,
            max_peers_per_torrent: DEFAULT_MAX_PEERS_PER_TORRENT,
            peer_staleness: DEFAULT_PEER_STALENESS,
        }
    }
}

// ---------------------------------------------------------------------------
// PeerStore
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct TorrentEntry {
    peers: HashMap<SocketAddr, Instant>,
    last_announced: Instant,
}

/// In-memory map of info-hashes to recent peers.
#[derive(Debug)]
pub struct PeerStore {
    config: PeerStoreConfig,
    torrents: HashMap<InfoHash, TorrentEntry>,
}

impl PeerStore {
    /// Empty store.
    #[must_use]
    pub fn new(config: PeerStoreConfig) -> Self {
        Self {
            config,
            torrents: HashMap::new(),
        }
    }

    /// Record `peer` as announcing for `info_hash` at `now`. If the
    /// torrent is new and the store is full, the least-recently-
    /// announced torrent is evicted to make room. If the torrent
    /// is known but its peer set is full, the stalest peer is
    /// evicted first.
    pub fn announce(&mut self, info_hash: InfoHash, peer: SocketAddr, now: Instant) {
        if !self.torrents.contains_key(&info_hash)
            && self.torrents.len() >= self.config.max_torrents
        {
            self.evict_oldest_torrent();
        }
        let entry = self
            .torrents
            .entry(info_hash)
            .or_insert_with(|| TorrentEntry {
                peers: HashMap::new(),
                last_announced: now,
            });
        entry.last_announced = now;
        if !entry.peers.contains_key(&peer)
            && entry.peers.len() >= self.config.max_peers_per_torrent
        {
            evict_stalest_peer(entry);
        }
        entry.peers.insert(peer, now);
    }

    /// Return up to `limit` peers for `info_hash`, most-recently-
    /// announced first. Empty when the hash is unknown.
    #[must_use]
    pub fn peers_for(&self, info_hash: &InfoHash, limit: usize) -> Vec<SocketAddr> {
        let Some(entry) = self.torrents.get(info_hash) else {
            return Vec::new();
        };
        let mut peers: Vec<(&SocketAddr, &Instant)> = entry.peers.iter().collect();
        peers.sort_by(|(_, a), (_, b)| b.cmp(a));
        peers
            .into_iter()
            .take(limit)
            .map(|(addr, _)| *addr)
            .collect()
    }

    /// Drop peers older than [`PeerStoreConfig::peer_staleness`];
    /// drop torrents whose peer set has emptied.
    pub fn sweep(&mut self, now: Instant) {
        let threshold = self.config.peer_staleness;
        for entry in self.torrents.values_mut() {
            entry
                .peers
                .retain(|_, seen| now.saturating_duration_since(*seen) < threshold);
        }
        self.torrents.retain(|_, e| !e.peers.is_empty());
    }

    /// Count of tracked torrents.
    #[must_use]
    pub fn torrent_count(&self) -> usize {
        self.torrents.len()
    }

    /// Count of peers known for `info_hash` (0 when unknown).
    #[must_use]
    pub fn peer_count(&self, info_hash: &InfoHash) -> usize {
        self.torrents.get(info_hash).map_or(0, |t| t.peers.len())
    }

    fn evict_oldest_torrent(&mut self) {
        if let Some((stale, _)) = self
            .torrents
            .iter()
            .min_by_key(|(_, e)| e.last_announced)
            .map(|(k, v)| (*k, v.last_announced))
        {
            self.torrents.remove(&stale);
        }
    }
}

fn evict_stalest_peer(entry: &mut TorrentEntry) {
    if let Some((stale, _)) = entry
        .peers
        .iter()
        .min_by_key(|(_, seen)| **seen)
        .map(|(k, v)| (*k, *v))
    {
        entry.peers.remove(&stale);
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{IpAddr, Ipv4Addr};

    fn hash(b: u8) -> InfoHash {
        InfoHash::from_bytes([b; 20])
    }

    fn peer(n: u8) -> SocketAddr {
        SocketAddr::new(IpAddr::V4(Ipv4Addr::new(10, 0, 0, n)), u16::from(n) + 6881)
    }

    #[test]
    fn announce_and_peers_for_roundtrip() {
        let t0 = Instant::now();
        let mut store = PeerStore::new(PeerStoreConfig::default());
        store.announce(hash(1), peer(1), t0);
        let got = store.peers_for(&hash(1), 8);
        assert_eq!(got, vec![peer(1)]);
    }

    #[test]
    fn peers_for_unknown_hash_is_empty() {
        let store = PeerStore::new(PeerStoreConfig::default());
        assert!(store.peers_for(&hash(0xff), 8).is_empty());
    }

    #[test]
    fn peers_for_respects_limit() {
        let t0 = Instant::now();
        let mut store = PeerStore::new(PeerStoreConfig::default());
        for i in 1..=10 {
            store.announce(hash(1), peer(i), t0 + Duration::from_secs(u64::from(i)));
        }
        let got = store.peers_for(&hash(1), 3);
        assert_eq!(got.len(), 3);
    }

    #[test]
    fn peers_for_orders_by_recency() {
        let t0 = Instant::now();
        let mut store = PeerStore::new(PeerStoreConfig::default());
        store.announce(hash(1), peer(1), t0);
        store.announce(hash(1), peer(2), t0 + Duration::from_secs(60));
        store.announce(hash(1), peer(3), t0 + Duration::from_secs(120));
        let got = store.peers_for(&hash(1), 3);
        assert_eq!(got, vec![peer(3), peer(2), peer(1)]);
    }

    #[test]
    fn announce_updates_last_seen() {
        let t0 = Instant::now();
        let mut store = PeerStore::new(PeerStoreConfig::default());
        store.announce(hash(1), peer(1), t0);
        store.announce(hash(1), peer(1), t0 + Duration::from_secs(60));
        assert_eq!(store.peer_count(&hash(1)), 1);
    }

    #[test]
    fn sweep_removes_stale_peers() {
        let t0 = Instant::now();
        let mut store = PeerStore::new(PeerStoreConfig {
            peer_staleness: Duration::from_secs(60),
            ..PeerStoreConfig::default()
        });
        store.announce(hash(1), peer(1), t0);
        store.announce(hash(1), peer(2), t0 + Duration::from_secs(40));
        // Sweep at 80 s: peer 1 is 80 s old (> 60, pruned); peer 2
        // is 40 s old (< 60, kept).
        store.sweep(t0 + Duration::from_secs(80));
        let got = store.peers_for(&hash(1), 8);
        assert_eq!(got, vec![peer(2)]);
    }

    #[test]
    fn sweep_removes_empty_torrents() {
        let t0 = Instant::now();
        let mut store = PeerStore::new(PeerStoreConfig {
            peer_staleness: Duration::from_secs(60),
            ..PeerStoreConfig::default()
        });
        store.announce(hash(1), peer(1), t0);
        store.sweep(t0 + Duration::from_secs(120));
        assert_eq!(store.torrent_count(), 0);
    }

    #[test]
    fn torrent_cap_evicts_oldest() {
        let t0 = Instant::now();
        let mut store = PeerStore::new(PeerStoreConfig {
            max_torrents: 2,
            ..PeerStoreConfig::default()
        });
        store.announce(hash(1), peer(1), t0);
        store.announce(hash(2), peer(2), t0 + Duration::from_secs(1));
        store.announce(hash(3), peer(3), t0 + Duration::from_secs(2));
        // hash(1) is oldest and got evicted.
        assert_eq!(store.torrent_count(), 2);
        assert!(store.peers_for(&hash(1), 8).is_empty());
        assert!(!store.peers_for(&hash(2), 8).is_empty());
        assert!(!store.peers_for(&hash(3), 8).is_empty());
    }

    #[test]
    fn per_torrent_peer_cap_evicts_stalest() {
        let t0 = Instant::now();
        let mut store = PeerStore::new(PeerStoreConfig {
            max_peers_per_torrent: 2,
            ..PeerStoreConfig::default()
        });
        store.announce(hash(1), peer(1), t0);
        store.announce(hash(1), peer(2), t0 + Duration::from_secs(1));
        store.announce(hash(1), peer(3), t0 + Duration::from_secs(2));
        // peer 1 is stalest → evicted.
        let got = store.peers_for(&hash(1), 8);
        assert_eq!(got.len(), 2);
        assert!(!got.contains(&peer(1)));
        assert!(got.contains(&peer(2)));
        assert!(got.contains(&peer(3)));
    }
}
