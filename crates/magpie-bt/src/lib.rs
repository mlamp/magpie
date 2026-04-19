//! Facade crate for the [magpie](https://github.com/mlamp/magpie) BitTorrent library.
//!
//! Re-exports the stable public API from the member crates so consumers can
//! depend on a single crate (`magpie-bt`) rather than chasing the individual
//! `magpie-bt-*` subcrates.
//!
//! As of M0, the surface covers:
//!
//! - [`bencode`] — zero-copy bencode codec (`decode`, `encode`, `skip_value`,
//!   `dict_value_span`, `Value`).
//! - [`metainfo`] — `.torrent` parser (`parse`, `MetaInfo`, `InfoHash`, v1/v2
//!   file tree types).
//! - [`alerts`], [`peer_id`], [`picker`], [`storage`] from the engine core.
//! - [`engine`], [`peer_filter`], [`session`], [`tracker`], [`wire`] — the
//!   M1 network surface (HTTP tracker, BEP 3 + BEP 6 wire codec, per-torrent
//!   actor, multi-torrent [`Engine`]).
//!
//! # Example
//! ```
//! use magpie_bt::metainfo::parse;
//!
//! let bytes: &[u8] = b"d4:infod6:lengthi13e4:name5:hello\
//!                      12:piece lengthi32768e\
//!                      6:pieces20:aaaaaaaaaaaaaaaaaaaaee";
//! let meta = parse(bytes).unwrap();
//! assert_eq!(meta.info.name, b"hello");
//! assert!(meta.info_hash.v1().is_some());
//! ```
#![forbid(unsafe_code)]
#![doc = include_str!("../README.md")]

pub use magpie_bt_bencode as bencode;
pub use magpie_bt_core::{alerts, engine, peer_filter, peer_id, picker, session, storage, tracker};
pub use magpie_bt_metainfo as metainfo;
pub use magpie_bt_wire as wire;

// Convenience re-exports of the most common types at the crate root.
pub use magpie_bt_core::alerts::{Alert, AlertCategory, AlertQueue};
pub use magpie_bt_core::engine::{
    AddPeerError, AddTorrentError, AddTorrentRequest, AttachTrackerConfig, Engine, ListenConfig,
    TorrentId, TorrentNotFoundError, TorrentStateView,
};
pub use magpie_bt_core::peer_filter::{DefaultPeerFilter, PeerFilter};
pub use magpie_bt_core::peer_id::PeerIdBuilder;
pub use magpie_bt_core::picker::Picker;
pub use magpie_bt_core::session::resume::{
    FileResumeSink, ResumeSink, ResumeSinkError, ResumeSnapshot,
};
pub use magpie_bt_core::session::stats::StatsSnapshot;
pub use magpie_bt_core::session::stats::sink::{FileStatsSink, StatsSink};
pub use magpie_bt_core::session::{TorrentParams, TorrentState};
#[cfg(unix)]
pub use magpie_bt_core::storage::{FdPool, FileSpec, FileStorage, MultiFileStorage};
pub use magpie_bt_core::storage::{MemoryStorage, Storage, StorageError};
pub use magpie_bt_core::tracker::{HttpTracker, Tracker, UdpTracker};
pub use magpie_bt_metainfo::{InfoHash, MetaInfo, ParseError, parse};
