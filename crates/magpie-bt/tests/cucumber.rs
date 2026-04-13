#![allow(clippy::missing_const_for_fn, clippy::trivial_regex)]
//! Cucumber (BDD) test harness for magpie.
//!
//! Features live in `tests/features/`, indexed by BEP number. See
//! `docs/bep-coverage.md` for the live coverage matrix.
use cucumber::World;

#[derive(World, Debug, Default)]
pub(crate) struct MagpieWorld {
    // State accumulated across scenario steps. Extended as features land.
}

mod steps;

#[tokio::main]
async fn main() {
    MagpieWorld::run("tests/features").await;
}
