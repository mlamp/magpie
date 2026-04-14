//! Typed view of a `.torrent` metainfo document.

use std::collections::BTreeMap;

use crate::info_hash::InfoHash;

/// Parsed `.torrent` metainfo, borrowing byte strings from the source buffer.
#[derive(Debug, Clone)]
pub struct MetaInfo<'a> {
    /// Announce URL (HTTP tracker), if present.
    pub announce: Option<&'a [u8]>,
    /// Multi-tracker list (BEP 12), if present.
    pub announce_list: Option<Vec<Vec<&'a [u8]>>>,
    /// Human-readable comment.
    pub comment: Option<&'a [u8]>,
    /// `created by` client identifier.
    pub created_by: Option<&'a [u8]>,
    /// Unix timestamp of creation, if present.
    pub creation_date: Option<i64>,
    /// Optional dictionary encoding in the torrent's text fields (BEP 3).
    pub encoding: Option<&'a [u8]>,
    /// The info dictionary, parsed into its typed view.
    pub info: Info<'a>,
    /// Raw bytes of the info dictionary in the source buffer — the bytes that
    /// were hashed to produce [`MetaInfo::info_hash`]. Useful for verifying
    /// downloaded metadata (BEP 9) or re-broadcasting verbatim.
    pub info_bytes: &'a [u8],
    /// Info-hash computed from [`MetaInfo::info_bytes`].
    pub info_hash: InfoHash,
}

/// Typed view of the `info` dictionary.
///
/// A torrent can carry v1-only, v2-only, or hybrid info dicts. The [`v1`] and
/// [`v2`] fields hold the view for whichever variants are present; either the
/// v1 or v2 side (or both, for hybrid) will be `Some`.
///
/// [`v1`]: Info::v1
/// [`v2`]: Info::v2
#[derive(Debug, Clone)]
pub struct Info<'a> {
    /// Suggested base name for the file/directory. Always present.
    pub name: &'a [u8],
    /// Piece length in bytes. Always a positive power of two.
    pub piece_length: u64,
    /// If `true`, this torrent is private (BEP 27) — trackers should not
    /// distribute peers to the DHT or to PEX.
    pub private: bool,
    /// v1 view (`pieces` blob + file list), when present.
    pub v1: Option<InfoV1<'a>>,
    /// v2 view (file tree + merkle roots), when present.
    pub v2: Option<InfoV2<'a>>,
}

impl Info<'_> {
    /// `true` if this is a v1-only info dict.
    #[must_use]
    pub const fn is_v1_only(&self) -> bool {
        self.v1.is_some() && self.v2.is_none()
    }

    /// `true` if this is a v2-only info dict.
    #[must_use]
    pub const fn is_v2_only(&self) -> bool {
        self.v2.is_some() && self.v1.is_none()
    }

    /// `true` if this is a hybrid info dict (both v1 and v2 views populated).
    #[must_use]
    pub const fn is_hybrid(&self) -> bool {
        self.v1.is_some() && self.v2.is_some()
    }
}

/// v1-specific fields of an info dict.
#[derive(Debug, Clone)]
pub struct InfoV1<'a> {
    /// Concatenated SHA-1 piece hashes. Length is always a multiple of 20.
    pub pieces: &'a [u8],
    /// File layout (single- or multi-file).
    pub files: FileListV1<'a>,
}

/// v1 file listing.
#[derive(Debug, Clone)]
pub enum FileListV1<'a> {
    /// Single-file torrent: the `info` dict itself carries the file length.
    Single {
        /// File length in bytes.
        length: u64,
    },
    /// Multi-file torrent.
    Multi {
        /// Ordered list of files.
        files: Vec<FileV1<'a>>,
    },
}

/// v1 file entry in a multi-file torrent.
#[derive(Debug, Clone)]
pub struct FileV1<'a> {
    /// File length in bytes.
    pub length: u64,
    /// Path components (each already validated to be non-empty and free of `/`).
    pub path: Vec<&'a [u8]>,
}

/// v2-specific fields of an info dict (BEP 52).
#[derive(Debug, Clone)]
pub struct InfoV2<'a> {
    /// Value of the `meta version` key. Currently BEP 52 defines only `2`.
    pub meta_version: u64,
    /// Root of the file tree.
    pub file_tree: FileTreeNode<'a>,
}

/// A node in a v2 file tree.
///
/// Each entry in a v2 `file tree` dictionary is either a directory (a map of
/// further children) or a file (a dict whose empty key `""` maps to a leaf).
/// This enum flattens the BEP 52 encoding into an easily-navigated tree.
#[derive(Debug, Clone)]
pub enum FileTreeNode<'a> {
    /// A file leaf. `pieces_root` is the root of that file's merkle tree
    /// (present for files ≥ one piece; absent for empty files).
    File {
        /// File length in bytes.
        length: u64,
        /// Merkle root of the file's piece layer, if present.
        pieces_root: Option<[u8; 32]>,
    },
    /// A directory containing named children.
    Dir(BTreeMap<&'a [u8], Self>),
}
