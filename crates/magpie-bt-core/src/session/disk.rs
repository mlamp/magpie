//! Bounded disk-write task.
//!
//! Decouples slow storage and SHA-1 verification from the per-torrent actor
//! event loop. The actor `send().await`s [`DiskOp`]s onto a bounded
//! [`mpsc`] channel; when the queue saturates, the actor naturally
//! backpressures, which in turn backpressures peer tasks (S1 channel cap),
//! which in turn backpressures TCP. End result: a torrent on a slow disk
//! does not OOM, regardless of how fast peers send blocks.
//!
//! See ADR-0007 for the full rationale.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use bytes::Bytes;
use magpie_bt_metainfo::sha1;
use tokio::sync::{mpsc, oneshot};

use crate::storage::Storage;

/// Default capacity of the [`DiskOp`] queue. At a typical 256 KiB piece, this
/// caps in-flight unverified buffers at ~16 MiB before the actor blocks.
///
/// **Backpressure model**: this is the *only* bounded link in the disk path
/// (D1 hardening). The return-leg [`DiskCompletion`] channel is intentionally
/// **unbounded** because the number of outstanding completions can never
/// exceed the number of in-flight ops, which is already capped by this
/// constant. Bounding the return leg as well would deadlock: actor blocked
/// on `disk_tx.send` cannot drain `completion_rx`, writer blocked on
/// `completion_tx.send` cannot drain `disk_rx`.
pub const DEFAULT_DISK_QUEUE_CAPACITY: usize = 64;

/// Result of a verify-and-write operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum DiskError {
    /// SHA-1 of the buffer did not match the expected piece hash.
    HashMismatch,
    /// Underlying storage write failed.
    Io,
}

/// Operation submitted to the [`DiskWriter`] task.
#[derive(Debug)]
#[non_exhaustive]
pub enum DiskOp {
    /// Verify SHA-1 and, on match, commit to storage.
    VerifyAndWrite {
        /// Zero-based piece index.
        piece: u32,
        /// Piece-aligned byte offset within the storage object.
        offset: u64,
        /// Piece payload to verify and write. [`Bytes`] (ADR-0007 amended
        /// via ADR-0018) so the store-buffer short-circuit — reads for blocks
        /// still in the pending-write queue — shares the same allocation
        /// with the cache via refcount, zero memcpy on fan-out.
        buffer: Bytes,
        /// Expected SHA-1 from the metainfo.
        expected_hash: [u8; 20],
        /// Channel to send the [`DiskCompletion`] back on. Must be unbounded
        /// — see [`DEFAULT_DISK_QUEUE_CAPACITY`] for why.
        completion_tx: mpsc::UnboundedSender<DiskCompletion>,
    },
    /// Read a block from storage. Added in M2 for the upload path
    /// (ADR-0017): seed-side peers request block data via this op.
    ///
    /// Fulfilled via a `oneshot` so the per-peer upload task can `await` the
    /// read without blocking the pull-model watermark loop.
    Read {
        /// Zero-based piece index.
        piece: u32,
        /// Piece-aligned byte offset (block start) within storage.
        offset: u64,
        /// Length of the read in bytes (typically `BLOCK_SIZE`, smaller for
        /// the final block of a short piece).
        length: u32,
        /// Reply channel. Sender side fires with `Ok(Bytes)` on success or
        /// `Err(DiskError::Io)` on failure. Dropped receiver means the peer
        /// disconnected before the read resolved — writer best-effort
        /// discards the result.
        reply_tx: oneshot::Sender<Result<Bytes, DiskError>>,
    },
    /// Drain pending ops and exit. Intended for graceful shutdown.
    Shutdown,
}

/// Completion notification published by the writer once a verify-and-write
/// resolves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DiskCompletion {
    /// Piece this completion refers to.
    pub piece: u32,
    /// `Ok` on successful verify + commit; `Err` on hash mismatch or I/O.
    pub result: Result<(), DiskError>,
}

/// Live counters published by the writer. Cheap to read from any thread.
#[derive(Debug, Default)]
pub struct DiskMetrics {
    /// Pieces verified and committed successfully (cumulative).
    pub pieces_written: AtomicU64,
    /// Bytes written to storage (cumulative).
    pub bytes_written: AtomicU64,
    /// Pieces that failed SHA-1 verification (cumulative).
    pub piece_verify_fail: AtomicU64,
    /// I/O failures observed when writing to storage (cumulative).
    pub io_failures: AtomicU64,
}

/// The bounded disk-write task.
pub struct DiskWriter {
    rx: mpsc::Receiver<DiskOp>,
    storage: Arc<dyn Storage>,
    metrics: Arc<DiskMetrics>,
}

impl DiskWriter {
    /// Construct a new writer plus its op-submission [`mpsc::Sender`] and
    /// shared [`DiskMetrics`] handle.
    ///
    /// The caller is responsible for `tokio::spawn`-ing [`Self::run`].
    #[must_use]
    pub fn new(storage: Arc<dyn Storage>, queue_capacity: usize) -> (Self, mpsc::Sender<DiskOp>, Arc<DiskMetrics>) {
        let (tx, rx) = mpsc::channel(queue_capacity);
        let metrics = Arc::new(DiskMetrics::default());
        let writer = Self {
            rx,
            storage,
            metrics: Arc::clone(&metrics),
        };
        (writer, tx, metrics)
    }

