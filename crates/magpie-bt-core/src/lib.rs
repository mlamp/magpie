//! Engine core for magpie — piece picker, storage trait, alert ring,
//! session orchestration.
//!
//! `unsafe` is restricted to documented syscall wrappers (see
//! [`docs/DISCIPLINES.md`][disc]). Every `unsafe` block must carry a
//! `// SAFETY:` comment.
//!
//! Pre-M0 placeholder. Real implementation lands during the M0 milestone
//! (see `docs/milestones/001-foundations.md`).
//!
//! [disc]: https://github.com/mlamp/magpie/blob/main/docs/DISCIPLINES.md
#![deny(unsafe_code)]
#![doc = include_str!("../README.md")]

#[cfg(test)]
mod smoke {
    #[test]
    fn smoke() {}
}
