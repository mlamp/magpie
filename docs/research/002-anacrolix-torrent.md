# 002 — anacrolix/torrent

- **Repo**: https://github.com/anacrolix/torrent
- **Commit**: `a59d7c92209d7306d4f41af1ce3dc0b2f5fd88c2`
- **Date**: 2026-04-13

## Project layout

Monolithic Go library, clean package hierarchy: `bencode/`, `storage/`, `metainfo/`, `merkle/`, `peer_protocol/`, `types/` (with `infohash-v2/`), `internal/request-strategy/`, `tracker/`, `webseed/`, `mse/`, `dialer/`, `smartban/`, `cmd/`. Core types in `torrent.go` (`Torrent`, `Peer`, `PeerConn`) and `client.go` (`Client`, session orchestration). Packages are well-separated but the central `Client` and `Torrent` types thread through most of them.

## Storage trait — the headline pattern

Interface at `storage/interface.go:12-89`. Translated to Rust idiom:

```go
type ClientImpl interface {
    OpenTorrent(ctx, info *metainfo.Info, infoHash metainfo.Hash) (TorrentImpl, error)
}

type TorrentImpl struct {
    Piece          func(p metainfo.Piece) PieceImpl
    PieceWithHash  func(p metainfo.Piece, pieceHash g.Option[[]byte]) PieceImpl
    Close          func() error
    Capacity       TorrentCapacity                // storage usage cap; shared-pointer detection
    NewReader      func() TorrentReader           // io.ReaderAt + Close
    NewPieceReader func(p Piece) PieceReader      // stateful per-piece reader
}

type PieceImpl interface {
    io.ReaderAt
    io.WriterAt
    MarkComplete() error
    MarkNotComplete() error
    Completion() Completion
}
```

**Per-file streaming**: `TorrentImpl.NewReader()` and `NewPieceReader()` return `io.Reader`-like abstractions. The file backend (`storage/file-torrent.go:102-112`) wraps pieces with `io.SectionReader` for reads, `missinggo.SectionWriter` for writes. Clients consume torrents via `ReadAt` semantics without knowing file boundaries.

**Backends supported**: file (default), mmap (`storage/mmap.go`), sqlite (`storage/sqlite/`), possum (blob), bolt. `Capacity` function identity is used to detect shared storage across torrents.

This is the pattern magpie's `Storage` trait should mirror almost verbatim, translated to `AsyncRead + AsyncSeek` / `AsyncWriteAt` in async Rust.

## Piece picker

Location: `internal/request-strategy/piece-request-order.go`. B-tree abstraction:

```go
type PieceRequestOrder struct {
    tree Btree
    keys map[PieceRequestOrderKey]PieceRequestOrderState
}

type PieceRequestOrderState struct {
    Availability int          // rarest-first: peer count with this piece
    Priority     piecePriority
    Partial      bool
}
```

Sorted via `pieceOrderLess()` (line 50) comparing state tuples. Rarest-first is core; priority stacks on top. B-tree gives O(log n) updates as bitfields change. More machinery than cratetorrent's Vec but needed once rarity updates are frequent.

## Peer connection task

`peerconn.go`. `PeerConn` struct lines 47-126:

- `validReceiveChunks map[RequestIndex]int` — expected-chunk bookkeeping.
- `unreadPeerRequests`, `readyPeerRequests map[Request][]byte` — upload request buffers, split by I/O phase.
- `peerRequestDataAllocDecreased chansync.BroadcastCond` — signal when upload buffer drains (line 119).
- `requestState requestStrategy.PeerRequestState` — current request/cancel state (line 102).
- `sentHashRequests map[hashRequest]struct{}` — v2 hash fetches (line 108).
- `v2 bool` (line 63) — peer supports BEP 52.
- `messageWriter peerConnMsgWriter` (line 88) — rate-limited, buffered outgoing write path.

**Backpressure**: `MaxAllocPeerRequestDataPerConn` (`config.go:169`) caps buffered upload bytes per peer. Exceeded → peer choked until buffer drains. Explicit, not automatic.

## Event/progress model

**Callbacks, not channels.** `callbacks.go`:

