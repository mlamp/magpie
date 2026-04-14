# magpie-bt

Facade crate for the [magpie](https://github.com/mlamp/magpie) BitTorrent library. Re-exports the public API from the `magpie-bt-*` member crates.

Consumers should depend on this crate, not the individual subcrates.

**Status**: M0 complete — non-network foundation (bencode, metainfo, storage, picker, alert ring, peer-ID) is shippable. Network-facing APIs (trackers, peer wire, DHT, uTP) land from M1 onward.

## Surface (M0)

- `magpie_bt::bencode` — zero-copy bencode codec.
- `magpie_bt::metainfo` — `.torrent` parser for v1, v2, and hybrid.
- `magpie_bt::alerts` — custom event ring (`Alert`, `AlertQueue`, `AlertCategory`).
- `magpie_bt::peer_id` — Azureus-style 20-byte peer-ID builder.
- `magpie_bt::picker` — rarest-first piece picker with endgame.
- `magpie_bt::storage` — `Storage` trait + `MemoryStorage` (all platforms) + `FileStorage` (Unix only).

## Quick start

```rust
use magpie_bt::{parse, InfoHash};

let bytes: &[u8] = /* …a .torrent file… */
#   b"d4:infod6:lengthi13e4:name5:hello\
#     12:piece lengthi32768e\
#     6:pieces20:aaaaaaaaaaaaaaaaaaaaee";
let meta = parse(bytes).unwrap();
match meta.info_hash {
    InfoHash::V1(h)            => println!("v1 hash: {h:x?}"),
    InfoHash::V2(h)            => println!("v2 hash: {h:x?}"),
    InfoHash::Hybrid { v1, v2 } => println!("hybrid: {v1:x?} / {v2:x?}"),
}
```
