//! Real-network leecher example.
//!
//! Downloads a single-file v1 .torrent end-to-end against its real public
//! tracker. Useful as the "live" smoke test for the M1 leecher gate
//! (`docs/milestones/001-leecher-tcp-v1.md` §"Gate criteria" #2).
//!
//! Usage:
//!
//! ```text
//! cargo run --example leech --release -- \
//!     --torrent /path/to/ubuntu-24.04.1-desktop-amd64.iso.torrent \
//!     --out /tmp/ubuntu.iso
//! ```
//!
//! The example:
//! - parses the .torrent (single-file v1 only — multi-file is M2),
//! - allocates a [`FileStorage`] sized to the torrent's total length,
//! - opens an [`Engine`] with [`DefaultPeerFilter::default`] (rejects
//!   loopback, allows RFC 1918 + global unicast),
//! - constructs an [`HttpTracker`] for the announce URL and attaches it via
//!   [`Engine::attach_tracker`],
//! - prints progress every second using the alert ring + disk metrics,
//! - exits when every piece is verified or when SIGINT arrives.
//!
//! Multi-tracker (BEP 12) and magnet links land in M2/M3.
#![allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_lossless,
    clippy::cast_sign_loss,
    clippy::needless_pass_by_value,
    clippy::print_stdout,
    clippy::print_stderr,
    clippy::too_many_lines,
    clippy::manual_let_else,
    clippy::missing_docs_in_private_items
)]

use std::env;
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use magpie_bt::alerts::{Alert, AlertCategory, AlertQueue};
use magpie_bt::engine::AttachTrackerConfig;
use magpie_bt::peer_filter::DefaultPeerFilter;
use magpie_bt::session::TorrentParams;
use magpie_bt::tracker::{HttpTracker, Tracker};
use magpie_bt::{AddTorrentRequest, Engine, FileStorage, InfoHash, MetaInfo, PeerIdBuilder, parse};
use magpie_bt_metainfo::FileListV1;

#[derive(Debug, Default)]
struct Args {
    torrent: Option<String>,
    out: Option<String>,
    listen_port: u16,
}

fn parse_args() -> Result<Args, String> {
    let mut a = Args {
        listen_port: 6881,
        ..Default::default()
    };
    let mut iter = env::args().skip(1);
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--torrent" => a.torrent = iter.next(),
            "--out" => a.out = iter.next(),
            "--port" => {
                a.listen_port = iter
                    .next()
                    .ok_or_else(|| "--port needs a value".to_string())?
                    .parse()
                    .map_err(|e: std::num::ParseIntError| format!("--port: {e}"))?;
            }
            "--help" | "-h" => return Err(usage()),
            other => return Err(format!("unknown arg: {other}\n{}", usage())),
        }
    }
    if a.torrent.is_none() || a.out.is_none() {
        return Err(usage());
    }
    Ok(a)
}

fn usage() -> String {
    "usage: leech --torrent <FILE.torrent> --out <FILE.iso> [--port <NNNN>]".into()
}

#[tokio::main(flavor = "multi_thread")]
async fn main() -> ExitCode {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| {
                tracing_subscriber::EnvFilter::new("magpie_bt_core=info,leech=info")
            }),
        )
        .with_target(true)
        .try_init();

    let args = match parse_args() {
        Ok(a) => a,
        Err(msg) => {
            eprintln!("{msg}");
            return ExitCode::from(2);
        }
    };

    if let Err(e) = run(args).await {
        eprintln!("error: {e}");
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}

