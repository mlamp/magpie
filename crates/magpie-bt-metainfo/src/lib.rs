//! `.torrent` metainfo parser for magpie — v1, v2, and hybrid.
//!
//! Pre-M0 placeholder. The real parser + `InfoHash`/`PieceHash` abstractions
//! land during the M0 milestone
//! (see `docs/milestones/001-foundations.md`).
#![forbid(unsafe_code)]
#![doc = include_str!("../README.md")]

#[cfg(test)]
mod smoke {
    #[test]
    fn smoke() {}
}
