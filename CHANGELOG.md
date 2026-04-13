# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added
- Initial documentation scaffold: `README.md`, `CLAUDE.md`, `docs/PROJECT.md`, `docs/ROADMAP.md`, `docs/MILESTONES.md`, `docs/milestones/001-foundations.md`.
- Quality disciplines baseline: `docs/DISCIPLINES.md`, `docs/adr/` scaffold, GitHub CI + nightly workflows, rustfmt/clippy/deny configs, security + contributing policies.
- Dual `Apache-2.0 OR MIT` licensing.

### Changed
- `rustfmt.toml`: edition `2021` → `2024`.
- `clippy.toml`: MSRV placeholder `1.75` → `1.94` (matches latest stable on dev machine, rustc 1.94.1).
