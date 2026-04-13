<!--
Before opening: read docs/DISCIPLINES.md. CI enforces every bar listed there.
-->

## What & why

<!-- One paragraph. Link the milestone (docs/milestones/NNN-*.md) this work advances. -->

## Change kind

- [ ] Feature
- [ ] Bug fix
- [ ] Refactor (no behaviour change)
- [ ] Docs / ADR
- [ ] Tooling / CI

## Checklist

- [ ] Tests added / updated (unit, integration, property as applicable).
- [ ] Fuzz target updated if I touched a parser.
- [ ] `cargo fmt`, `cargo clippy -D warnings`, `cargo test` all pass locally.
- [ ] Public-API changes recorded in `CHANGELOG.md` under `## [Unreleased]`.
- [ ] If this is a non-trivial design decision, I added/updated an ADR in `docs/adr/`.
- [ ] If I added `unsafe`, each block has a `// SAFETY:` comment and the crate's allowlist in `docs/DISCIPLINES.md` covers it.

## Breaking changes

<!-- State "None" or describe. If any, link or draft the CHANGELOG entry. -->
