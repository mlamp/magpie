# 0019 — Leech→seed completion transition

- **Status**: accepted
- **Date**: 2026-04-14
- **Deciders**: magpie maintainers
- **Consulted**: BEP 3 ("completed" tracker event), ADR-0007 (disk-completion ordering), ADR-0012 (choker modes + timer reset), ADR-0017 (upload path), ADR-0018 (read cache promotion)

## Context

M2 is where a torrent can finish downloading and start seeding without being torn down and re-added. That lifecycle change is more than flipping a bit: four subsystems must converge at the transition instant (alerts, tracker, choker, interest state), and three of them — tracker `event=completed`, choker mode swap, outgoing `NotInterested` — have visible wire consequences. Getting the order wrong creates observable bugs: slots held by peers we no longer serve, duplicate `completed` tracker events, stale choker ticks firing seed-mode evaluations against leech-mode state, and so on.

ADR-0007 establishes that `our_have[piece]` flips only after disk acknowledges, so by the time the torrent observes `our_have.all_set()`, every piece is on disk and `DiskWriter::pending_writes` is empty for this torrent. That eliminates one class of question (no mid-transition flush-window races) and lets the transition itself be purely logical.

## Decision

### Trigger

The torrent actor fires the transition **exactly once**, the first tick after a `DiskCompletion { verified: true }` makes `our_have.all_set() == true`. Transition is **forward-only** — if a piece later fails re-verification (file deleted under us, storage corruption), magpie emits an alert and the picker re-requests the piece, but the torrent stays in `ChokerMode::Seed`. Rasterbar does the same; back-transition adds complexity with no operator benefit — restart handles the edge.

Torrents loaded from resume data with all pieces already present skip the transition entirely: the session constructs them directly in `ChokerMode::Seed` and announces `event=started` normally (not `completed`, which most trackers reject for a torrent that was never in progress).

### Sequence (strict order)

Five steps, synchronous within the torrent actor tick. No ride-out of the current choker interval — the transition fires the moment completion is observed.

**Guard**: the torrent actor atomically checks and sets `completion_fired: bool` **before** entering step 1. If already set, the whole block short-circuits — no alert, no interest broadcast, no tracker announce, no choker swap, no unchoke round. All five steps are covered by one flag, not just the externally-visible ones.

1. **Emit `Alert::TorrentComplete { info_hash, at: Instant }`** on the event ring. First so a lightorrent-style observer sees completion before the burst of upload-side wire traffic begins.
2. **Swap `ChokerMode::Leech → Seed`** on the torrent. **Reset both choker timers** (regular-tick and optimistic-tick) to fire `choker_interval` / `optimistic_interval` from the transition instant. The reset is load-bearing: a stale leech-scheduled tick firing 1 s post-swap would run an empty or misdirected seed evaluation. Per ADR-0012 §Transition.
3. **Immediately run one seed-mode unchoke round.** Do not wait for step 2's timer. Peers currently regular-unchoked by the leech choker are the baseline; those not making the seed cut receive `SetChoking(true)` → `Choke` on the wire. Any peer whose `interested` flag is already set and who ranks in the top N by upload-rate-to-them takes a regular slot. First-round optimistic slot is also drawn. If no peers are `interested` at the transition instant, the round unchokes nobody — the choker sits idle until the next `Interested` arrives.
4. **Send `NotInterested` to every peer in `we_are_interested` set.** Our last act as a leecher: stop declaring interest in peers whose bitfields no longer matter. **Placed after the unchoke round** (steps 2–3), not before, because some clients interpret an incoming `NotInterested` as "drop my `Interested` flag toward this peer too" — if we sent `NotInterested` first and *then* unchoked them, they could have dropped interest in the interval, leaving us with unchoked-but-not-interested slots that sit idle until their next interest re-evaluation. Order `Unchoke` → `NotInterested` means any peer we just unchoked sees "you're unchoked; I'm no longer downloading from you" in the correct sequence: I'm a seeder now, here's your slot.
5. **Fire-and-forget tracker announce `event=completed`** on the tracker client task. **Non-blocking**: the transition must not wait for tracker response, because a stuck tracker would gate our ability to serve peers who already want blocks. The tracker task handles retries per its own policy; transition continues immediately. Placed last because it's the least time-sensitive and most failure-tolerant of the five steps.

### What explicitly does **not** happen at transition

Documented because the absence is as intentional as the presence.

