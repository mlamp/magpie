//! Storage abstraction — block-granular persistence for torrent piece data.
//!
//! The engine never sees concrete files; it speaks to a [`Storage`]
//! implementation, which hides the difference between real on-disk files and
//! in-memory buffers used in tests. Implementations are expected to be
//! thread-safe (`Send + Sync`) because piece verification and peer sessions
//! can issue concurrent reads/writes.
//!
//! Magpie ships two implementations:
//!
//! - [`MemoryStorage`] — `Vec`-backed; used by tests, examples, and
//!   BEP 9 magnet-metadata fetches before on-disk files are allocated.
//! - [`FileStorage`] (**Unix only**) — file-backed using the stdlib
//!   positional-I/O wrappers. Vectorised `preadv`/`pwritev` via direct `libc`
//!   calls lands behind the unsafe allowlist — see ADR-0008. Windows support
//!   requires a different backend (overlapped I/O or `io_ring`) and is out of
//!   scope for M0.
//!
//! # Example
//! ```
//! use magpie_bt_core::storage::{MemoryStorage, Storage};
//!
//! let s = MemoryStorage::new(1024);
//! s.write_block(0, b"hello").unwrap();
//! let mut buf = [0_u8; 5];
//! s.read_block(0, &mut buf).unwrap();
//! assert_eq!(&buf, b"hello");
//! ```

mod error;
#[cfg(unix)]
mod file;
mod memory;
mod traits;

pub use error::{StorageError, StorageErrorKind};
#[cfg(unix)]
pub use file::FileStorage;
pub use memory::MemoryStorage;
pub use traits::{IoVec, IoVecMut, Storage};