async fn run(args: Args) -> Result<(), String> {
    let torrent_bytes = std::fs::read(args.torrent.as_deref().unwrap())
        .map_err(|e| format!("read .torrent: {e}"))?;

    let meta = parse(&torrent_bytes).map_err(|e| format!("parse .torrent: {e}"))?;

    let info_hash = match meta.info_hash {
        InfoHash::V1(h) | InfoHash::Hybrid { v1: h, .. } => h,
        InfoHash::V2(_) => {
            return Err("v2-only torrents not supported in M1 (BEP 52 leech lands in M4)".into());
        }
    };
    let v1 = meta.info.v1.as_ref().ok_or("torrent has no v1 info dict")?;
    let total_length = match &v1.files {
        FileListV1::Single { length } => *length,
        FileListV1::Multi { .. } => {
            return Err(
                "multi-file torrents not yet supported by the example (single-file ISO works); land in M2 with layout-aware storage".into(),
            );
        }
    };
    let piece_length = meta.info.piece_length;
    let piece_count = u32::try_from(v1.pieces.len() / 20).map_err(|_| "piece count overflow")?;

    let params = TorrentParams {
        piece_count,
        piece_length,
        total_length,
        piece_hashes: v1.pieces.to_vec(),
        private: meta.info.private,
    };

    println!(
        "torrent: {name}\n  info_hash: {hash}\n  piece_count: {pc}, piece_length: {pl}, total: {total} bytes ({mib:.2} MiB)",
        name = String::from_utf8_lossy(meta.info.name),
        hash = hex(&info_hash),
        pc = piece_count,
        pl = piece_length,
        total = total_length,
        mib = total_length as f64 / (1024.0 * 1024.0),
    );

    // Storage: pre-allocates a sparse file of `total_length` bytes.
    let storage = Arc::new(
        FileStorage::create(args.out.as_deref().unwrap(), total_length)
            .map_err(|e| format!("create output file: {e}"))?,
    );

    // Engine + alerts.
    let alerts = Arc::new(AlertQueue::new(2048));
    alerts.set_mask(AlertCategory(u32::MAX));
    let engine = Arc::new(Engine::new(Arc::clone(&alerts)));

    let peer_id = PeerIdBuilder::magpie(*b"0001").build();

    let mut req = AddTorrentRequest::new(info_hash, params, storage.clone(), peer_id);
    req.peer_filter = Arc::new(DefaultPeerFilter::default());

    let id = engine
        .add_torrent(req)
        .await
        .map_err(|e| format!("add_torrent: {e}"))?;

    // Tracker.
    let announce = pick_announce(&meta).ok_or("no HTTP/HTTPS tracker URL in .torrent")?;
    println!("tracker: {announce}");
    let tracker: Arc<dyn Tracker> =
        Arc::new(HttpTracker::new(announce.clone()).map_err(|e| format!("HttpTracker::new: {e}"))?);
    let cfg = AttachTrackerConfig {
        listen_port: args.listen_port,
        ..Default::default()
    };
    engine
        .attach_tracker(id, tracker, cfg)
        .await
        .map_err(|e| format!("attach_tracker: {e}"))?;

    // Progress loop.
    let metrics = engine
        .disk_metrics(id)
        .await
        .ok_or("missing disk_metrics for torrent")?;
    let start = Instant::now();
    let mut completed = 0u32;
    let mut peers_connected = 0i64;
    let mut last_log = Instant::now();
    let interrupt = tokio::signal::ctrl_c();
    tokio::pin!(interrupt);

    loop {
        tokio::select! {
            _ = &mut interrupt => {
                println!("\nctrl-c received, shutting down...");
                engine.shutdown(id).await;
                let _ = tokio::time::timeout(Duration::from_secs(3), engine.join()).await;
                return Err("interrupted".into());
            }
            () = tokio::time::sleep(Duration::from_millis(500)) => {}
        }

        for a in alerts.drain() {
            match a {
                Alert::PieceCompleted { .. } => completed += 1,
                Alert::PeerConnected { .. } => peers_connected += 1,
                Alert::PeerDisconnected { .. } => peers_connected -= 1,
                _ => {}
            }
        }

        if completed == piece_count {
            let elapsed = start.elapsed();
            let bytes = metrics.bytes_written.load(Ordering::Relaxed);
            println!(
                "\nDONE — {bytes} bytes ({mib:.2} MiB) in {elapsed:.1?} ({mbps:.2} MiB/s)",
                bytes = bytes,
                mib = bytes as f64 / (1024.0 * 1024.0),
                mbps = bytes as f64 / (1024.0 * 1024.0) / elapsed.as_secs_f64(),
            );
            engine.shutdown(id).await;
            let _ = tokio::time::timeout(Duration::from_secs(3), engine.join()).await;
            return Ok(());
        }

        if last_log.elapsed() >= Duration::from_secs(1) {
            last_log = Instant::now();
            let bytes = metrics.bytes_written.load(Ordering::Relaxed);
            let pct = (f64::from(completed) / f64::from(piece_count)) * 100.0;
            println!(
                "  {completed}/{piece_count} pieces ({pct:>5.1}%)  {mib:>8.2} MiB written  peers={peers_connected:>3}  failed_pieces={fp}",
                mib = bytes as f64 / (1024.0 * 1024.0),
                fp = metrics.piece_verify_fail.load(Ordering::Relaxed),
            );
        }
    }
}

fn pick_announce(meta: &MetaInfo<'_>) -> Option<String> {
    // Prefer the flat `announce` field; fall back to the first tier of
    // `announce-list` (multi-tracker proper iteration is M2/BEP 12).
    if let Some(url) = meta.announce
        && let Ok(s) = std::str::from_utf8(url)
        && (s.starts_with("http://") || s.starts_with("https://"))
    {
        return Some(s.to_string());
    }
    if let Some(tiers) = &meta.announce_list {
        for tier in tiers {
            for url in tier {
                if let Ok(s) = std::str::from_utf8(url)
                    && (s.starts_with("http://") || s.starts_with("https://"))
                {
                    return Some(s.to_string());
                }
            }
        }
    }
    None
}

fn hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        use std::fmt::Write;
        write!(s, "{b:02x}").unwrap();
    }
    s
}
