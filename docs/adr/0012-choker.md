# 0012 — Choker

- **Status**: accepted
- **Date**: 2026-04-14
- **Deciders**: magpie maintainers
- **Consulted**: BEP 3 §"Choking and optimistic unchoking"; rasterbar `choker.cpp` (research/003); libtorrent issue on seed round-robin starvation (surfaced during ADR-0017 review)

## Context

M1 was leech-only; the session never issued `Choke` / `Unchoke` from us and didn't care who we'd serve. M2 adds upload, and with it the question of which peers should be allowed to request blocks from us at any given time. BEP 3's answer is two mechanisms run together:

1. **Regular unchoke**: pick N peers to serve; rotate the set every 10 s by a criterion ("tit-for-tat" in the leech case: reward peers who are sending to us).
2. **Optimistic unchoke**: additionally serve 1 random choked peer; rotate every 30 s. Bootstrap new peers and discover unknown-good uploaders.

Seeding has no download rate to reciprocate on, so it needs a different ranking. Rasterbar shipped a round-robin "quota in bytes uploaded" seed choker, discovered years later that **fast peers hit quota first and slow peers ended up owning most upload slots** — the algorithm was inverted. The fix (`fastest_upload` mode) ranks by upload rate to the peer, so fast pipes stay open. Magpie starts with the fixed algorithm.

The choker is ADR-0007-style infrastructure: tick rate is 10 s, not remotely in the hot path. What matters is that the per-peer rate signal it reads is lock-free on the byte-counting side, which ADR-0014 already guarantees (`AtomicU64` per peer).

## Decision

### Unchoker shape: enum, not trait

```rust
enum ChokerMode {
    Leech(LeechState),
    Seed(SeedState),
}
```

Single enum on `Torrent` actor, switched at the leech→seed transition (ADR-0019). An `Unchoker` trait was the first sketch; rejected because both variants live in the same crate, testability is identical, and the torrent actor already switches on this shape in its transition logic. Match arms beat dyn dispatch for a two-case-forever decision.

### Slot counts and rotation

