# 0022 — Resume-state persistence

- **Status**: proposed
- **Date**: 2026-04-19
- **Deciders**: magpie maintainers
- **Consulted**: `_tmp/rakshasa-libtorrent/src/torrent/utils/resume.h`, ADR-0014 (stats sink — same shape), `session::torrent::apply_initial_have`

## Context

Magpie currently persists cumulative upload/download counters via
[`FileStatsSink`](../../crates/magpie-bt-core/src/session/stats/sink.rs)
(ADR-0014) but **does not persist which pieces have been verified**. A
process restart, crash, or SIGKILL mid-leech forces the consumer to
re-download from scratch — the on-disk blocks are intact, but the
engine has no memory of which pieces are already good.

rtorrent solves this with `Resume` (`src/torrent/utils/resume.h`): a
per-torrent bencoded sidecar holding the bitfield + priority map +
tracker cache. On startup, the client loads the sidecar, hands the
bitfield to the engine, and the download resumes without re-verifying.

The M2 workstream J gate (multi-file) shipped without resume; today
even a 10-file torrent that's 99% done has to start over after a
restart. This ADR closes that gap with a minimal, consumer-driven
sink that mirrors the stats-sink pattern.

## Decision

Add a new object-safe `trait ResumeSink` mirroring `StatsSink`, with a
default [`FileResumeSink`] that writes one bencoded sidecar per torrent.
Scope for v1 is **only the verified bitfield** — enough for the headline
"restart without re-downloading completed pieces" use case. Piece
priority and tracker-cache resumption are explicit non-goals for this
iteration; they fold in once magpie has public surface for those
concepts.

### Shape

```rust
pub struct ResumeSnapshot {
    pub info_hash: [u8; 20],
    pub piece_count: u32,
    pub piece_length: u64,
    pub total_length: u64,
    /// Per-piece verified flag. Length equals `piece_count`.
    pub have: Vec<bool>,
}

pub trait ResumeSink: Send + Sync {
    fn enqueue(&self, snap: ResumeSnapshot) -> Result<(), ResumeSinkError>;
    fn flush_graceful(&self, timeout: Duration) -> Result<(), ResumeSinkError>;
}

pub struct FileResumeSink { /* dir + pending + bitfield packer */ }

impl FileResumeSink {
    pub fn new(dir: impl Into<PathBuf>) -> Result<Self, ResumeSinkError>;
    pub fn flush_now(&self) -> Result<(), ResumeSinkError>;
    pub fn sidecar_path(&self, info_hash: &[u8; 20]) -> PathBuf;
    pub fn load_sidecar(&self, info_hash: &[u8; 20])
        -> Result<Option<ResumeSnapshot>, ResumeSinkError>;
}
```

### Sidecar schema

File path: `<dir>/<hex_info_hash>.resume`.

Bencode dict, keys alphabetically ordered:

```text
d
  8:bitfield<NN>:<packed bitfield — big-endian, MSB=piece 0, trailing bits zero>
  9:info_hash20:<20 bytes>
  12:piece_counti<N>e
  12:piece_lengthi<N>e
  12:total_lengthi<N>e
  7:versioni1e
e
```

- **`bitfield`**: packed MSB-first (matches the BEP 3 wire `Bitfield`
  encoding, so we can reuse the same packing helper and make the format
  trivially interoperable with any future v2 of this ADR).
  Length = `ceil(piece_count / 8)`. The trailing bits of the last byte
  beyond `piece_count` MUST be zero and MUST be ignored on load.
- **`info_hash`**: the v1 info-hash the sidecar belongs to. On load the
  caller compares against their expected hash — mismatch is a hard error
  (wrong sidecar, or renamed torrent).
- **`piece_count`**, **`piece_length`**, **`total_length`**: geometry
  guardrails. Mismatch against the caller's [`TorrentParams`] is a hard
  error (same torrent, different slicing — possible in a BEP 9 magnet
  restart scenario).
- **`version`**: schema version, currently `1`. Forward-compat escape
  hatch. A loader seeing `> 1` returns `UnsupportedVersion`.

Writes are atomic (write-to-tmp + `rename`) so a crash mid-flush leaves
the prior snapshot intact. Sync via `File::sync_all` before rename.

### Write cadence

Consumer-driven, same pattern as stats:

- Consumer subscribes to `Alert::PieceCompleted` (already fires on every
  verified piece) and periodically calls
  `Engine::torrent_bitfield_snapshot(id) -> Option<Vec<bool>>` (new
  accessor — see below) to build a `ResumeSnapshot`.
- Enqueue into the sink; the sink batches (30 s default) and flushes.
- On graceful shutdown the consumer calls `sink.flush_graceful(5s)` to
  force a final write.

The engine does not pull on the sink directly — consistent with the
stats model, where the consumer owns cadence and override policy. This
keeps the library unopinionated about disk cadence.

### Restore flow

```rust
let sink = FileResumeSink::new(dir)?;
let have = match sink.load_sidecar(&info_hash)? {
    Some(snap) if snap.piece_count == params.piece_count
        && snap.info_hash == info_hash => snap.have,
    Some(mismatched) => {
        // Log and ignore — start fresh rather than corrupt.
        Vec::new()
    }
    None => Vec::new(), // cold start
};
let req = AddTorrentRequest::new(info_hash, params, storage, peer_id)
    .with_initial_have(have);
engine.add_torrent(req).await?;
```

## Consequences

Positive:

- A consumer can restart magpie and the engine immediately resumes
  verified pieces — the headline missing behaviour.
- Schema is trivial (6 scalar keys + a bitfield) and uses the same
  bencode primitives + atomic-rename pattern as `FileStatsSink`, so the
  surface area of new code is small.
- The bitfield packing matches BEP 3's wire format, so an eventual
  "import/export torrent session" feature across implementations can
  reuse the same helper.

Negative:

- **Blind trust**: loading a sidecar assumes the on-disk pieces still
  match their hashes. A user who manually edited a data file between
  runs can silently corrupt the download until endgame verification
  catches it. Mitigation: consumers that need a safety net can call
  an out-of-band re-verify pass (not in this ADR's scope); the default
  behaviour matches rtorrent's "fast resume" mode.
- **Priority + tracker state deferred**: v1 only persists the bitfield.
  A user that reconfigures piece priorities post-resume loses that
  state. Acceptable for M2 — those features aren't exposed publicly yet.

Neutral:

- Requires a new `Engine::torrent_bitfield_snapshot(id)` accessor,
  shaped to match `torrent_stats_snapshot`: async, returns `None` if
  the torrent isn't registered. A small, read-only addition.

## Alternatives considered

- **Library-driven auto-persistence**: engine owns the sink, pulls on
  every `PieceCompleted`. Rejected for symmetry with stats — consumers
  want a single injection point for "disk-bound I/O", and `StatsSink`
  set the precedent that the consumer decides cadence.
- **Single combined "session" sidecar** (stats + resume in one file).
  Rejected: the two have different flush cadences (stats: every 30 s;
  resume: every piece-complete during download, quiescent while
  seeding) and different consumer override points (lightorrent has a
  redb-backed stats sink that would not naturally merge with a filesystem-
  bound resume sink).
- **Store the bitfield as raw bytes vs a list of booleans**. Rejected:
  bencode lacks a dedicated bit-array type; a `list[int]` bitfield
  would 40× the file size. Packed bytes match BEP 3 wire format and
  are the only sensible choice.
