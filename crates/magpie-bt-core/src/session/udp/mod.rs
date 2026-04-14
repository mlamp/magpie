//! UDP demux (ADR-0015): one socket, first-byte dispatch. M2 ships with
//! the tracker subscriber only; M3 DHT and M4 uTP plug in via `None → Some`
//! registrations without rewiring.

pub mod demux;

pub use demux::{DemuxError, TrackerResponse, UdpDemux, DEFAULT_TRACKER_TXN_TTL};
