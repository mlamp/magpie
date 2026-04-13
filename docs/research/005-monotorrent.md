# MonoTorrent (C#) — Architecture Review

**Repo**: [alanmcgovern/monotorrent](https://github.com/alanmcgovern/monotorrent)  
**Commit**: `9e98a44c3af93ace7fe11da363fe345a60c0c93f`  
**Date**: 2026-04-13

## Executive Summary

MonoTorrent is a mature, well-factored C# BitTorrent library (v1 and partial v2 support). We reviewed it against three magpie seeding questions: Azureus-style peer-ID builder (configurable client identity), BEP 52 v2 hash data model (merkle tree abstraction), and TorrentManager public API shape. Key findings: peer-ID is *hardcoded* (not configurable), v2 hashing is cleanly abstracted via `IPieceHashes` interface, and the public API is disciplined but callback-heavy (events instead of channels).

---

## 1. Peer-ID Construction — Hardcoded, Not Configurable

### Current API Shape

**File**: `/src/GitInfoHelper.cs:37-88`

```csharp
internal static class GitInfoHelper {
    internal static string ClientIdentifier { get; } = "MO";
    internal static string ClientVersion { get; private set; }
    
    internal static void Initialize(Version version) {
        // Enforces: 0 ≤ major, minor < 10; build < 100
        var versionString = $"{version.Major}{version.Minor}{version.Build:00}";
        ClientVersion = $"{ClientIdentifier}{versionString}";
        // Result: "MO1234" for version 1.2.34
    }
}
```

**File**: `/src/MonoTorrent.Client/ClientEngine.cs:1007-1023`

```csharp
static BEncodedString GeneratePeerId () {
    var sb = new StringBuilder(20);
    sb.Append("-");
    sb.Append(GitInfoHelper.ClientVersion);  // "-MO1234"
    sb.Append("-");
    lock (PeerIdRandomGenerator) {
        while (sb.Length < 20)
            sb.Append(PeerIdRandomGenerator.Next(0, 9));  // Random digits
    }
    return new BEncodedString(sb.ToString());
}
```

### Format
- **Structure**: `-MO####-XXXXXXXXXX` (20 bytes total)
  - `-`: Azureus marker
  - `MO`: Client identifier (hardcoded to "MonoTorrent")
  - `####`: 4-digit version (major=1 digit, minor=1 digit, build=2 digits)
  - `-`: Separator
  - `XXXXXXXXXX`: Random decimal digits (not base62, just 0-9)

### Consumer Flexibility — **Very Limited**
- **Client code**: Hardcoded to `"MO"` (no configuration in `EngineSettings` or constructor).
- **Version**: Derived from assembly version via `GitInfoHelper.Initialize(version)` at static init time.
- **Randomness**: Only the trailing 10 bytes are randomized; non-cryptographic RNG, thread-safe via lock.

### Magpie Implication
MonoTorrent does *not* expose configurability here. If magpie needs `client_code: [u8; 2]` + `version: [u8; 4]`, it must design its own peer-ID builder (or fork this pattern). **MonoTorrent is not a good source for the peer-ID configurability requirement.**

---

## 2. BEP 52 v2 Hash Data Model — Clean Interface Abstraction

### Hash Storage Architecture

**File**: `/src/MonoTorrent/IPieceHashes.cs:35-45` (Public interface)

```csharp
public interface IPieceHashes {
    int Count { get; }
    bool HasV1Hashes { get; }
    bool HasV2Hashes { get; }
    
    ReadOnlyPieceHash GetHash(int hashIndex);
    bool IsValid(ReadOnlyPieceHash hashes, int hashIndex);
    bool TryGetV2Hashes(MerkleRoot piecesRoot, [NotNullWhen(true)] out ReadOnlyMerkleTree? merkleTree);
    bool TryGetV2Hashes(MerkleRoot piecesRoot, int layer, int index, int count, int proofCount, 
                        Span<byte> hashesAndProofsBuffer, out int bytesWritten);
}
```

**Concrete Implementation v1**: `/src/MonoTorrent/PieceHashesV1.cs:35-73`

```csharp
class PieceHashesV1 : IPieceHashes {
    readonly ReadOnlyMemory<byte> HashData;  // Flat 20-byte-per-piece array
    readonly int HashCodeLength;            // = 20 for SHA-1
    
    public int Count => HashData.Length / HashCodeLength;
    public bool HasV1Hashes => true;
    public bool HasV2Hashes => false;
    
    public ReadOnlyPieceHash GetHash(int hashIndex) 
        => new ReadOnlyPieceHash(
            HashData.Slice(hashIndex * HashCodeLength, HashCodeLength), 
            default);
    
    // v2 methods return false/null (no-op)
}
```

**Concrete Implementation v2**: `/src/MonoTorrent/PieceHashesV2.cs:42-147`

```csharp
class PieceHashesV2 : IPieceHashes {
    readonly Dictionary<MerkleRoot, ReadOnlyMerkleTree> Layers;  // Per-file merkle trees
    readonly IList<ITorrentFile> Files;
    readonly int HashCodeLength = 32;  // SHA-256
    
    public bool HasV1Hashes => false;
    public bool HasV2Hashes => true;
    
    public ReadOnlyPieceHash GetHash(int hashIndex) {
        // Locates the file owning this piece, then queries the merkle layer
        for (int i = 0; i < Files.Count; i++) {
            if (hashIndex within Files[i].piece range && Files[i].Length > 0) {
                if (Layers.TryGetValue(Files[i].PiecesRoot, out var layer))
                    return new ReadOnlyPieceHash(
                        ReadOnlyMemory<byte>.Empty,
                        layer.GetHash(layer.PieceLayerIndex, hashIndex - Files[i].StartPieceIndex));
                if (file has exactly 1 piece)
                    return PiecesRoot hash directly;
            }
        }
    }
}
```

### Dual-Hash Container

**File**: `/src/MonoTorrent/ReadOnlyPieceHash.cs:34-47`

```csharp
public readonly struct ReadOnlyPieceHash {
    public ReadOnlyMemory<byte> V1Hash { get; }    // 20 bytes or empty
    public ReadOnlyMemory<byte> V2Hash { get; }    // 32 bytes or empty
    
    internal ReadOnlyPieceHash(ReadOnlyMemory<byte> v1Hash, ReadOnlyMemory<byte> v2Hash) {
        // Validation: exactly one must be non-empty per version, or both empty
        if (!v1Hash.IsEmpty && v1Hash.Length != 20)
            throw new ArgumentException("V1 must be 20 bytes");
        if (!v2Hash.IsEmpty && v2Hash.Length != 32)
            throw new ArgumentNullException("V2 must be 32 bytes");
        (V1Hash, V2Hash) = (v1Hash, v2Hash);
    }
}
```

### Merkle Tree Access Pattern

For BEP 9/10 metadata and BEP 52 proof generation:

```csharp
// Layer-based access (used for ut_metadata and BEP 52 merkle proofs)
bool TryGetV2Hashes(MerkleRoot piecesRoot, int baseLayer, int index, int length, 
                    int proofCount, Span<byte> hashesAndProofsBuffer, out int bytesWritten)
```

- Extracts hashes from a given merkle layer.
- Calculates and appends sibling proofs for the requested range.
- Used by ut_metadata extension to serve incomplete metadata.

### Magpie Implication — **Excellent Model**
✓ Clean `enum`-like interface distinction (never both, always one per piece).  
✓ Per-file merkle root with lazy-loaded layer dictionary.  
✓ `ReadOnlyMemory<T>` slicing (zero-copy).  
✓ Merkle proof generation built into the v2 impl.

**Borrow**: Use a similar `PieceHash::{V1(Sha1), V2(Sha256)}` enum or struct with optional merkle layer access trait.

---

## 3. TorrentManager — Public API Shape

### Constructor & Metadata Ingestion

**File**: `/src/MonoTorrent.Client/ClientEngine.cs:355-407`

```csharp
// Add from .torrent file
public Task<TorrentManager> AddAsync(string metadataPath, string saveDirectory, TorrentSettings settings)

// Add from magnet link
public Task<TorrentManager> AddAsync(MagnetLink magnetLink, string saveDirectory, TorrentSettings settings)

// Add from Torrent object
public Task<TorrentManager> AddAsync(Torrent torrent, string saveDirectory, TorrentSettings settings)
```

All return `Task<TorrentManager>`, which integrates with auto-save/restore and DHT bootstrap.

### State Machine — Enum with Mode Subclasses

**File**: `/src/MonoTorrent/Enums.cs:32-45`

```csharp
public enum TorrentState {
    Stopped,          // Initial
    Paused,           // Paused by user
    Starting,         // Transitional
    Downloading,      // Active
    Seeding,          // All pieces; uploading
    Hashing,          // Hash-checking at startup
    HashingPaused,    // Paused during hash check
    Stopping,         // Transitional
    Error,            // Unrecoverable state
    Metadata,         // Fetching .torrent (magnet)
    FetchingHashes    // Fetching v2 layer hashes
}
```

Each state is a separate `IMode` subclass; transitions happen via `mode = new NextMode()`. No explicit state machine; mode changes are side-effectful.

### Public Properties & Stats

**File**: `/src/MonoTorrent.Client/Managers/TorrentManager.cs:150-432`

```csharp
public class TorrentManager : IEquatable<TorrentManager>, ITorrentManagerInfo, ... {
    // Core state
    public TorrentState State => mode.State;
    public ReadOnlyBitField Bitfield { get; }          // Progress
    public double Progress { get; }                     // 0-100%
    public double PartialProgress { get; }             // For "DoNotDownload" files
    public bool Complete { get; }                      // All pieces + correct sizes
    
    // File listing
    public IList<ITorrentManagerFile> Files { get; }
    public string ContainingDirectory { get; }
    public string SavePath { get; }
    
    // Connectivity & speed
    public int OpenConnections { get; }                // Connected peer count
    public int UploadingTo { get; }
    public ConnectionMonitor Monitor { get; }          // Rate + total bytes
    
    // Piece management
    public PieceManager PieceManager { get; }          // Picker
    public int HashFails { get; }                      // Corruption count
    public bool IsInEndGame { get; }                   // Few pieces left
    
    // Metadata
    public bool HasMetadata { get; }
    public InfoHashes InfoHashes { get; }
    public Torrent? Torrent { get; }
    public MagnetLink MagnetLink { get; }
    
    // Extension support
    public bool CanUseDht { get; }
    public bool CanUseLocalPeerDiscovery { get; }
    
    // Settings & configuration
    public TorrentSettings Settings { get; }
    public ITrackerManager TrackerManager { get; }
}
```

### Events — Asynchronous Callbacks

```csharp
public event EventHandler<PeerConnectedEventArgs>? PeerConnected;
public event EventHandler<PeerDisconnectedEventArgs>? PeerDisconnected;
public event EventHandler<ConnectionAttemptFailedEventArgs>? ConnectionAttemptFailed;
public event EventHandler<PeersAddedEventArgs>? PeersFound;           // From trackers, DHT, LSD
public event EventHandler<PieceHashedEventArgs>? PieceHashed;         // Sync verification result
public event EventHandler<TorrentStateChangedEventArgs>? TorrentStateChanged;
```

Events are raised on the main `ClientEngine.MainLoop` (single-threaded scheduler).

### Control Methods

```csharp
public async Task StartAsync() { }
public async Task StopAsync() { }
public async Task PauseAsync() { }
public async Task ResumeAsync() { }
public async Task SetFilePriorityAsync(ITorrentManagerFile file, Priority priority) { }
public async Task HashCheckAsync(bool skipIfComplete = false) { }
public async Task SetNeedsHashCheckAsync() { }
```

All are async; called from consumer code, enqueued to `MainLoop`.

### Magpie Implication — **Pattern Worth Borrowing**

✓ Clean public API surface: no `pub(crate)` types.  
✓ Factory methods (Add* overloads) consolidate setup.  
✓ Immutable bitfield + progress exposed; settable via explicit methods.  
✓ Events for async notification (no polling).  
✗ Event-based rather than broadcast channel (harder to backpressure, no filtering at source).  
✗ State enum forces match statements; no trait-based polymorphism in public API.

**Borrow**: Copy the "factory methods for add, async control methods, public properties for stats" structure. Replace events with `broadcast::Receiver<TorrentEvent>`.

---

## 4. Extension Protocol — ut_metadata & BEP 9/10

### Metadata Extension

**File**: `/src/MonoTorrent.Client.Modes/MetadataMode.cs:48-100`

MonoTorrent handles magnet link metadata fetching via the `MetadataMode` state. It uses the Libtorrent `ut_metadata` extension:

- Fragments metadata into ~16 KiB blocks (treated as a "virtual piece").
- Peers supply metadata blocks via `LTMetadata` messages.
- Layer merkle hashes fetched concurrently with base metadata for v2 torrents.

No user-facing API for ut_metadata; it's transparent—metadata appears in the `Torrent` property once complete.

### ut_metadata & Layer Proofs

The merkle layer access method shown in §2 (`TryGetV2Hashes` with layer/index/proofCount) is used to serve metadata and layer proofs to peers requesting them via BEP 9/10 extensions.

---

## 5. Hash Verification — Block Reception vs. Piece Validation

### Block Reception

**File**: `/src/MonoTorrent.Client/Managers/PieceManager.cs:67-86`

```csharp
internal bool PieceDataReceived(PeerId id, PieceMessage message, out bool pieceComplete, 
                                HashSet<IRequester> peersInvolved) {
    var isValidLength = Manager.Torrent!.BytesPerBlock(...) == message.RequestLength;
    if (Initialised && isValidLength && 
        Requester.ValidatePiece(id, new PieceSegment(...), out pieceComplete, peersInvolved)) {
        // Block is syntactically valid; queue piece for hashing if complete
        if (pieceComplete)
            PendingHashCheckPieces[message.PieceIndex] = true;  // Mark for hash check
        return true;
    }
}
```

Block validation is structural: correct size, not already received. No cryptographic check.

### Piece Hashing

Pieces are hashed asynchronously by a background task. The `PieceHashed` event fires once the hash is computed:

**File**: `/src/MonoTorrent.Client/Managers/TorrentManager.cs:145-149`

```csharp
public event EventHandler<PieceHashedEventArgs>? PieceHashed;
```

The hash check sets or clears the bitfield:

```csharp
internal void PieceHashed(int pieceIndex) {
    if (Initialised)
        PendingHashCheckPieces[pieceIndex] = false;  // No longer pending
}
```

### Separation of Concerns
✓ Block receipt (syntax) separate from piece verification (hash).  
✓ Pending hash queue (`PendingHashCheckPieces` bitfield) decouples download pace from hash latency.  
✓ Hash results are events, allowing UI updates and downstream state changes.

---

## 6. Pain Points & Design Regrets

### Peer-ID Rigidity (Not Published, but Inferred)
- No configuration for client code/version in public API.
- Limits private-tracker client whitelisting scenarios (tracker's problem, not MonoTorrent's).

### Event Model Callback Hell
- Many event handlers; no filtering, backpressure, or ordering guarantees.
- Main loop scheduler is a bottleneck for high-frequency events (peer churn).
- No typed event bus; each event type is a separate `EventHandler<T>`.

### Metadata Fetching Transparency
- Metadata fetch (`MetadataMode`) is a hidden state transition; user sees the `.Torrent` property suddenly populate.
- No progress indicator for metadata downloads (common complaint in UI implementations).

### Mode-Based State Machine
- Each state is a separate class (`DownloadingMode`, `SeedingMode`, etc.). Transitions are imperative (`mode = new X()`), not declarative.
- Hard to visualize the full state graph; implicit preconditions on state changes.

### Single-Threaded Main Loop
- All I/O and logic runs on `ClientEngine.MainLoop` (a single `Thread` with a priority queue).
- High-concurrency scenarios (many torrents) compete for CPU.
- Profiling requires flame graphs; no structured observability.

---

## 7. What Magpie Should Borrow

### From MonoTorrent:

1. **Dual-hash container (v1/v2)** — `ReadOnlyPieceHash` struct with empty/non-empty invariant.  
   Translates to Rust: `enum PieceHash { V1(Sha1), V2(Sha256) }`.

2. **Per-file merkle roots with lazy layer dictionaries** — Avoid loading all layers upfront.  
   Translates: `HashMap<MerkleRoot, MerkleTree>` in v2 impl, populated on demand.

3. **Factory methods for multi-config add** — `AddAsync(magnet)`, `AddAsync(torrent)`, `AddAsync(path)`.  
   Translates: Builder or overloaded functions; Rust prefers `From`/`Into`.

4. **Bitfield for progress tracking** — Fast bitwise ops, no floating-point rounding.  
   Translates: `bitvec` crate or similar.

5. **ConnectionMonitor trait for speed/totals** — Encapsulates rate limiting, byte counters, timers.  
   Translates: `pub struct PeerStats { bytes_up: u64, bytes_down: u64, rate_up: Rate, ... }`.

### From PeerID Builder Absence:

6. **Design your own peer-ID builder** — Accept `client_code: [u8; 2]`, `version: [u8; 4]`, suffix randomness.  
   Example Rust:
   ```rust
   pub struct PeerIdBuilder {
       client_code: [u8; 2],
       version: [u8; 4],
   }
   impl PeerIdBuilder {
       pub fn build(&self) -> [u8; 20] {
           // "-XX1234-XXXXXXXXXX"
       }
   }
   ```

---

## 8. What Magpie Should Avoid

1. **Hardcoded client identity** — Always expose `client_code`, `version`, `user_agent` as configuration.

2. **Event-callback-based messaging for high-frequency updates** — Piece hashing, block reception, peer state changes.  
   Use bounded MPSC or broadcast channels instead; allow consumers to filter/batch.

3. **Implicit state transitions via hidden mode subclasses** — Use an explicit state machine (enum or type-state pattern).  
   Make preconditions and postconditions explicit in the API.

---

## Summary Table

| Aspect | MonoTorrent | Magpie (Recommendation) |
|--------|-------------|------------------------|
| **Peer-ID Configurability** | Hardcoded (NO) | Configurable builder (YES) |
| **v2 Hash Abstraction** | `IPieceHashes` interface ✓ | `PieceHash` enum ✓ |
| **Merkle Layers** | Per-file dict, lazy ✓ | Per-file dict, lazy ✓ |
| **Public API Surface** | Disciplined, `pub` types ✓ | Same ✓ |
| **Add Torrents** | Multiple overloads ✓ | Same ✓ |
| **State Machine** | Hidden mode subclasses ✗ | Explicit enum (type-state?) ✓ |
| **Events** | EventHandler callbacks ✗ | Broadcast channels ✓ |
| **Block vs. Hash Separation** | Clear ✓ | Same ✓ |
| **Metadata Fetch Progress** | Hidden ✗ | Explicit event ✓ |
| **Concurrency Model** | Single-threaded scheduler | Tokio multi-threaded ✓ |

---

## References

- PeerID: `src/MonoTorrent/PeerID.cs:48-414` (enum parsing), `src/GitInfoHelper.cs` (generation).
- V2 Hashes: `src/MonoTorrent/PieceHashesV{1,2}.cs`, `src/MonoTorrent/IPieceHashes.cs`.
- TorrentManager API: `src/MonoTorrent.Client/Managers/TorrentManager.cs:49-432`.
- State Enum: `src/MonoTorrent/Enums.cs:32-45`.
- Metadata: `src/MonoTorrent.Client.Modes/MetadataMode.cs:48-100`.
- Piece Hashing: `src/MonoTorrent.Client/Managers/PieceManager.cs:67-86`, `166-170`.
