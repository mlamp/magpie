# magpie-bt-dht

Mainline DHT (BEP 5) for the [magpie](https://github.com/mlamp/magpie)
BitTorrent library. Kademlia routing, KRPC wire codec, bootstrap,
tokens, and BEP 42 node-ID salting.

This subcrate is the direct product of the M4 milestone. See
[`docs/MILESTONES.md`](../../docs/MILESTONES.md) and ADRs 0024–0026 in
the repo.

Status: **M4 workstream A** — crate skeleton, node IDs, routing-table
data carriers, and KRPC wire codec. Transport wiring, RPC handlers,
and bootstrap follow in workstreams B–G.

## Licence

Dual-licensed under Apache-2.0 or MIT; see the repo root.
