//! Single-file v1 seeder example.
//!
//! Counterpart to `leech.rs`. Opens an existing file that's a bit-exact
//! match for a given `.torrent`, runs a magpie [`Engine`] in seed-only
//! mode, accepts inbound peer connections, optionally announces to the
//! torrent's tracker so other peers can find us, and persists cumulative
//! upload/download counters across restarts via [`FileStatsSink`].
//!
//! This is the binary that subprocess-level M2 follow-ups target:
//! - `tests/stats_persist_subprocess.rs` (task #23) spawns this binary,
//!   observes non-zero counters, `SIGKILL`s, restarts, and asserts the
//!   sink restored the prior snapshot.
//! - `ci/soak/dhat.sh` invokes it with `--features dhat-heap` to capture
//!   a 24 h heap-allocation trace (`dhat-heap.json`) — the `--features`
//!   flag wires `dhat` in as the global allocator and dumps the trace on
//!   graceful exit.
//!
//! ## Usage
//!
//! ```text
//! cargo run --release --example seeder -- \
//!     --torrent /path/to/foo.torrent \
//!     --data    /path/to/foo.iso \
//!     --listen  0.0.0.0:6881 \
//!     --stats-dir /tmp/magpie-stats \
//!     [--announce]      # attach the torrent's tracker
//!     [--verify]        # SHA-1 every piece before serving (slow)
//! ```
//!
//! With heap profiling:
//!
//! ```text
//! cargo run --release --features dhat-heap --example seeder -- <args>
//! ```
//!
//! ## Scope
//!
//! **v1 only** (single-file or multi-file). Single-file torrents use
//! [`FileStorage`]; multi-file torrents use [`MultiFileStorage`] (ADR-0021).
//! Auto-detected from the path type: if `--data` is a file, single-file;
//! if it's a directory, multi-file (the torrent's file list is laid out
//! under that root).
//!
//! **Unix only.** Both storage backends rely on `pread`/`pwrite` which
//! the M2 storage implementations only support on Unix. The example is
//! `#[cfg(unix)]`-gated; Windows builds are a no-op `main` that prints
//! a message.
//!
//! ## Trust model
//!
//! By default the example starts with `initial_have = all`, trusting the
//! caller that `--data` is bit-exact for `--torrent`. If it isn't, remote
//! leechers will hash-fail their blocks and disconnect — no local
//! corruption — but the seeder log will show zero successful uploads.
//! Pass `--verify` to SHA-1 every piece at startup (slow; O(total_length)
//! I/O). **This flag is recommended for the first run on any new
//! fixture.**
//!
//! ## Stats persistence (task #23)
//!
//! The `FileStatsSink` writes bencode sidecars under `--stats-dir` at a
//! 30 s cadence, plus a 5 s bounded graceful-shutdown flush on `SIGINT`.
//! A `SIGKILL` mid-flush leaves a partial `.stats.tmp` next to the atomic
//! `.stats`; `FileStatsSink::load_sidecar` on restart still sees the
//! prior committed snapshot (atomic write-tmp+rename).

#![allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_lossless,
    clippy::cast_sign_loss,
    clippy::too_many_lines,
    clippy::uninlined_format_args,
    clippy::unchecked_time_subtraction,
    clippy::significant_drop_tightening,
    clippy::redundant_closure_for_method_calls,
    clippy::map_unwrap_or,
    clippy::similar_names,
    clippy::doc_markdown,
    clippy::format_push_string,
    clippy::items_after_statements,
    clippy::collapsible_if,
    clippy::doc_lazy_continuation,
    unused_qualifications,
    unreachable_pub
)]

#[cfg(feature = "dhat-heap")]
#[global_allocator]
static ALLOC: dhat::Alloc = dhat::Alloc;

#[cfg(not(unix))]
fn main() {
    eprintln!("seeder example: Unix-only (FileStorage requires pread/pwrite — see ADR-0008)");
    std::process::exit(2);
}

#[cfg(unix)]
#[tokio::main(flavor = "multi_thread")]
async fn main() -> std::process::ExitCode {
    run::run().await
}

#[cfg(unix)]
mod run {
    use std::env;
    use std::path::PathBuf;
    use std::process::ExitCode;
    use std::sync::Arc;
    use std::time::Duration;

    use magpie_bt::alerts::{AlertCategory, AlertQueue};
    use magpie_bt::engine::AttachTrackerConfig;
    use magpie_bt::peer_filter::DefaultPeerFilter;
    use magpie_bt::storage::{FdPool, FileStorage, MultiFileStorage, Storage};
    use magpie_bt::tracker::HttpTracker;
    use magpie_bt::{
        AddTorrentRequest, Engine, FileStatsSink, InfoHash, ListenConfig, PeerIdBuilder,
        TorrentParams,
    };
    use magpie_bt_metainfo::{FileListV1, parse};

