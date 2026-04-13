# Contributing to magpie

Thanks for your interest. Before you write code, please skim the following — they are short and they set expectations:

1. [`README.md`](README.md) — what magpie is.
2. [`CLAUDE.md`](CLAUDE.md) — how this repo is organised.
3. [`docs/PROJECT.md`](docs/PROJECT.md) — the *why*.
4. [`docs/ROADMAP.md`](docs/ROADMAP.md) — sequenced phases.
5. [`docs/MILESTONES.md`](docs/MILESTONES.md) — what is in flight.
6. **[`docs/DISCIPLINES.md`](docs/DISCIPLINES.md)** — the bars every PR must clear. Non-negotiable.

## Workflow

- **Pick work that fits the current milestone.** In-flight scope lives in `docs/milestones/NNN-*.md`. Work outside that scope goes on the roadmap first.
- **Open an issue before large PRs.** "Large" = anything touching public API or adding a new BEP.
- **One PR, one concern.** Unrelated refactors go in their own PRs.
- **Tests ship with code.** Unit + integration as applicable. Property tests for parsers/picker. See DISCIPLINES.md for the coverage bars.
- **Update `CHANGELOG.md`** under `## [Unreleased]` for any change that affects public API, behaviour, or MSRV.
- **ADRs** (`docs/adr/`) for non-trivial design decisions. Template in `docs/adr/README.md`.

## What CI will check

`fmt`, `clippy -D warnings`, `test`, `doc`, `cargo-deny check`, coverage thresholds. Nightly: fuzz + miri. See `docs/DISCIPLINES.md` for the full list.

## Licensing

By submitting a contribution, you agree to license it under the dual `Apache-2.0 OR MIT` terms of this project.
