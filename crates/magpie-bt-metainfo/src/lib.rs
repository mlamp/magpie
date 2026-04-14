//! `.torrent` metainfo parser for magpie — v1, v2, and hybrid.
//!
//! Parse a byte slice with [`parse`]; the returned [`MetaInfo`] borrows every
//! byte string from the input (zero-copy). The exact bytes that were hashed to
//! produce [`MetaInfo::info_hash`] are preserved in [`MetaInfo::info_bytes`],
//! which allows consumers to verify, relay, or re-broadcast the info dict
//! without re-encoding.
//!
//! ## Supported variants
//! - BEP 3 v1: `info.pieces` + (`info.length` | `info.files`).
//! - BEP 52 v2: `info.file tree` + `info.meta version = 2`.
//! - Hybrid: both forms present, yielding [`InfoHash::Hybrid`].
//!
//! ## Example
//! ```
//! use magpie_bt_metainfo::parse;
//!
//! // A hand-written single-file v1 torrent (no announce).
//! let bytes: &[u8] = b"d4:infod6:lengthi13e\
//!     4:name5:hello\
//!     12:piece lengthi32768e\
//!     6:pieces20:aaaaaaaaaaaaaaaaaaaaee";
//! let meta = parse(bytes).unwrap();
//! assert!(meta.info.is_v1_only());
//! assert_eq!(meta.info.name, b"hello");
//! assert!(meta.info_hash.v1().is_some());
//! ```
#![forbid(unsafe_code)]

mod error;
mod info_hash;
mod parse;
#[cfg(any(feature = "test-support", test))]
pub mod test_support;
mod types;

pub use error::{ParseError, ParseErrorKind};
pub use info_hash::{InfoHash, sha1, sha256};
pub use parse::parse;
pub use types::{FileListV1, FileTreeNode, FileV1, Info, InfoV1, InfoV2, MetaInfo};
