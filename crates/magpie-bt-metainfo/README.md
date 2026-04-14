# magpie-bt-metainfo

`.torrent` metainfo parser for the [magpie](https://github.com/mlamp/magpie) BitTorrent library. Supports BEP 3 (v1), BEP 52 (v2), and hybrid torrents.

**Status**: M0 phase B complete — v1/v2/hybrid parsing, `InfoHash` enum, raw-span hash preservation, property + fuzz + bench coverage.

## Highlights

- **Three variants, one API**: v1-only, v2-only, and hybrid are detected automatically from the info dict.
- **Hash-correct by construction**: `info_hash` is computed from the **exact bytes** in the source buffer (via [`magpie_bt_bencode::dict_value_span`]), never from a re-encode.
- **Zero-copy**: every byte string in `MetaInfo` borrows directly from the input.
- **Typed errors**: `ParseError` pinpoints missing/invalid fields.

## Quick start

```rust
use magpie_bt_metainfo::{parse, InfoHash};

let bytes: &[u8] = /* …contents of a .torrent file… */
#   b"d4:infod6:lengthi13e4:name5:hello12:piece lengthi32768e6:pieces20:aaaaaaaaaaaaaaaaaaaaee";
let meta = parse(bytes).unwrap();

match meta.info_hash {
    InfoHash::V1(h)            => println!("v1 hash: {h:x?}"),
    InfoHash::V2(h)            => println!("v2 hash: {h:x?}"),
    InfoHash::Hybrid { v1, v2 } => println!("hybrid: v1={v1:x?} v2={v2:x?}"),
}
```

See the [M0 foundations milestone](https://github.com/mlamp/magpie/blob/main/docs/milestones/000-foundations.md) for scope.

## Stability

The `test-support` Cargo feature (added in M2) exposes a deterministic
synthetic torrent generator at `magpie_bt_metainfo::test_support`. This
module is **semver-exempt** — its function surface and output format may
change in any release without a major version bump. Use it only from tests
and CI harnesses, never from production code.