    /// Drain ops until [`DiskOp::Shutdown`] arrives or every sender is dropped.
    pub async fn run(mut self) {
        let span = tracing::info_span!("disk_writer");
        let _enter = span.enter();
        tracing::debug!("disk writer started");
        while let Some(op) = self.rx.recv().await {
            match op {
                DiskOp::Shutdown => {
                    tracing::debug!("disk writer received shutdown");
                    break;
                }
                DiskOp::VerifyAndWrite {
                    piece,
                    offset,
                    buffer,
                    expected_hash,
                    completion_tx,
                } => {
                    let storage = Arc::clone(&self.storage);
                    let metrics = Arc::clone(&self.metrics);
                    let buffer_len = buffer.len() as u64;
                    let result =
                        tokio::task::spawn_blocking(move || verify_and_commit(&buffer, expected_hash, &*storage, offset))
                            .await
                            .unwrap_or(Err(DiskError::Io));
                    match result {
                        Ok(()) => {
                            metrics.pieces_written.fetch_add(1, Ordering::Relaxed);
                            metrics.bytes_written.fetch_add(buffer_len, Ordering::Relaxed);
                        }
                        Err(DiskError::HashMismatch) => {
                            metrics.piece_verify_fail.fetch_add(1, Ordering::Relaxed);
                        }
                        Err(DiskError::Io) => {
                            metrics.io_failures.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                    // Best-effort: if the session is gone, just drop the completion.
                    let _ = completion_tx.send(DiskCompletion { piece, result });
                }
                DiskOp::Read { piece: _, offset, length, reply_tx } => {
                    let storage = Arc::clone(&self.storage);
                    let result: Result<Bytes, DiskError> = tokio::task::spawn_blocking(move || {
                        let mut buf = vec![0u8; length as usize];
                        storage
                            .read_block(offset, &mut buf)
                            .map(|()| Bytes::from(buf))
                            .map_err(|_| DiskError::Io)
                    })
                    .await
                    .unwrap_or(Err(DiskError::Io));
                    if result.is_err() {
                        self.metrics.io_failures.fetch_add(1, Ordering::Relaxed);
                    }
                    // Best-effort: dropped receiver means the peer
                    // disconnected before the read resolved.
                    let _ = reply_tx.send(result);
                }
            }
        }
    }
}

fn verify_and_commit(
    buffer: &[u8],
    expected: [u8; 20],
    storage: &dyn Storage,
    offset: u64,
) -> Result<(), DiskError> {
    let actual = sha1(buffer);
    if actual != expected {
        return Err(DiskError::HashMismatch);
    }
    storage.write_block(offset, buffer).map_err(|_| DiskError::Io)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::MemoryStorage;

    #[tokio::test]
    async fn verifies_and_writes() {
        let storage: Arc<dyn Storage> = Arc::new(MemoryStorage::new(1024));
        let (writer, op_tx, metrics) = DiskWriter::new(Arc::clone(&storage), 4);
        let writer_task = tokio::spawn(writer.run());

        let buffer_vec = vec![7u8; 64];
        let expected = sha1(&buffer_vec);
        let buffer = Bytes::from(buffer_vec);
        let (completion_tx, mut completion_rx) = mpsc::unbounded_channel();
        op_tx
            .send(DiskOp::VerifyAndWrite {
                piece: 0,
                offset: 0,
                buffer,
                expected_hash: expected,
                completion_tx,
            })
            .await
            .unwrap();
        let completion = completion_rx.recv().await.unwrap();
        assert_eq!(completion.piece, 0);
        assert_eq!(completion.result, Ok(()));
        assert_eq!(metrics.pieces_written.load(Ordering::Relaxed), 1);
        assert_eq!(metrics.bytes_written.load(Ordering::Relaxed), 64);
        assert_eq!(metrics.piece_verify_fail.load(Ordering::Relaxed), 0);

        // Confirm storage actually got the bytes.
        let mut got = vec![0u8; 64];
        storage.read_block(0, &mut got).unwrap();
        assert_eq!(got, vec![7u8; 64]);

        op_tx.send(DiskOp::Shutdown).await.unwrap();
        writer_task.await.unwrap();
    }

    #[tokio::test]
    async fn surfaces_hash_mismatch() {
        let storage: Arc<dyn Storage> = Arc::new(MemoryStorage::new(1024));
        let (writer, op_tx, metrics) = DiskWriter::new(Arc::clone(&storage), 4);
        let writer_task = tokio::spawn(writer.run());

        let (completion_tx, mut completion_rx) = mpsc::unbounded_channel();
        op_tx
            .send(DiskOp::VerifyAndWrite {
                piece: 1,
                offset: 0,
                buffer: Bytes::from(vec![1u8; 32]),
                expected_hash: [0xFFu8; 20], // wrong hash
                completion_tx,
            })
            .await
            .unwrap();
        let completion = completion_rx.recv().await.unwrap();
        assert_eq!(completion.result, Err(DiskError::HashMismatch));
        assert_eq!(metrics.piece_verify_fail.load(Ordering::Relaxed), 1);
        assert_eq!(metrics.pieces_written.load(Ordering::Relaxed), 0);

        op_tx.send(DiskOp::Shutdown).await.unwrap();
        writer_task.await.unwrap();
    }
}
