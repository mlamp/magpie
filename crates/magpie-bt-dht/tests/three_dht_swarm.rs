//! Three-DhtRuntime swarm integration test — the DHT-layer part of
//! the M4 hard gate (three engines sharing peers via DHT alone).
//!
//! The full milestone gate #2 runs end-to-end torrent traffic on
//! loopback; that belongs in workstream G where the engine glue
//! lands. This test is the narrower precondition: three in-process
//! `DhtRuntime`s, no engine, no UDP socket, channel-based transport
//! — does `announce` on B then `find_peers` on C from a cold-cache
//! swarm bootstrapped through A return B's address?

use std::collections::HashMap;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::{Mutex, mpsc};

use magpie_bt_dht::{Datagram, DhtRuntime, DhtRuntimeConfig, InfoHash, NodeId};

/// A single DHT's in-memory transport endpoint.
struct Endpoint {
    runtime: DhtRuntime,
    addr: SocketAddr,
    outbound_rx: Arc<Mutex<mpsc::Receiver<Datagram>>>,
}

/// Spawn `count` endpoints; wire a message-broker task that routes
/// every outbound `Datagram` to the endpoint whose synthetic
/// address matches its target `addr`.
#[allow(clippy::unused_async)] // must run inside a tokio runtime (tokio::spawn)
async fn boot_swarm(count: u8) -> Vec<Endpoint> {
    let mut endpoints = Vec::with_capacity(usize::from(count));
    let mut addr_to_inbound: HashMap<SocketAddr, mpsc::Sender<Datagram>> = HashMap::new();

    for i in 0..count {
        let addr = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(127, 0, 0, i + 1), 6881));
        let (inbound_tx, inbound_rx) = mpsc::channel::<Datagram>(256);
        let (outbound_tx, outbound_rx) = mpsc::channel::<Datagram>(256);

        let local_id = NodeId::from_bytes([i + 1; 20]);
        let (runtime, _joins) = DhtRuntime::spawn(
            DhtRuntimeConfig::new(local_id),
            inbound_rx,
            outbound_tx,
            Instant::now(),
        )
        .unwrap();

        addr_to_inbound.insert(addr, inbound_tx);
        endpoints.push(Endpoint {
            runtime,
            addr,
            outbound_rx: Arc::new(Mutex::new(outbound_rx)),
        });
    }

    // Message broker per endpoint: drain outbound, look up target,
    // rewrite the from-address to the sending endpoint's addr, and
    // forward. Runs for the lifetime of the test — the tokio
    // runtime cleans up when the test future drops.
    for ep in &endpoints {
        let rx = Arc::clone(&ep.outbound_rx);
        let sender_addr = ep.addr;
        let routes = addr_to_inbound.clone();
        tokio::spawn(async move {
            loop {
                let next = { rx.lock().await.recv().await };
                let Some(mut dg) = next else { return };
                let Some(dst) = routes.get(&dg.addr) else {
                    continue;
                };
                dg.addr = sender_addr;
                let _ = dst.send(dg).await;
            }
        });
    }

    endpoints
}

/// Pre-seed A with B + C contacts so A can answer `find_node`
/// queries about either of them. In real life the bootstrap round
/// would walk this graph from a DNS hostname.
async fn seed_introducer(a: &Endpoint, b: &Endpoint, c: &Endpoint) {
    let now = Instant::now();
    a.runtime
        .seed_contact(b.runtime.local_id().await, b.addr, now)
        .await;
    a.runtime
        .seed_contact(c.runtime.local_id().await, c.addr, now)
        .await;
}

/// Seed B + C with a single contact — A — so their lookups have a
/// starting point.
async fn seed_client(client: &Endpoint, introducer: &Endpoint) {
    client
        .runtime
        .seed_contact(
            introducer.runtime.local_id().await,
            introducer.addr,
            Instant::now(),
        )
        .await;
}

#[tokio::test]
async fn b_announces_then_c_finds_via_a() {
    let swarm = boot_swarm(3).await;
    let a = &swarm[0];
    let b = &swarm[1];
    let c = &swarm[2];

    seed_introducer(a, b, c).await;
    seed_client(b, a).await;
    seed_client(c, a).await;

    let info_hash = InfoHash::from_bytes([0xab; 20]);

    // B announces — iterative get_peers bootstraps the path to A
    // then announces to the token-bearing nodes (A returns the
    // token because B first `get_peers`es it).
    let _peers_at_b = b
        .runtime
        .announce(info_hash, 51413, false)
        .await
        .expect("B announce");
    // Give the handler loops a moment to record A's side.
    tokio::time::sleep(Duration::from_millis(50)).await;

    // C asks the DHT — iterative get_peers expands through A and
    // returns B's address as a `values` entry.
    let peers_at_c = c
        .runtime
        .find_peers(info_hash, false)
        .await
        .expect("C find_peers");

    // B's address was inserted into A's peer store under the
    // announce_peer path. `find_peers` at C should pick that up.
    assert!(
        peers_at_c.iter().any(|p| p.ip() == b.addr.ip()),
        "C should discover B's IP via A; got {peers_at_c:?}"
    );
}

#[tokio::test]
async fn announce_private_flag_emits_zero_krpc_messages() {
    // BEP 27 private-torrent compliance: announce(private=true)
    // must return Ok(vec![]) and send zero KRPC messages on the
    // outbound channel — regression guard for M4 gate 7.
    let swarm = boot_swarm(1).await;
    let a = &swarm[0];

    // Ensure the routing table has *something* — a non-seeded
    // table would make announce a no-op by default.
    a.runtime
        .seed_contact(
            NodeId::from_bytes([0xff; 20]),
            SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(10, 0, 0, 1), 6881)),
            Instant::now(),
        )
        .await;

    let info_hash = InfoHash::from_bytes([0xab; 20]);
    let peers = a
        .runtime
        .announce(info_hash, 51413, true)
        .await
        .expect("private announce");
    assert!(
        peers.is_empty(),
        "private announce must return empty peer list"
    );

    // Drain outbound for a short window; there must be zero
    // datagrams.
    let got = tokio::time::timeout(Duration::from_millis(100), async {
        a.outbound_rx.lock().await.recv().await
    })
    .await;
    assert!(got.is_err(), "private announce emitted outbound traffic");
}
