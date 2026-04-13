# Research: lambdaclass/libtorrent-rs — Rust BitTorrent v2 Reference

**Repo URL:** https://github.com/lambdaclass/libtorrent-rs  
**Commit SHA:** 2620a925104c20183ea5ebe4018f5630c88980ab  
**Date:** 2026-04-13

---

## 1. Project Layout

The crate is a small (~7K lines of Rust across 65 files), modular workspace with four members:

- **`bencoder/`** — Generic bencode codec (`Bencode` enum, `ToBencode` trait). Single-pass decode/encode, supports numbers, strings, lists, dicts. No specialized v1/v2 logic.
- **`dtorrent/`** — BitTorrent v1 client (peer protocol, storage, torrent handler). Core modules: `torrent_parser/` (metainfo parsing), `peer/` (handshake, messages, validation), `tracker/` (HTTP announcements), `storage_manager/` (block I/O).
- **`dtracker/`** — Tracker implementation (announce/stats endpoints, peer swarm tracking).
- **`url_encoder/`** — URL encoding utility for tracker queries.

**Status:** Per README, "only V1 is implemented but we're working on V2." This is a v1-focused reference; v2 is roadmapped but absent.

---

## 2. v2 Data Model — THE KEY FINDING

**There is no v2 data model.** The repo is v1-only.

### InfoHash Representation

**v1-only:** `String` (hex-encoded SHA1).

File path: `/dtorrent/src/torrent_parser/torrent.rs`, lines 10–15:

```rust
pub struct Torrent {
    pub announce_url: String,
    pub info: Info,
    pub info_hash: String,  // ← SHA1 as hex string
}
```

Lines 85–99 compute it via bencode-digest:

```rust
pub fn create_info_hash(info: &Info) -> Result<String, FromTorrentError> {
    let bencoded_info = Bencode::encode(info);
    let hash = Sha1::digest(bencoded_info);
    let mut hex_string = String::with_capacity(hash.len() * 2);
    for b in hash {
        match write!(&mut hex_string, "{:02x}", b) {
            Ok(_) => (),
            Err(_) => return Err(FromTorrentError::InfoHashError),
        }
    }
    Ok(hex_string)
}
```

**No enum, no v2 variant, no SHA256 hash.**

### Merkle Hash Trees

**Not implemented.** There is no merkle tree structure, no layer hashing, no BEP 52 verification path.

### BEP 52 Invariants (16 KiB blocks, power-of-two piece sizes)

**Not enforced.** The `Info` struct (lines 6–11 in `/dtorrent/src/torrent_parser/info.rs`) accepts arbitrary piece lengths:

```rust
pub struct Info {
    pub length: i64,
    pub name: String,
    pub piece_length: i64,    // ← No validation; can be any i64
    pub pieces: Vec<u8>,
}
```

No constraints on `piece_length` being a power of two, no block size validation.

### v1 vs v2 vs Hybrid Parsing

**v1 only.** The parser rejects multi-file torrents outright (lines 44–46 in `info.rs`):

```rust
} else if k == b"files" {
    return Err(FromInfoError::MultipleFilesNotSupported);
}
```

No support for `pieces root`, `piece layers`, file trees, or hybrid metadata.

### Single PieceHash Abstraction

**No abstraction.** v1 uses raw byte concatenation; pieces are stored as a flat `Vec<u8>` of 20-byte SHA1 hashes (line 10 in `info.rs`). Extraction is manual (lines 608–612 in `/dtorrent/src/peer/peer_session.rs`):

```rust
let start = (piece_index * 20) as usize;
let end = start + 20;
let real_hash = &self.torrent.info.pieces[start..end];
```

---

## 3. Metainfo Parser

**File:** `/dtorrent/src/torrent_parser/parser.rs`, lines 27–44  
**Top-level function:** `TorrentParser::parse(filepath: &Path) -> Result<Torrent, ParseError>`

**Completeness:**

- **v1:** Full support (announces, info dict, pieces).
- **v2:** Zero support.
- **Hybrid:** Zero support.
- **Multi-file:** Explicitly unsupported (errors on `files` key).

The parser chains three steps:

1. Read file → decode bencode → construct `Torrent` struct
2. Bencode decode (generic, any valid format)
3. Torrent struct validates announce + info presence, computes SHA1 info hash

No per-field validation (piece length, block alignment, etc.).

---

## 4. What's NOT There (Honestly)

This is a working v1 client, but many v2 requirements are missing:

- **No merkle hash trees:** No layer hashing, no hash tree construction/verification.
- **No v2 metainfo:** No `pieces root`, `piece layers`, file trees, or hybrid support.
- **No block-level verification:** Validation is per-piece (SHA1 of full piece); no per-16KiB-block merkle proofs.
- **No picker with merkle logic:** Piece selection is likely simple (download-first-available); no selective block validation.
- **No multi-file support:** Single-file torrents only.
- **No constraint enforcement:** No power-of-two piece-size validation, no BEP 52 structure checks.

**What's there (v1):**
- Peer handshake & bit-field exchange
- Block download & reassembly
- Per-piece SHA1 validation (one-shot, not per-block)
- HTTP tracker announcement
- Storage (save/retrieve blocks by offset)

---