```go
type Callbacks struct {
    CompletedHandshake            func(*PeerConn, InfoHash)
    ReadMessage                   func(*PeerConn, *pp.Message)
    PeerConnClosed                func(*PeerConn)
    PeerConnReadExtensionMessage  []func(PeerConnReadExtensionMessageEvent)
    ReceivedUsefulData            []func(ReceivedUsefulDataEvent)
    ReceivedRequested             []func(PeerMessageEvent)
    DeletedRequest                []func(PeerRequestEvent)
    SentRequest                   []func(PeerRequestEvent)
    PeerClosed                    []func(*Peer)
    NewPeer                       []func(*Peer)
    PeerConnAdded                 []func(*PeerConn)
    StatusUpdated                 []func(StatusUpdatedEvent)
}
```

Synchronous, invoked inline. Simpler than a ring buffer but **holds `Client`/`Torrent` locks** during invocation → slow callbacks stall the picker. No persistent event log, no late-subscriber replay.

## v1 vs v2 hashing

Full BEP 52 support:

- `merkle/hash.go` — accumulates 32-byte blocks, implements `hash.Hash` interface, builds tree on the fly.
- `merkle/merkle.go` — verification with padding for partial blocks.
- `types/infohash-v2/infohash-v2.go` — 32-byte hash type for v2 torrents.
- `peerconn.go:63` `v2 bool` — capability negotiated in extended handshake.
- `TorrentImpl.PieceWithHash(p, Option<[]byte>)` (`storage/interface.go:33`) lets the storage layer consume the hash when the metainfo lacks piece layers.

No hash enum in the storage layer — hash verification lives one level up.

## Hot-path tricks

- `sync.Pool`: `torrent.go:108 chunkPool`, `peer_protocol/decoder.go:19` message-struct pool, `webseed/client.go:202 bufPool`.
- Roaring bitmaps for bitfields: `peerconn.go:52 sentHaves`, `peerconn.go:97 _peerPieces`, `torrent.go:179-183` pending/completed. Compressed, fast bitwise ops.
- Pre-allocated state maps: `peerconn.go:56 validReceiveChunks`, `torrent.go:207-208` piece-request state.
- `messageWriter` pairs net.Conn with rate limiting + stats; no per-message allocation.

## Pain points

- **Lock contention**: `Torrent` holds a write lock during piece-completion checks and priority updates; long storage ops block the picker.
- **Callbacks under locks**: sync callbacks that take time stall the core loop.
- **Storage sync semantics**: `MarkComplete()` is sync but durability not guaranteed — `pieceCompletionIsPersistent()` (`storage/piece-completion.go:50`) is a workaround.
- **Per-piece allocation**: `PieceImpl` built on each access; no pooling at that layer.
- **v2 metadata without piece layers**: defers to peer-fetch, adds request-manager complexity.

## What magpie should borrow

1. **Storage trait shape**: `OpenTorrent → TorrentImpl → PieceImpl` with `ReadAt`/`WriteAt` semantics. Translated: `Storage` trait returning `TorrentStorage` handle, each `PieceHandle` implements `AsyncRead`/`AsyncWrite` at piece offsets.
2. **Per-file streaming** via `io.SectionReader` analogue: our streaming support (M6) falls out naturally if the storage trait has this shape from day one.
3. **B-tree piece-request order** with `(availability, priority, partial)` sort key. Better than cratetorrent's linear scan as the swarm grows.
4. **Split unread/ready peer-request queues**. Separates network phase from disk phase cleanly.
5. **Merkle package** as a standalone helper. Usable for both v1 piece verification (trivially) and v2 tree verification.

## What magpie should avoid

1. **Synchronous callbacks under locks.** Use `broadcast::<TorrentEvent>` with bounded buffer; slow consumers get `Lagged`, never stall the engine.
2. **No persistent event log.** Magpie should at minimum snapshot current state so late subscribers can resync without losing history.
3. **Mixed sync/async storage semantics.** Pick one: either `async fn mark_complete` with an explicit fsync contract, or sync with documented "no durability" behaviour. Don't leave it ambiguous.
4. **Per-piece allocation on every access.** Pool `PieceHandle`s or make them zero-cost views.

## ADR seeds

- **Storage trait shape** (new ADR candidate): borrow anacrolix verbatim, translated to async Rust.
- **ADR 0002 (event bus)**: anacrolix's callback pain explicitly validates our broadcast-channel choice. Cite this.
- **Picker architecture** (new ADR candidate): B-tree availability/priority sort is a proven middle ground between cratetorrent (linear) and rasterbar (bucketed). Pick deliberately.
