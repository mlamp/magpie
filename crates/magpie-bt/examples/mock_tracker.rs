//! Tiny HTTP BEP 3 / BEP 23 tracker for interop docker scenarios.
//!
//! Accepts `GET /announce?...` and returns a compact-peer-list bencode
//! response. Peer list is read from env var `MOCK_TRACKER_PEERS` as a
//! comma-separated list of `IP:PORT` strings (IPv4 only). Interval defaults
//! to 60s; override with `MOCK_TRACKER_INTERVAL`.
//!
//! Listens on `0.0.0.0:MOCK_TRACKER_PORT` (default 6969).
//!
//! This is scaffolding for `ci/interop/`. Not a production tracker —
//! doesn't validate requests, doesn't track swarms, doesn't handle
//! scrape. Just replies with a fixed peer list so magpie seeder and a
//! third-party leecher (qBittorrent / Transmission) can find each
//! other without an external tracker service.

#![allow(clippy::missing_docs_in_private_items, unreachable_pub)]

use std::env;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::time::Duration;

fn main() {
    let port: u16 = env::var("MOCK_TRACKER_PORT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(6969);
    let interval: u64 = env::var("MOCK_TRACKER_INTERVAL")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(60);
    let peers_csv = env::var("MOCK_TRACKER_PEERS").unwrap_or_default();
    let peers: Vec<SocketAddr> = peers_csv
        .split(',')
        .filter_map(|s| s.trim().parse().ok())
        .collect();

    let compact = build_compact_peer_list(&peers);
    let body = build_announce_body(interval, &compact, peers.len());
    eprintln!(
        "mock_tracker: listening on 0.0.0.0:{port}; {} peers; interval={}s",
        peers.len(),
        interval
    );

    let listener = TcpListener::bind(("0.0.0.0", port)).expect("bind");
    for stream in listener.incoming() {
        match stream {
            Ok(mut s) => {
                let _ = s.set_read_timeout(Some(Duration::from_secs(5)));
                let _ = s.set_write_timeout(Some(Duration::from_secs(5)));
                let _ = handle(&mut s, &body);
            }
            Err(e) => eprintln!("accept error: {e}"),
        }
    }
}

fn handle(stream: &mut TcpStream, body: &[u8]) -> std::io::Result<()> {
    let mut buf = [0u8; 4096];
    let _ = stream.read(&mut buf)?;
    // Don't parse the request — any GET gets the same bencode response.
    let mut resp = Vec::new();
    resp.extend_from_slice(b"HTTP/1.1 200 OK\r\n");
    resp.extend_from_slice(b"Content-Type: text/plain\r\n");
    resp.extend_from_slice(format!("Content-Length: {}\r\n", body.len()).as_bytes());
    resp.extend_from_slice(b"Connection: close\r\n\r\n");
    resp.extend_from_slice(body);
    stream.write_all(&resp)?;
    stream.flush()?;
    Ok(())
}

fn build_compact_peer_list(peers: &[SocketAddr]) -> Vec<u8> {
    let mut out = Vec::with_capacity(peers.len() * 6);
    for p in peers {
        if let SocketAddr::V4(v4) = p {
            out.extend_from_slice(&v4.ip().octets());
            out.extend_from_slice(&v4.port().to_be_bytes());
        }
    }
    out
}

fn build_announce_body(interval: u64, compact_peers: &[u8], peer_count: usize) -> Vec<u8> {
    // d 8:complete i0e 10:incomplete iN e 8:interval iT e 5:peers <N>:<bytes> e
    let mut out = Vec::new();
    out.push(b'd');
    out.extend_from_slice(b"8:completei0e");
    out.extend_from_slice(format!("10:incompletei{peer_count}e").as_bytes());
    out.extend_from_slice(format!("8:intervali{interval}e").as_bytes());
    out.extend_from_slice(format!("5:peers{}:", compact_peers.len()).as_bytes());
    out.extend_from_slice(compact_peers);
    out.push(b'e');
    out
}