- **No per-peer need-set fixup**. Need-sets are computed on-demand as `our_have & !peer.have` at each SeedChoker read (ADR-0005). The `our_have` flip is already visible to the next read; no cached state to invalidate.
- **No read-cache flush**. ADR-0018's `ReadCache` may or may not have entries for the freshly-completed torrent; eviction is LRU-only. Seed reads will hit the cache naturally as peers request pieces.
- **No disk-writer drain wait**. `our_have.all_set()` is post-flush per ADR-0007's ordering; nothing pending.
- **No endgame teardown**. Endgame was a picker mode, not a shared object; the picker simply stops being invoked. `in_progress` is already empty.
- **No `Have` re-broadcast for pieces we already broadcast**. The last piece's own completion emits its `Have` via the normal per-piece path, before the transition tick runs.

### Idempotency

One flag guards the whole block: `completion_fired: bool` per torrent, checked-and-set atomically *before* step 1. A spurious second transition fire sees the flag set and exits before emitting anything. Belt-and-braces inside the sequence: step 2 also no-ops if `ChokerMode` is already `Seed`, which covers the race where two threads observe `our_have.all_set()` simultaneously and both enter the section — the loser on the atomic flip on `completion_fired` aborts; the `ChokerMode` check is defence in depth.

## Consequences

Positive:

- **One code path owns the transition.** Alert, tracker, interest, choker, all in one five-step block in the torrent actor. Easy to read, easy to test, easy to trace.
- **Peer-visible atomicity.** A remote peer observing us at transition sees at most (a) our regular per-block traffic, (b) a `NotInterested` if applicable, (c) an updated choke state in the same tick. No intermediate state where we're simultaneously a leech (for choker purposes) and a seed (for interest purposes).
- **Tracker never gates upload.** Even a dead tracker doesn't prevent us from serving peers who already want blocks. Operators like this invariant.
- **Matches ADR-0012's timer-reset contract.** ADR-0012 §Transition already declared this ordering; 0019 is the authoritative source for the full sequence.
- **Forward-only simplicity.** No mode-thrashing from file flaps; operators diagnose those out-of-band.

Negative:

- **Non-blocking tracker announce can fail silently.** If `event=completed` never reaches the tracker (network failure, tracker crash), our seeder status isn't registered there. Mitigation: the regular announce cycle continues, and the next normal announce carries our `left=0` state, which trackers use as a seeder signal regardless of whether they received `completed`. Functional net: the tracker figures out we're a seeder within one announce interval even if the `completed` is lost.
- **Back-transition impossible without restart.** Deliberate. A torrent whose files disappear mid-seed stops being a useful seed immediately, but the choker continues in Seed mode, potentially unchoking peers against stale `our_have`. Mitigation: file-corruption alerts are severe; operators are expected to act.
- **Transition under load can flap the unchoke set.** Step 5 may change who is unchoked for every peer in the regular set. Peers who had been receiving blocks from us as leechers will receive a `Choke` if they're not seed-choker-competitive. This is correct; flagged here so the observability layer can explain "why did all my unchokes change at completion" to operators.

## Alternatives considered

- **Ride out the current leech choker tick, transition on the next boundary.** Rejected: BEP 3 regular interval is 10 s; waiting up to that long after completion means 10 s where we're a seeder who's not serving optimal peers. The immediate re-eval is worth the one-tick flap.
- **Synchronous tracker announce (block transition on response).** Rejected: a slow or unreachable tracker would delay upload of a completed torrent. The fire-and-forget path with next-announce-carries-`left=0` fallback is strictly better.
- **Leave `Interested` set on peers, let it fall off naturally.** Rejected: `NotInterested` is cheap (one message per peer), and leaving stale interest declared is a protocol smell that shows up in wire captures during interop debugging.
- **Back-transition on file disappearance.** Rejected: rare, destabilising, and rasterbar's precedent is also forward-only. The operator-restart path is acceptable.
- **Fire the transition inside the `DiskCompletion` handler.** Attractive because completion is detected exactly there. Rejected: the completion handler runs in the tick that also processes the disk ack; adding five more steps to it bloats a hot path. A check at the end of the tick — `if newly_complete() { run_transition() }` — keeps the handler narrow.
- **Defer choker re-eval until the next regular tick after transition.** Rejected: ADR-0012 explicitly requires immediate re-eval to avoid the "I'm a seeder but my unchoke set is still my leech set" window. The 10 s ride-out would be observable as throughput jitter right after completion.