## 5. Hash Verification Path

**File:** `/dtorrent/src/peer/peer_session.rs`, lines 605–623  
**Function:** `PeerSession::validate_piece(piece: &[u8], piece_index: u32)`

Verification is **per-piece, one-shot:**

```rust
fn validate_piece(&self, piece: &[u8], piece_index: u32) -> Result<(), PeerSessionError> {
    let start = (piece_index * 20) as usize;
    let end = start + 20;
    
    let real_hash = &self.torrent.info.pieces[start..end];
    let real_piece_hash = self.convert_to_hex_string(real_hash);
    
    let hash = Sha1::digest(piece);
    let res_piece_hash = self.convert_to_hex_string(hash.as_slice());
    
    if real_piece_hash == res_piece_hash {
        Ok(())
    } else {
        Err(PeerSessionError::PieceHashDoesNotMatch)
    }
}
```

- Computes SHA1 of full downloaded piece
- Looks up expected 20-byte hash at offset `piece_index * 20` in the `pieces` byte vector
- Hex-compares
- **Not merkle-aware:** No per-block tree verification, no partial-piece validation.

---

## 6. Testing Approach

**Test torrents:** Three example `.torrent` files in `/torrents/` (file1–3.torrent, ~32 KB each).

**Code tests:**

- **Bencode codec tests:** Comprehensive (decode/encode strings, numbers, lists, dicts, nesting).  
  File: `/bencoder/src/bencode.rs`, lines 247–444 (150+ tests).
- **Torrent parser tests:** Basic smoke tests for parsing, info hash computation.  
  File: `/dtorrent/src/torrent_parser/torrent.rs`, lines 152–318.
- **Storage tests:** File I/O with offset (save/retrieve blocks).  
  File: `/dtorrent/src/storage_manager/manager.rs`, lines 74–150+.

**No merkle test vectors.** This is a v1 reference; reusable patterns are bencode codec and storage offset semantics only.

---

## 7. What magpie Should Borrow

1. **Bencode codec design**: Enum-based (`BNumber`, `BString`, `BList`, `BDict`) with reversible `ToBencode` trait. Simple, ergonomic, works for v1 and v2.
2. **Torrent struct: `announce_url` + `info` + `info_hash`** separation is clean; leaves room for extended fields (comment, creation date, announce-list, etc.).
3. **Storage offset semantics**: `write_all_at()` / `read_exact_at()` traits for block-level I/O. Generalizes to multi-file layouts.
4. **Hex encoding helper**: The `convert_to_hex_string()` pattern (loop over bytes, `write!` to buffer) is Rust idiomatic and zero-copy-friendly.
5. **Modular crate layout**: Separate `bencoder`, client, tracker concerns into independent crates. Allows reuse.

---

## 8. What magpie Should Avoid

1. **v1-only `String` info hash**: Don't hard-code hex encoding. Use a tagged type that can hold v1 (SHA1) and v2 (SHA256) simultaneously and print appropriately.
2. **`Vec<u8>` pieces with manual slicing**: This is fragile for v2 (merkle trees have variable depth). Abstract behind a `PieceHash` trait or enum.
3. **No constraint validation**: Don't accept arbitrary `piece_length` values. Validate power-of-two (or powers of 2 for v2 layers) at parse time or via the type system.

---

## 9. Critical ADR Input: Proposed InfoHash & PieceHash Types

Based on the found patterns and v2 requirements, propose:

```rust
// InfoHash: tagged to hold v1 (Sha1) and v2 (Sha256)
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum InfoHash {
    V1(Sha1Digest),  // 20 bytes
    V2(Sha256Digest), // 32 bytes
}

// Or, if hybrid is needed:
pub struct InfoHash {
    v1: Option<Sha1Digest>,
    v2: Option<Sha256Digest>,
}

// PieceHash: abstraction over v1 SHA1, v2 leaf, and v2 merkle nodes
pub enum PieceHash {
    V1(Sha1Digest),
    V2Leaf(Sha256Digest),
    V2Node(Sha256Digest, /* depth/index */),
}

// Or, simpler for v2-only focus:
pub enum Hash {
    Sha1([u8; 20]),
    Sha256([u8; 32]),
}
pub type InfoHash = Hash;
pub type PieceHash = Hash;
```

**Rationale:**
- Enum tags avoid silent misuse (wrong hash type for protocol version).
- Fixed-size byte arrays `[u8; 20/32]` avoid heap allocation, enable zero-copy verification.
- Hybrid `Option<V1>, Option<V2>` plays well with BEP 52 hybrid mode (both hashes present).
- Merkle node tagging (if storing trees) enables type-safe layer navigation.

---

## Summary

**lambdaclass/libtorrent-rs is a v1-complete, v2-absent reference.** Use it for:
- Bencode codec patterns
- Hex encoding and storage I/O traits
- Modular crate organization

**Do not copy for v2 work:**
- No merkle abstractions exist
- No power-of-two enforcement
- No multi-file or hybrid parsing

**For magpie's v2 data model, borrow the enum-tagged philosophy (v1 vs v2 info hash, piece hash abstraction), and introduce merkle tree handling from scratch or from BEP 52 reference implementations (e.g., Transmission, libtorrent-rasterbar).**

