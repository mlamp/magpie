# magpie — guide for AI assistants

`magpie` is a greenfield, general-purpose Rust BitTorrent library. [lightorrent](../lightorrent) is the current reference consumer used as a design sanity check for API completeness — not a milestone gate or API-shape driver. For the full *why*, read [docs/PROJECT.md](docs/PROJECT.md).

Current status: **M2 in-progress** (seeder + multi-torrent — consumer-integration ready). Cargo workspace exists; A/A2/B/C/D/E/G largely implemented, remaining work is observability tail, interop, verification gates, and consumer-surface audit. See `docs/MILESTONES.md`.

## File map

| Path | Purpose | Edit when |
|---|---|---|
| `docs/PROJECT.md` | Stable *why/what*: motivation, non-goals, architecture principles, BEP strategy, crate layout. | Motivation or principles change. |
| `docs/DISCIPLINES.md` | Canonical list of quality bars (safety, testing, engineering, process, security). | A new bar is added or an existing one changes. |
| `docs/adr/` | Architecture Decision Records. One file per non-trivial design decision. | Before introducing `unsafe`, hard runtime/ecosystem commitments, or notable design choices. |
| `docs/archive/` | Historical context (e.g. original librqbit gap analysis). Do not edit unless correcting a factual record. | Rare. |
| `docs/ROADMAP.md` | Sequenced phases M0→M6 with scope + gate per milestone. | Phases re-sequenced or gates revised. |
| `docs/MILESTONES.md` | Status index of all milestones. | A milestone moves `not-started → planned → in-progress → done`. |
| `docs/milestones/NNN-*.md` | Per-milestone detail. Created on transition to `planned`. | During a milestone, this is the source of truth for in-flight scope. |
| `docs/research/` | Findings from reading reference implementations. Populated during the research pass. | When analysing a reference repo. |
| `_tmp/` | Gitignored. Reference-repo clones live here — read-only, never commit. | Clone into this dir only. |
| `CLAUDE.md` | This file. | Ways of working change. |
| `README.md` | Short external pitch + pointers. | Top-level status changes (e.g. pre-M0 → M0). |

## Disciplines

Every change must clear the bars in [`docs/DISCIPLINES.md`](docs/DISCIPLINES.md). CI enforces them mechanically — read that file before starting work. A rule that isn't there isn't a discipline.

## Ways of working

1. **Start every task by checking `docs/MILESTONES.md`.** The milestone currently marked `in-progress` dictates what's in scope. If none is in progress, the next `planned` milestone defines scope.
2. **In-flight work lives in the milestone detail file.** Update its deliverables checklist as items land. Don't scatter progress into the roadmap or project doc.
3. **`PROJECT.md` is stable.** Only edit when motivation, non-goals, or architecture principles genuinely change — not for routine progress.
4. **`ROADMAP.md` changes on phase re-sequencing.** Adjusting scope *within* a milestone goes in the milestone file; moving scope *between* milestones updates the roadmap.
5. **A new detail file is created when a milestone transitions `not-started → planned`** — not before. Defer scoping until the predecessor milestone is close to done.
6. **Reference implementations** (cratetorrent, anacrolix/torrent, libtorrent-rasterbar, librqbit, MonoTorrent, lambdaclass/libtorrent-rs) are cloned into `_tmp/` during the research pass. They are read-only. Findings distilled into `docs/research/`.
7. **Crate prefix on crates.io is `magpie-bt-*`.** The bare `magpie` name is taken.

## Doing actual work

- No Cargo workspace exists yet, so `cargo` commands will not run until M0 is kicked off.
- When M0 kicks off, the workspace is created at the repo root with member crates `magpie-bt-bencode`, `magpie-bt-metainfo`, `magpie-bt-wire`, `magpie-bt-core`, `magpie-bt`.
- Until then, all tasks are documentation, research, or planning.