- **4 regular unchoke slots**, configurable per torrent (`max_unchoked`). This is BEP 3's default and appropriate for a residential-link consumer. **Production seeders on fat pipes should raise this**: a 50-peer swarm at 4 + 1 = 5 slots serves only 10 % of interested peers at any time, and on a gigabit uplink 5 slots may leave bandwidth unused. `lightorrent` serving *arr workflows is the canonical "raise me" case — start at 8–16 depending on upstream capacity and peer count. The default stays conservative because the downside of too-high is spreading upload thin across slow peers; the downside of too-low is leaving the pipe unused. Tune up only on measured headroom.
- **1 optimistic unchoke slot**, configurable (`optimistic_slots`, default 1).
- **Regular round: every 10 s** (`choker_interval`, default 10 s).
- **Optimistic round: every 30 s** (`optimistic_interval`, default 30 s). Swarm-size adaptation (rasterbar: shorter cycle in small swarms so new peers don't wait 12 min for a slot) deferred to M5 — revisit when interop evidence shows fixed 30 s hurts small swarms.

Note on tuning: a fixed 30 s optimistic rotation with 1 slot in a 50-peer swarm gives a new peer a mean 12.5-min wait for optimistic unchoke. Raising `optimistic_slots` to 2 is the supported knob before the M5 adaptive work lands.

### Leech ranking (`LeechState`)

Rank interested peers by **20 s-windowed download rate from them** (classic tit-for-tat). Highest-rate N peers take the 4 regular slots. Ties broken by random permutation (avoids starving an arbitrary deterministic order).

**Rate signal**: per-peer `AtomicU64 downloaded_bytes` counters (ADR-0014) are read at each choker tick (10 s). Delta-since-last-sample / elapsed → instantaneous rate → EWMA-updated with τ = 20 s. One `f64` per peer; choker-local state, no contention. The 1 Hz `StatsUpdate` alert (ADR-0014) is consumer-facing and explicitly not the choker's truth source.

**Expected signal lag**: with τ = 20 s sampled every 10 s (two samples per time constant), a peer that goes from 10 MB/s to 0 takes ~20 s for the EWMA to drop to ~37 % of peak and ~60 s to drop below 5 %. The choker therefore provides *gradual demotion* of stalled peers over tens of seconds, not fast ejection. A peer that stops uploading holds its regular slot until either (a) a higher-rate peer outcompetes them on the next tick, which requires their EWMA to have dropped enough to be beaten, or (b) anti-snub fires at 60 s. **Worst-case a stalled peer occupies a regular slot for up to 60 s before being ejected.** This is acceptable because the swarm impact of a single stalled slot is one unchoke round of wasted capacity; making the signal faster (shorter τ) trades smoothing for flap, which rasterbar's experience says costs more than the lag saves.

### Seed ranking (`SeedState`)

Rank interested-and-not-snubbed peers by **20 s-windowed upload rate to them** (rasterbar `fastest_upload`). Highest-rate N peers take the 4 regular slots.

**Not round-robin by bytes-uploaded.** Explicit rejection: rasterbar shipped this and fixed it — quota-by-bytes means fast peers hit their quota in seconds and slow peers accumulate dominance of the unchoke set, strangling total upload throughput. Fastest-upload is the corrected algorithm.

**Need-set hook, unused in M2.** ADR-0005 puts peer bitfields on `PeerState`; `our_have & !peer.have` is available on-demand. The seed ranking formula stays `= upload_rate` for M2 — no need-set weighting. The hook exists for BEP 16 super-seeding (out of scope through M6), where rare-piece distribution drives the ranking. Adding it to M2's rank function adds a factor without a gate criterion to validate it against; keep the algorithm simple and lock in the signal-source contract so M6+ can extend cleanly.

### Optimistic unchoke

One slot (default), rotated every 30 s. Selection:

- Pool = choked peers who are `interested`.
- **New-peer bonus**: peers connected in the last `3 × optimistic_interval` (90 s default) get 3× weight in the random draw. Rasterbar's rule; accelerates discovery of unknown-good peers.
- **Concrete probabilities** (worked example, 10 old + 2 new interested peers): total weight = 10 × 1 + 2 × 3 = 16. Each *individual* new peer has draw probability 3/16 = 18.75 %; each *individual* old peer has 1/16 = 6.25 %. **Per-peer ratio is the weight ratio, 3×** — not 6×, a common confusion when conflating combined-new-weight with individual-old-weight. Combined: new peers 6/16 = 37.5 %, old peers 10/16 = 62.5 %.
- Random weighted pick from pool; no tracking of "already optimistically-unchoked" — rotation means everyone eventually gets a turn.

### Anti-snub (leech mode only)

A peer we've regularly-unchoked but who sends **zero bytes for 60 s** is **snubbed**: removed from the regular set and a fresh candidate takes the slot. Snubbed peers are eligible for optimistic unchoke but not regular until they upload again. "Zero bytes" is checked against the rate EWMA; precise enough and reuses the choker's existing signal.

### Seed-mode: no anti-snub. Handled in ADR-0017

Seed mode doesn't track peer-side snubbing because the upload path (ADR-0017) already disconnects peers who keep requesting blocks while choked past the 2 s grace window and caps fast-set abuse. Peers who don't drain their send buffer naturally fall out of the seed ranking (low upload-rate-to-them).

### Transition (cross-ref ADR-0019)

Leech→seed swap is atomic from the peer's viewpoint: the torrent actor (1) finishes the current verify + `Have` broadcast, (2) swaps `ChokerMode::Leech → Seed`, (3) **immediately runs one seed-mode unchoke round** (re-evaluates the whole unchoke set, not waiting for the current 10 s tick to finish), (4) **resets the regular and optimistic tick timers to fire `choker_interval` / `optimistic_interval` after the transition instant**. Peers currently unchoked by `LeechChoker` are the baseline; those not making the seed cut get `Choke`d in the same tick. No ride-out. If no peers are `interested` at the transition instant, the re-eval unchokes nobody — correct behaviour, the choker stays idle until a new `Interested` arrives, and the reset timers prevent a stale leech-scheduled tick from firing a pointless empty seed evaluation 1 s later. Documented in ADR-0019; cross-referenced here because it's where the choker changes identity.

### Configuration surface

```rust
pub struct ChokerConfig {
    pub max_unchoked: usize,           // default 4
    pub optimistic_slots: usize,       // default 1
    pub choker_interval: Duration,     // default 10 s
    pub optimistic_interval: Duration, // default 30 s
    pub anti_snub_timeout: Duration,   // default 60 s
    pub rate_ewma_tau: Duration,       // default 20 s
}
```

All six tunable per session and per torrent. Defaults are the BEP 3 + rasterbar convergent set.

## Consequences

Positive:

- One algorithm per mode, no hybrid-scoring-across-modes confusion.
- Fast-pipe peers stay open in seed mode — the rasterbar round-robin bug is precluded by construction.
- Rate signal is the same `AtomicU64` stream that ADR-0014 already maintains for stats; the choker doesn't need a second byte counter or a separate EWMA maintainer. Single source of truth per peer.
- Anti-snub runs off the same rate EWMA — no extra state.
- New peers bootstrap within `optimistic_interval` × (slots worth of rounds before they're likely picked), which is tunable without shipping swarm-size-adaptive code. Small-swarm users raise `optimistic_slots` to 2.
- Leech→seed transition is one atomic tick (ADR-0019) — no mid-transition weirdness where slots belong to neither algorithm.

Negative:

- `SeedState` doesn't use the need-set. A peer requesting ultra-rare pieces that only we have gets no preferential treatment over a peer requesting common pieces, as long as their upload rates match. This is rasterbar's current behaviour and is fine for general seeding; super-seeding (when we *want* to skew toward rare-piece distribution) is a separate algorithm landing later.
- Fixed 30 s optimistic interval means new peers on large swarms may wait 10+ minutes for a first-round optimistic unchoke. Tunable mitigates; swarm-size-adaptive fix scheduled M5.
- EWMA rate signal lags real rate by one τ (20 s). Choker decisions react one tick slower than an instantaneous-rate signal would. Acceptable: the rotation interval is 10 s, and the signal smoothing reduces flap between peers whose short-term rates oscillate.

Neutral:

- The choker config surface is session- and torrent-level tunable; magpie does not ship per-peer choker configuration. Per-peer priorities are future work.

## Alternatives considered

- **`Unchoker` trait with `LeechChoker` / `SeedChoker` impls.** First sketch; rejected. Single enum is simpler, matches the rest of magpie's style (picker is also not a trait), and the transition flip is a natural `match` not a `Box<dyn>` swap.
- **Round-robin seed choker with bytes-uploaded quota.** Rejected. This was rasterbar's original shipped algorithm; the bug is well-documented: slow peers accumulate dominance of the upload set because fast peers exhaust their per-round quota first and get rotated out. `fastest_upload` is the shipped fix; we adopt it from day one.
- **Need-set weighting in the seed rank.** First plan pencilled this in. Rejected for M2: adds a factor without a gate criterion to validate it against. The `our_have & !peer.have` hook is available on-demand per ADR-0005 and will drive super-seeding later (BEP 16, out of scope through M6). Keep the algorithm narrow until there's a reason to widen it.
- **BitTyrant / PropShare / other game-theoretic leech algorithms.** Rejected. Marginal empirical wins, significant complexity, and known fragility under adversarial peer populations. BEP 3 tit-for-tat is the Schelling point.
- **Instantaneous rate instead of 20 s EWMA.** Rejected: causes slot flap on bursty peers (big `Piece` arriving mid-tick spikes the rate for one tick, then zero). EWMA smooths the signal at the cost of one-τ latency, which is well below the 10 s rotation.
- **Optimistic unchoke "new-peer multiplier" absent.** Considered for simplicity. Rejected: on a busy swarm, brand-new peers never win the random draw without the bonus because old peers dominate the pool. Rasterbar's 3× multiplier costs nothing and closes a real fairness gap.
