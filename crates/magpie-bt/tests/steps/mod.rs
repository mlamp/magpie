//! Step definitions for cucumber scenarios.
//!
//! One module per domain (wire, tracker, metainfo, picker, ...). Populated
//! incrementally as BEP features land.
use cucumber::given;

use crate::MagpieWorld;

#[given("the cucumber harness is loaded")]
fn harness_loaded(_: &mut MagpieWorld) {
    // Intentional no-op. Proves the cucumber runner + step resolution wire
    // is functional. Replaced with real steps as M1 scenarios land.
}
