//! Alert ring — the primary magpie event bus (ADR-0002).
//!
//! Producer (the engine) pushes [`Alert`]s into a bounded, category-filtered
//! ring. A single primary reader drains them in batches. The producer never
//! blocks; if the ring is full, the oldest alerts are evicted and a
//! [`Alert::Dropped`] sentinel is enqueued so the consumer learns the exact
//! count of lost events.
//!
//! Multi-subscriber fan-out is a consumer-side concern; the hot path through
//! magpie itself is single-reader, zero-clone.
//!
//! # Example
//! ```
//! use magpie_bt_core::alerts::{Alert, AlertCategory, AlertQueue};
//!
//! let q = AlertQueue::new(16);
//! q.push(Alert::PieceCompleted { piece: 3 });
//! q.push(Alert::PeerConnected { peer: 42 });
//! let batch = q.drain();
//! assert_eq!(batch.len(), 2);
//! assert!(matches!(batch[0], Alert::PieceCompleted { piece: 3 }));
//! // Category filtering:
//! q.set_mask(AlertCategory::PIECE);
//! q.push(Alert::PeerConnected { peer: 7 }); // filtered out
//! q.push(Alert::PieceCompleted { piece: 4 });
//! assert_eq!(q.drain().len(), 1);
//! ```

mod category;
mod queue;

pub use category::{Alert, AlertCategory, AlertErrorCode};
pub use queue::AlertQueue;
