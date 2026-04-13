//! Peer wire protocol codec (framing, messages, extension protocol).
//!
//! Pre-M0 placeholder. Real implementation lands in M1 (leecher + TCP + v1
//! wire); see `docs/milestones/001-foundations.md` for the prerequisite M0
//! deliverables.
#![forbid(unsafe_code)]
#![doc = include_str!("../README.md")]

#[cfg(test)]
mod smoke {
    #[test]
    fn smoke() {}
}