    #[derive(Debug)]
    #[allow(clippy::struct_excessive_bools)]
    struct Args {
        torrent: PathBuf,
        data: PathBuf,
        listen: String,
        stats_dir: Option<PathBuf>,
        announce: bool,
        verify: bool,
        /// Dev flag: swap `DefaultPeerFilter::default()` (rejects loopback
        /// + link-local, allows RFC1918 + global unicast) for
        /// `permissive_for_tests()` (accepts everything). Required to
        /// accept a leech from 127.0.0.1 — don't enable in production.
        allow_loopback: bool,
        /// Seconds between stats-sink flushes. Default 30s matches the
        /// sink's batch cadence. Lower values are useful for subprocess
        /// tests that need to see a committed sidecar quickly.
        flush_secs: u64,
        /// BEP 9 / BEP 10: also advertise `metadata_size` in our
        /// extension handshake and serve `ut_metadata` Data responses.
        /// Required for the magnet-flavoured interop scenarios where
        /// the leecher bootstraps from a magnet URI and pulls metadata
        /// from us.
        advertise_metadata: bool,
    }

    fn parse_args() -> Result<Args, String> {
        let mut torrent: Option<PathBuf> = None;
        let mut data: Option<PathBuf> = None;
        let mut listen = "0.0.0.0:6881".to_string();
        let mut stats_dir: Option<PathBuf> = None;
        let mut announce = false;
        let mut verify = false;
        let mut allow_loopback = false;
        let mut flush_secs: u64 = 30;
        let mut advertise_metadata = false;
        let mut iter = env::args().skip(1);
        while let Some(arg) = iter.next() {
            match arg.as_str() {
                "--torrent" => torrent = iter.next().map(PathBuf::from),
                "--data" => data = iter.next().map(PathBuf::from),
                "--listen" => {
                    listen = iter
                        .next()
                        .ok_or_else(|| "--listen needs a value".to_string())?;
                }
                "--stats-dir" => stats_dir = iter.next().map(PathBuf::from),
                "--announce" => announce = true,
                "--verify" => verify = true,
                "--allow-loopback" => allow_loopback = true,
                "--advertise-metadata" => advertise_metadata = true,
                "--flush-secs" => {
                    flush_secs = iter
                        .next()
                        .ok_or_else(|| "--flush-secs needs a value".to_string())?
                        .parse()
                        .map_err(|e: std::num::ParseIntError| format!("--flush-secs: {e}"))?;
                }
                "--help" | "-h" => return Err(usage()),
                other => return Err(format!("unknown arg: {other}\n{}", usage())),
            }
        }
        Ok(Args {
            torrent: torrent.ok_or_else(|| format!("--torrent required\n{}", usage()))?,
            data: data.ok_or_else(|| format!("--data required\n{}", usage()))?,
            listen,
            stats_dir,
            announce,
            verify,
            allow_loopback,
            flush_secs,
            advertise_metadata,
        })
    }

    fn usage() -> String {
        "usage: seeder --torrent <FILE.torrent> --data <FILE> \
         [--listen <ADDR>] [--stats-dir <DIR>] [--announce] [--verify] \
         [--allow-loopback] [--flush-secs <N>] [--advertise-metadata]"
            .into()
    }

    fn hex(bytes: &[u8]) -> String {
        let mut s = String::with_capacity(bytes.len() * 2);
        for b in bytes {
            s.push_str(&format!("{b:02x}"));
        }
        s
    }

