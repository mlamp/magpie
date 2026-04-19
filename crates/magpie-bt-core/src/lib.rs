//! Engine core for magpie — piece picker, storage trait, alert ring, peer-id
//! builder, tracker client, and (from M1 phase 3) session orchestration.
//!
//! `unsafe` is restricted to documented syscall wrappers (see
//! [`docs/DISCIPLINES.md`][disc]). Every `unsafe` block must carry a
//! `// SAFETY:` comment.
//!
//! [disc]: https://github.com/mlamp/magpie/blob/main/docs/DISCIPLINES.md
#![deny(unsafe_code)]
#![doc = include_str!("../README.md")]

pub mod alerts;
pub mod engine;
pub mod ids;
pub mod lsd;

pub use ids::{PeerSlot, TorrentId};
#[cfg(feature = "prometheus")]
pub mod metrics_exporter;
pub mod peer_filter;
pub mod peer_id;
pub mod picker;
pub mod session;
pub mod storage;
pub mod tracker;
