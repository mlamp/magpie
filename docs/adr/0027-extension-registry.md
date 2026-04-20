# 0027 — Extension registry + dispatch (BEP 10)

- **Status**: accepted
- **Date**: 2026-04-21 (post-hoc acceptance at M3 close)
- **Deciders**: magpie maintainers
- **Consulted**: BEP 10 § "Extension Protocol", rasterbar's extension handler dispatch, librqbit's per-peer extension map

## Context

BEP 10 defines a generic extension protocol where peers exchange a
bencode-encoded handshake mapping extension *names* (stable across
the ecosystem — `ut_metadata`, `ut_pex`, etc.) to *message IDs*
(negotiated per-peer, ephemeral). Every extension message on the
wire is an `Extended` frame whose discriminator byte is the peer's
chosen ID; the receiver must reverse-map that ID back to the
canonical name before dispatching to the handler.

Design decisions needed:
- Where does the ID ↔ name mapping live (per-peer or engine-global)?
- When is the handshake exchanged, and what happens on late / missing
  handshakes?
- How is the payload dispatched to handlers once the name is known?
- How does a new extension register itself?

## Decision

### Shape

Per-peer **`ExtensionRegistry`** struct owned by each peer connection:

```rust
pub struct ExtensionRegistry {
    /// Our local name → our local ID mapping. Constant per peer.
    local: HashMap<String, u8>,
    /// Remote's name → remote's ID mapping. Populated from the
    /// peer's BEP 10 handshake.
    remote: HashMap<String, u8>,
}
```

Location: `crates/magpie-bt-wire/src/extension.rs`.

API:
- `ExtensionRegistry::new(local: HashMap<String, u8>)` — build with
  our static local assignment.
- `set_remote(&mut self, hs: &ExtensionHandshake)` — populate from
  decoded peer handshake.
- `local_id(&self, name: &str) -> Option<u8>` — send-side lookup.
- `remote_id(&self, name: &str) -> Option<u8>` — receive-side send.
- `local_name_for_id(&self, id: u8) -> Option<&str>` — reverse
  lookup for inbound dispatch.
- `our_handshake(&self) -> ExtensionHandshake` — emit our outbound
  handshake.

### Handshake exchange

`peer.rs::exchange_extension_handshake()` runs once post-BEP-3
handshake when both sides advertise BEP 10 support (reserved bit
set):

1. Send ours.
2. Wait up to `extension_handshake_timeout` (default 10 s) for
   theirs.
3. On receipt, call `registry.set_remote(&decoded)`.
4. On timeout or non-handshake first extension message, keep the
   connection and process messages normally — BEP 10 is lenient.
   Late extension-ID-0 messages are accepted as handshakes and
   update the registry retroactively.

### Dispatch

Inbound `Message::Extended { id, payload }`:

1. `registry.local_name_for_id(id)` → canonical name.
2. If unknown, drop silently (`id == 0` is BEP 10's "extension
   disabled" sentinel; unknown IDs are non-fatal by spec).
3. Otherwise forward `PeerToSession::ExtensionMessage { name,
   payload }` to the session, which routes to the named handler
   (`ut_metadata`, `ut_pex`, …).

Payload size is capped at **1 MiB** at the wire layer before
dispatch so an adversary can't trivially OOM us.

### Parser hardening

`ExtensionHandshake::decode`:
- Rejects non-UTF-8 extension names (strict by spec; names are
  ASCII in every real extension).
- Rejects `m` dicts with > **128 entries** (bounds memory from a
  hostile handshake).
- Skips `id = 0` entries per BEP 10.
- Rejects `metadata_size > 16 MiB` (matches ADR-0028's assembler
  cap).

Fuzz target: `crates/magpie-bt-wire/fuzz/fuzz_targets/extension_handshake.rs`
with 4-entry corpus. Nightly CI runs it ≥ 600 s per
`.github/workflows/nightly.yml`.

## Consequences

Positive:
- Per-peer map lives exactly where it's needed; no global state.
- Every field in the decoded handshake is bounded at decode time,
  so downstream code doesn't re-check.
- `local_name_for_id` is O(n) over a handful of extensions
  (typically ≤ 8); no complex inverse-map machinery.

Negative:
- Per-peer allocation of two `HashMap<String, u8>`s. Negligible
  in practice (peers count in the low thousands, not millions).

Neutral:
- Late / missing handshake is handled by leniency rather than
  dropping the peer — matches BEP 10 spec behaviour and what real
  clients do in the wild.

## Alternatives considered

- **Engine-global name ↔ id map**. Rejected: IDs are per-peer-
  ephemeral; sharing would require locking on every message, and
  doesn't model the protocol cleanly.
- **Reject unknown extension IDs by disconnecting**. Rejected:
  violates BEP 10 leniency; a future BEP we don't support yet
  becomes a hard break.
- **Wait for handshake before processing any peer traffic**.
  Rejected: some clients send normal peer messages (bitfield,
  have) before their extension handshake. Blocking peer traffic
  stalls the swarm.