    pub async fn run() -> ExitCode {
        #[cfg(feature = "dhat-heap")]
        let _dhat_profiler = dhat::Profiler::new_heap();

        let _ = tracing_subscriber::fmt()
            .with_env_filter(
                tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| {
                    tracing_subscriber::EnvFilter::new("magpie_bt_core=info,seeder=info")
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

        match drive(args).await {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("error: {e}");
                ExitCode::FAILURE
            }
        }
    }

    async fn drive(args: Args) -> Result<(), String> {
        // --- metainfo ---------------------------------------------------
        let torrent_bytes =
            std::fs::read(&args.torrent).map_err(|e| format!("read .torrent: {e}"))?;
        let meta = parse(&torrent_bytes).map_err(|e| format!("parse .torrent: {e}"))?;
        let info_hash = match meta.info_hash {
            InfoHash::V1(h) | InfoHash::Hybrid { v1: h, .. } => h,
            InfoHash::V2(_) => {
                return Err("v2-only torrents not supported here (BEP 52 is M4)".into());
            }
        };
        let v1 = meta.info.v1.as_ref().ok_or("torrent has no v1 info dict")?;
        let total_length = match &v1.files {
            FileListV1::Single { length } => *length,
            FileListV1::Multi { files } => files.iter().map(|f| f.length).sum(),
        };
        let is_multi = matches!(v1.files, FileListV1::Multi { .. });
        let piece_length = meta.info.piece_length;
        let piece_count =
            u32::try_from(v1.pieces.len() / 20).map_err(|_| "piece count overflow")?;

        // --- sanity: data length matches ---------------------------------
        // Single-file: `--data` points at the file, lengths must match.
        // Multi-file: `--data` points at a directory; per-file size check
        // is delegated to MultiFileStorage::open_from_info.
        let data_meta = std::fs::metadata(&args.data).map_err(|e| format!("stat --data: {e}"))?;
        if is_multi {
            if !data_meta.is_dir() {
                return Err(format!(
                    "--data {} is not a directory (multi-file torrent expects a dir)",
                    args.data.display()
                ));
            }
        } else {
            if data_meta.is_dir() {
                return Err(format!(
                    "--data {} is a directory (single-file torrent expects a file)",
                    args.data.display()
                ));
            }
            if data_meta.len() != total_length {
                return Err(format!(
                    "--data length {} does not match torrent total {} — refusing to serve",
                    data_meta.len(),
                    total_length
                ));
            }
        }

        eprintln!(
            "seeder: {name}\n  info_hash: {hash}\n  piece_count: {pc}, piece_length: {pl}, \
             total: {total} bytes ({mib:.2} MiB)\n  data: {data}\n  listen: {listen}\n  \
             announce: {ann}  verify: {ver}",
            name = String::from_utf8_lossy(meta.info.name),
            hash = hex(&info_hash),
            pc = piece_count,
            pl = piece_length,
            total = total_length,
            mib = total_length as f64 / (1024.0 * 1024.0),
            data = args.data.display(),
            listen = args.listen,
            ann = args.announce,
            ver = args.verify,
        );

        // --- storage + (optional) verify --------------------------------
        let storage: Arc<dyn Storage> = if is_multi {
            let pool = Arc::new(FdPool::with_default_cap());
            Arc::new(
                MultiFileStorage::open_from_info(&args.data, &meta.info, pool)
                    .map_err(|e| format!("open multi-file --data: {e}"))?,
            )
        } else {
            Arc::new(FileStorage::open(&args.data).map_err(|e| format!("open --data: {e}"))?)
        };
        if args.verify {
            use magpie_bt_metainfo::sha1;
            eprintln!("verifying {piece_count} pieces...");
            let mut buf = vec![0u8; piece_length as usize];
            for i in 0..piece_count {
                let offset = u64::from(i) * piece_length;
                let remaining = total_length - offset;
                let this_len = remaining.min(piece_length) as usize;
                let slice = &mut buf[..this_len];
                storage
                    .read_block(offset, slice)
                    .map_err(|e| format!("read piece {i}: {e}"))?;
                let expected = &v1.pieces[(i as usize) * 20..(i as usize + 1) * 20];
                let got = sha1(slice);
                if got.as_slice() != expected {
                    return Err(format!(
                        "piece {i} hash mismatch — --data is not bit-exact for --torrent"
                    ));
                }
            }
            eprintln!("verify: all {piece_count} pieces match");
        }

        // --- engine + torrent ------------------------------------------
        let alerts = Arc::new(AlertQueue::new(2048));
        alerts.set_mask(AlertCategory(u32::MAX));
        let engine = Arc::new(Engine::new(Arc::clone(&alerts)));

        let peer_id = PeerIdBuilder::magpie(*b"0001").build();
        let storage_dyn: Arc<dyn magpie_bt::storage::Storage> = storage.clone();
        let mut req = AddTorrentRequest::new(
            info_hash,
            TorrentParams {
                piece_count,
                piece_length,
                total_length,
                piece_hashes: v1.pieces.to_vec(),
                private: meta.info.private,
            },
            storage_dyn,
            peer_id,
        );
        let filter: Arc<dyn magpie_bt::peer_filter::PeerFilter> = if args.allow_loopback {
            Arc::new(DefaultPeerFilter::permissive_for_tests())
        } else {
            Arc::new(DefaultPeerFilter::default())
        };
        req.peer_filter = Arc::clone(&filter);
        req.initial_have = vec![true; piece_count as usize];
        if args.advertise_metadata {
            // BEP 9: hand the raw info-dict bytes to the session so we can
            // serve ut_metadata Data responses and advertise metadata_size
            // in our extension handshake. Magnet leechers bootstrap from
            // this without ever needing the .torrent file.
            req.info_dict_bytes = Some(meta.info_bytes.to_vec());
        }
        let torrent_id = engine
            .add_torrent(req)
            .await
            .map_err(|e| format!("add_torrent: {e}"))?;

        // --- inbound listener ------------------------------------------
        let listen_cfg = ListenConfig {
            peer_filter: Arc::clone(&filter),
            ..ListenConfig::default()
        };
        let bound = engine
            .listen(
                args.listen
                    .parse()
                    .map_err(|e| format!("parse --listen: {e}"))?,
                listen_cfg,
            )
            .await
            .map_err(|e| format!("listen: {e}"))?;
        eprintln!("seeder: listening on {bound}");

        // --- optional tracker attach -----------------------------------
        if args.announce {
            if let Some(ann_bytes) = meta.announce {
                let ann = std::str::from_utf8(ann_bytes)
                    .map_err(|e| format!("announce URL not UTF-8: {e}"))?;
                let http_tracker =
                    Arc::new(HttpTracker::new(ann).map_err(|e| format!("HttpTracker: {e}"))?);
                engine
                    .attach_tracker(torrent_id, http_tracker, AttachTrackerConfig::default())
                    .await
                    .map_err(|e| format!("attach_tracker: {e}"))?;
                eprintln!("seeder: attached tracker {ann}");
            } else {
                eprintln!("seeder: --announce requested but .torrent has no announce URL");
            }
        }

        // --- stats sink ------------------------------------------------
        let stats_dir = args.stats_dir.unwrap_or_else(|| {
            let mut p = args.data.clone();
            p.set_extension("stats.d");
            p
        });
        let sink =
            Arc::new(FileStatsSink::new(&stats_dir).map_err(|e| format!("FileStatsSink: {e}"))?);
        if let Some(prior) = sink
            .load_sidecar(&info_hash)
            .map_err(|e| format!("load_sidecar: {e}"))?
        {
            eprintln!(
                "stats: restored from {} — uploaded {} down {}",
                stats_dir.display(),
                prior.uploaded,
                prior.downloaded
            );
        } else {
            eprintln!(
                "stats: no prior sidecar at {} (cold start)",
                stats_dir.display()
            );
        }
        // Periodic: snapshot per-torrent stats from the engine → enqueue
        // on the sink → flush to disk. Cadence is `--flush-secs` (default
        // 30s, matching FileStatsSink's documented batch cadence). Lower
        // values are useful for the #24 subprocess-SIGKILL test.
        let sink_flush = Arc::clone(&sink);
        let engine_for_stats = Arc::clone(&engine);
        let flush_interval = Duration::from_secs(args.flush_secs.max(1));
        let flush_task = tokio::spawn(async move {
            let mut ticker = tokio::time::interval(flush_interval);
            loop {
                ticker.tick().await;
                if let Some(snap) = engine_for_stats.torrent_stats_snapshot(torrent_id).await {
                    if let Err(e) = sink_flush.enqueue(snap) {
                        eprintln!("warn: sink enqueue failed: {e}");
                    }
                }
                if let Err(e) = sink_flush.flush_now() {
                    eprintln!("warn: periodic flush failed: {e}");
                }
            }
        });

        // --- main loop: print stats to stdout every 1s, until ctrl-c --
        eprintln!("seeder: ready. ctrl-c to exit.");
        let shutdown = tokio::signal::ctrl_c();
        tokio::pin!(shutdown);
        let mut ticker = tokio::time::interval(Duration::from_secs(1));
        let disk_metrics = engine
            .disk_metrics(torrent_id)
            .await
            .ok_or("disk_metrics missing")?;
        use std::sync::atomic::Ordering;
        loop {
            tokio::select! {
                _ = &mut shutdown => break,
                _ = ticker.tick() => {
                    let bw = disk_metrics.bytes_written.load(Ordering::Relaxed);
                    let pw = disk_metrics.pieces_written.load(Ordering::Relaxed);
                    let (up, down) = engine
                        .torrent_stats_snapshot(torrent_id)
                        .await
                        .map_or((0, 0), |s| (s.uploaded, s.downloaded));
                    println!("t={}s uploaded={} downloaded={} bytes_written={} pieces_written={}",
                        now_secs(), up, down, bw, pw);
                }
            }
        }

        // --- graceful shutdown -----------------------------------------
        eprintln!("seeder: shutting down");
        // Take one last snapshot + enqueue so flush_graceful has the
        // freshest counters to write before exit.
        if let Some(snap) = engine.torrent_stats_snapshot(torrent_id).await {
            let _ = sink.enqueue(snap);
        }
        flush_task.abort();
        let _ = flush_task.await;
        use magpie_bt::StatsSink as _;
        if let Err(e) = sink.flush_graceful(Duration::from_secs(5)) {
            eprintln!("warn: flush_graceful failed: {e}");
        }
        engine.shutdown(torrent_id).await;
        let _ = tokio::time::timeout(Duration::from_secs(5), engine.join()).await;
        Ok(())
    }

    fn now_secs() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs())
    }
}
