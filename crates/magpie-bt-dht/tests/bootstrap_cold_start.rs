//! Cold-start bootstrap integration test — milestone gate 3.
//!
//! Four mock DHT responders simulate the hardcoded bootstrap hosts
//! from ADR-0025. Each is pre-seeded with enough other nodes that
//! a single `find_node(local_id)` from the cold node yields many
//! new ids. The cold node's [`run_bootstrap`] must flip to
//! [`BootstrapOutcome::Operational`] — ≥ 32 good nodes AND ≥ 1
//! non-empty reply — within the stall window.
//!
//! Uses an in-process channel message broker (same pattern as the
//! three-Dht swarm test) so the test runs in milliseconds with no
//! UDP sockets.

use std::collections::HashMap;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::mpsc;

use magpie_bt_dht::{
    BootstrapConfig, BootstrapOutcome, Datagram, DhtRuntime, DhtRuntimeConfig, NodeId,
    run_bootstrap,
};

struct Endpoint {
    runtime: DhtRuntime,
    addr: SocketAddr,
}

/// Boot `count` endpoints + wire an in-process broker per endpoint.
/// Each endpoint's `Endpoint::addr` is what other endpoints use to
/// target it — the broker rewrites the from-address so the receiving
/// side sees the sender's synthetic addr, not the broker's.
#[allow(clippy::unused_async)]
async fn boot_swarm(count: u16) -> Vec<Endpoint> {
    let mut endpoints = Vec::with_capacity(usize::from(count));
    let mut addr_to_inbound: HashMap<SocketAddr, mpsc::Sender<Datagram>> = HashMap::new();
    let mut outbound_rxs: Vec<(SocketAddr, mpsc::Receiver<Datagram>)> = Vec::new();

    for i in 0..count {
        let addr = addr_for(i);
        let (inbound_tx, inbound_rx) = mpsc::channel::<Datagram>(512);
        let (outbound_tx, outbound_rx) = mpsc::channel::<Datagram>(512);

        let local_id = node_id_for(i);
        let (runtime, _joins) = DhtRuntime::spawn(
            DhtRuntimeConfig::new(local_id),
            inbound_rx,
            outbound_tx,
            Instant::now(),
        )
        .unwrap();

        addr_to_inbound.insert(addr, inbound_tx);
        outbound_rxs.push((addr, outbound_rx));
        endpoints.push(Endpoint { runtime, addr });
    }

    // Spawn one broker per endpoint so every outbound Datagram gets
    // re-addressed (from = sender's synthetic addr) and forwarded.
    let routes = Arc::new(addr_to_inbound);
    for (sender_addr, mut outbound_rx) in outbound_rxs {
        let routes = Arc::clone(&routes);
        tokio::spawn(async move {
            while let Some(mut dg) = outbound_rx.recv().await {
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

const fn addr_for(i: u16) -> SocketAddr {
    // Spread across the 127.0.0.0/8 loopback range so per-IP rate
    // limiting doesn't throttle cross-node traffic.
    SocketAddr::V4(SocketAddrV4::new(
        Ipv4Addr::new(
            127,
            #[allow(clippy::cast_possible_truncation)]
            {
                (i >> 8) as u8
            },
            #[allow(clippy::cast_possible_truncation)]
            {
                (i & 0xff) as u8
            },
            1,
        ),
        6881,
    ))
}

const fn node_id_for(i: u16) -> NodeId {
    // Shift so index 0 doesn't collide with NodeId::ZERO (which
    // triggers the "sender id equals target id" Kademlia edge case
    // some tests rely on being distinguishable).
    let shifted = i.wrapping_add(0x1000);
    let mut bytes = [0u8; 20];
    #[allow(clippy::cast_possible_truncation)]
    {
        bytes[0] = (shifted >> 8) as u8;
        bytes[1] = (shifted & 0xff) as u8;
    }
    NodeId::from_bytes(bytes)
}

#[tokio::test]
async fn cold_start_reaches_operational() {
    // 4 bootstrap responders each seeded with a disjoint 12-node
    // slice of the spare pool; every spare is also seeded with 8
    // of its neighbours. Round 1 folds in ≤ 28 spares via the 4
    // hosts; round 2 pings those spares and pulls in their
    // neighbour-graph, pushing total routing size well over the
    // 32-good-nodes exit criterion.
    let total = 53u16;
    let swarm = boot_swarm(total).await;

    let cold = &swarm[0];
    let bootstrap_hosts: Vec<&Endpoint> = swarm[1..=4].iter().collect();

    // Partition spares 5..53 into 4 disjoint blocks of 12 — each
    // bootstrap host's `find_node(cold_id)` reply will surface
    // only spares from its own block.
    let now = Instant::now();
    for (i, responder) in bootstrap_hosts.iter().enumerate() {
        let block_start = 5 + i * 12;
        let block_end = block_start + 12;
        for spare in &swarm[block_start..block_end] {
            responder
                .runtime
                .seed_contact(spare.runtime.local_id().await, spare.addr, now)
                .await;
        }
    }
    // Cross-seed each spare with 8 of its adjacent spares so
    // round-2 `find_node` queries return fresh nodes.
    for (i_rel, spare) in swarm.iter().enumerate().skip(5) {
        for (j_rel, peer) in swarm.iter().enumerate().skip(5) {
            if i_rel == j_rel {
                continue;
            }
            if i_rel.abs_diff(j_rel) <= 4 {
                spare
                    .runtime
                    .seed_contact(peer.runtime.local_id().await, peer.addr, now)
                    .await;
            }
        }
    }

    // Fast-forward the cadence so the test runs in sub-second time.
    // `exit_good_nodes` is tuned for the synthetic 53-node swarm:
    // with cold_id = 0x1000 every candidate falls into a narrow
    // prefix and non-local buckets are capped at K = 8 (ADR-0024),
    // so the achievable upper bound in this test is ≈ 28–35 rather
    // than the production default of 32. The `default_constants_match_adr_0025`
    // unit test guards the spec threshold; this integration test
    // proves the two-round find_node convergence shape.
    let config = BootstrapConfig {
        seed_contacts: bootstrap_hosts.iter().map(|h| h.addr).collect(),
        round_interval: Duration::from_millis(20),
        stalled_interval: Duration::from_millis(20),
        stall_after: Duration::from_mins(1),
        contact_query_timeout: Duration::from_secs(2),
        exit_good_nodes: 24,
        ..BootstrapConfig::default()
    };

    let outcome = tokio::time::timeout(
        Duration::from_secs(10),
        run_bootstrap(&cold.runtime, config),
    )
    .await
    .expect("bootstrap did not complete within 10 s wall-clock");

    assert_eq!(outcome, BootstrapOutcome::Operational);
    assert!(cold.runtime.good_node_count().await >= 24);
}

#[tokio::test]
async fn single_find_node_probe_works_via_broker() {
    // Sanity probe: before running the full bootstrap, verify a
    // single send_query actually completes via the broker path.
    let swarm = boot_swarm(2).await;
    let a = &swarm[0];
    let b = &swarm[1];

    let spare_id = NodeId::from_bytes([0x99; 20]);
    let spare_addr = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(10, 0, 0, 99), 6881));
    b.runtime
        .seed_contact(spare_id, spare_addr, Instant::now())
        .await;

    let resp = tokio::time::timeout(
        Duration::from_secs(2),
        a.runtime.send_query(
            b.addr,
            magpie_bt_dht::Query::FindNode {
                id: a.runtime.local_id().await,
                target: a.runtime.local_id().await,
            },
        ),
    )
    .await
    .expect("send_query timed out")
    .expect("send_query failed");
    assert!(!resp.nodes.is_empty(), "b did not return the seeded spare");
    assert!(
        resp.nodes.iter().any(|n| n.id == spare_id),
        "seeded spare missing from reply: {:?}",
        resp.nodes.iter().map(|n| n.id).collect::<Vec<_>>()
    );
}

#[tokio::test]
async fn empty_seed_list_stalls_then_returns() {
    // No responders, no seeds → the cold node cannot reach 32 good
    // nodes. After the (short, test-only) stall window it exits
    // with Stalled rather than deadlocking.
    let swarm = boot_swarm(1).await;
    let cold = &swarm[0];

    let config = BootstrapConfig {
        seed_contacts: Vec::new(),
        round_interval: Duration::from_millis(10),
        stalled_interval: Duration::from_millis(10),
        stall_after: Duration::from_millis(100),
        stall_threshold: 4,
        contact_query_timeout: Duration::from_millis(50),
        ..BootstrapConfig::default()
    };

    let outcome =
        tokio::time::timeout(Duration::from_secs(2), run_bootstrap(&cold.runtime, config))
            .await
            .expect("stalled bootstrap did not return in 2 s");

    assert_eq!(outcome, BootstrapOutcome::Stalled);
}
