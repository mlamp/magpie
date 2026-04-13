//! Facade crate for the magpie BitTorrent library.
//!
//! Re-exports the public API from the member crates. Consumers should depend
//! on this crate rather than the individual `magpie-bt-*` subcrates.
//!
//! Pre-M0 placeholder — no surface is exposed yet.
#![forbid(unsafe_code)]
#![doc = include_str!("../README.md")]

#[cfg(test)]
mod smoke {
    #[test]
    fn smoke() {}
}
