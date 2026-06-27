//! Property tests for magpie-bt-bencode.
//!
//! Two properties guard the codec:
//! 1. Random `Value` → `encode` → `decode` round-trips byte-for-byte.
//! 2. Any canonical input survives `decode` → `encode` unchanged.
#![allow(missing_docs)]

use std::borrow::Cow;
use std::collections::BTreeMap;

use magpie_bt_bencode::{DecodeErrorKind, DecodeOptions, Value, decode, decode_with, encode};
use proptest::prelude::*;

/// Total number of `Value` nodes in a tree — the quantity the decoder charges
/// against [`DecodeOptions::max_nodes`].
fn count_nodes(v: &Value) -> u32 {
    1 + match v {
        Value::List(items) => items.iter().map(count_nodes).sum(),
        Value::Dict(m) => m.values().map(count_nodes).sum(),
        _ => 0,
    }
}

fn arb_value() -> impl Strategy<Value = Value<'static>> {
    let leaf = prop_oneof![
        any::<i64>().prop_map(Value::Int),
        prop::collection::vec(any::<u8>(), 0..32).prop_map(|v| Value::Bytes(Cow::Owned(v))),
    ];
    leaf.prop_recursive(4, 32, 8, |inner| {
        prop_oneof![
            prop::collection::vec(inner.clone(), 0..6).prop_map(Value::List),
            // Dict keys: unique byte strings, enforced by BTreeMap insertion.
            prop::collection::vec((prop::collection::vec(any::<u8>(), 0..16), inner), 0..6,)
                .prop_map(|pairs| {
                    let mut m: BTreeMap<Cow<'static, [u8]>, Value<'static>> = BTreeMap::new();
                    for (k, v) in pairs {
                        m.insert(Cow::Owned(k), v);
                    }
                    Value::Dict(m)
                }),
        ]
    })
}

proptest! {
    #[test]
    fn encode_decode_roundtrip(v in arb_value()) {
        let bytes = encode(&v);
        let back = decode(&bytes).expect("encoded bytes must decode");
        prop_assert_eq!(back.into_owned(), v);
    }

    #[test]
    fn decode_encode_byte_identical_on_canonical(v in arb_value()) {
        let bytes = encode(&v);
        let decoded = decode(&bytes).expect("encoded bytes must decode");
        prop_assert_eq!(encode(&decoded), bytes);
    }

    #[test]
    fn decoder_never_panics_on_arbitrary_bytes(raw in prop::collection::vec(any::<u8>(), 0..256)) {
        // Result may be Ok or Err — we only guarantee no panic.
        let _ = decode(&raw);
    }

    /// The node budget is exact: a tree of `n` nodes decodes with `max_nodes = n`
    /// and is rejected with `max_nodes = n - 1`.
    #[test]
    fn node_budget_is_exact(v in arb_value()) {
        let bytes = encode(&v);
        let n = count_nodes(&v);

        let mut at_budget = DecodeOptions::default();
        at_budget.max_nodes = n;
        prop_assert!(decode_with(&bytes, at_budget).is_ok());

        let mut under_budget = DecodeOptions::default();
        under_budget.max_nodes = n - 1; // `n >= 1` always (the root counts).
        let err = decode_with(&bytes, under_budget).unwrap_err();
        let is_node_limit = matches!(err.kind, DecodeErrorKind::NodeLimitExceeded { .. });
        prop_assert!(is_node_limit, "expected NodeLimitExceeded, got {:?}", err.kind);
    }
}
