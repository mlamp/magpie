# M? — <Milestone name>

**Status**: not-started | planned | in-progress | done
**Gate summary**: one-sentence description of what proves this milestone is done.

## Goal

Why this milestone exists. One paragraph.

## Scope / deliverables

- [ ] Concrete deliverable 1
- [ ] Concrete deliverable 2
- [ ] …

## Gate criteria (verification)

Every item here must be mechanically checkable, not a judgement call.

1. **Tests** — unit + integration + property coverage at the bars in [`../DISCIPLINES.md`](../DISCIPLINES.md).
2. **Fuzz** — any parser/protocol code added is behind a `cargo-fuzz` target; corpus committed; nightly CI green for ≥10 min per target.
3. **Benchmarks** — criterion baselines for any hot-path code added or changed; no >5 % regressions.
4. **Docs** — every new `pub` item has a rustdoc summary; `cargo doc -D warnings` clean.
5. **ADRs** — any non-trivial design decision made during this milestone has an ADR under `../adr/`.
6. **CHANGELOG** — `## [Unreleased]` reflects all public-API changes from this milestone.
7. **Milestone-specific criteria** — …

## Open questions

- Question 1 (status: open | resolved-in ADR-NNNN | resolved-inline)
- …

## Out of scope

- Item deliberately not tackled here; link the milestone where it belongs if known.
