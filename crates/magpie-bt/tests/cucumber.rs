#![allow(clippy::missing_const_for_fn, clippy::trivial_regex,
    clippy::missing_panics_doc, clippy::needless_pass_by_value,
    clippy::missing_fields_in_debug)]
//! Cucumber (BDD) test harness for magpie.
//!
//! Features live in `tests/features/`, indexed by BEP number. See
//! `docs/bep-coverage.md` for the live coverage matrix.
use std::net::SocketAddr;
use std::sync::Arc;

use bytes::BytesMut;
use cucumber::World;
use magpie_bt_core::tracker::tiered::TieredTracker;
use magpie_bt_core::tracker::{AnnounceResponse, TrackerError};
use magpie_bt_wire::{Handshake, Message};

#[derive(World, Default)]
pub(crate) struct MagpieWorld {
    // Wire-level scratch space.
    handshake: Option<Handshake>,
    decoded_handshake: Option<Handshake>,
    pending_message: Option<Message>,
    encoded_buf: BytesMut,
    decoded_message: Option<Message>,
    // Tracker-level scratch space.
    announce_bytes: Vec<u8>,
    announce_response: Option<AnnounceResponse>,
    announce_error: Option<TrackerError>,
    // BEP 12 multi-tracker scratch space.
    tiered: Option<Arc<TieredTracker>>,
    tiered_a: Option<Arc<TieredTracker>>,
    tiered_b: Option<Arc<TieredTracker>>,
    /// Pointer-of-Tracker → human label (e.g. "failing", "working"). Used
    /// to compare `tier_order` snapshots across announce calls.
    tier_labels: Vec<(usize, &'static str)>,
    tiered_peer_count: Option<usize>,
    // BEP 15 UDP scratch space.
    udp_txid: u32,
    udp_buf: Vec<u8>,
    udp_decoded_conn_id: Option<u64>,
    udp_decode_failed: bool,
    udp_retry_secs: Vec<u64>,
    // BEP 27 metainfo + session scratch space.
    metainfo_bytes: Vec<u8>,
    metainfo_private: Option<bool>,
    session_private: Option<bool>,
}

impl std::fmt::Debug for MagpieWorld {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MagpieWorld")
            .field("encoded_buf_len", &self.encoded_buf.len())
            .field("announce_response", &self.announce_response)
            .field("announce_error", &self.announce_error)
            .field("tiered_set", &self.tiered.is_some())
            .field("tiered_peer_count", &self.tiered_peer_count)
            .field("udp_txid", &self.udp_txid)
            .field("metainfo_private", &self.metainfo_private)
            .field("session_private", &self.session_private)
            .finish()
    }
}

impl MagpieWorld {
    fn last_decoded(&self) -> &Message {
        self.decoded_message.as_ref().expect("a message must be decoded first")
    }

    fn parsed_peers(&self) -> Vec<SocketAddr> {
        self.announce_response.as_ref().expect("response parsed").peers.clone()
    }
}

mod steps;

#[tokio::main]
async fn main() {
    MagpieWorld::run("tests/features").await;
}

