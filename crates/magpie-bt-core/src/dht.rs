//! DHT integration glue (feature-gated).
//!
//! Mirrors [`Engine::attach_tracker`](crate::engine::Engine::attach_tracker):
//! spawns a periodic loop that calls
//! [`magpie_bt_dht::DhtRuntime::announce`] for the torrent's info-hash
//! and feeds discovered peers into [`Engine::add_peer`].
//!
//! Only compiled under the `dht` feature per ADR-0001 — consumers who
//! don't need DHT pay zero build cost for it. The DHT transport
//! (`UdpDemux` wiring + `DhtRuntime::spawn`) is the consumer's
//! responsibility; this module only drives the announce pipeline on
//! an already-running runtime.

use std::sync::Arc;
use std::time::{Duration, Instant};

use magpie_bt_dht::{Datagram, DhtRuntime, DhtRuntimeConfig, DhtRuntimeJoins, InfoHash};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::alerts::{Alert, AlertErrorCode};
use crate::engine::{AddPeerError, Engine};
use crate::ids::TorrentId;
use crate::session::udp::{UdpDemux, UdpPacket};

// ---------------------------------------------------------------------------
// Config
// ---------------------------------------------------------------------------

/// Configuration for [`Engine::attach_dht`].
#[derive(Debug, Clone, Copy)]
pub struct AttachDhtConfig {
    /// Port advertised in the `announce_peer` KRPC. Mirrors the
    /// `listen_port` semantics from [`crate::engine::AttachTrackerConfig`].
    pub listen_port: u16,
    /// BEP 27 private-torrent flag. When `true`, every announce round
    /// is a no-op (no KRPC emitted, empty peer list). Caller derives
    /// this from the torrent's info dict (`MetaInfo::info.private`).
    pub private: bool,
    /// Cadence between announce rounds. Defaults to the Mainline DHT
    /// re-announce interval of 30 minutes.
    pub announce_interval: Duration,
    /// Backoff after a `DhtRuntime::announce` error (e.g. outbound
    /// channel closed) before retrying.
    pub error_backoff: Duration,
}

impl Default for AttachDhtConfig {
    fn default() -> Self {
        Self {
            listen_port: 6881,
            private: false,
            announce_interval: Duration::from_secs(30 * 60),
            error_backoff: Duration::from_secs(60),
        }
    }
}

// ---------------------------------------------------------------------------
// UdpDemux ↔ DhtRuntime adapter
// ---------------------------------------------------------------------------

/// Channel capacity for the `UdpDemux → DhtRuntime` inbound pump.
///
/// Sized for "generous enough for any realistic burst, small enough
/// to surface stall bugs". Consumers who see datagram drops at the
/// DHT level can lift this and set their own
/// `DhtRuntimeConfig::outbound_capacity`.
pub const DEFAULT_DHT_CHANNEL_CAPACITY: usize = 2048;

/// Handles returned by [`spawn_dht_on_demux`]. Drop to detach; await
/// individual fields to run to completion when the socket closes.
#[derive(Debug)]
pub struct DhtOnDemux {
    /// The DHT handle — plug into [`Engine::attach_dht`].
    pub runtime: DhtRuntime,
    /// DHT pump + handler joins.
    pub joins: DhtRuntimeJoins,
    /// Outbound send loop (drains the DHT's outbound mpsc onto the
    /// `UdpDemux` socket).
    pub send_loop: JoinHandle<()>,
}

/// Wire a [`DhtRuntime`] onto an already-running [`UdpDemux`].
///
/// Registers the DHT first-byte-`b'd'` subscriber on the demux,
/// drains the DHT's outbound channel into `demux.send_to`, and
/// spawns the runtime with `config`. The returned [`DhtRuntime`]
/// is ready for [`Engine::attach_dht`].
///
/// # Errors
///
/// Surfaces [`std::io::Error`] from `UdpDemux::register_dht` (wrapped
/// as `io::ErrorKind::AlreadyExists` when the DHT slot is taken) or
/// [`getrandom::Error`] from the runtime's token-secret init.
#[allow(clippy::unused_async)] // spawns tokio tasks; must run inside a runtime
pub async fn spawn_dht_on_demux(
    demux: Arc<UdpDemux>,
    config: DhtRuntimeConfig,
    now: Instant,
) -> Result<DhtOnDemux, SpawnDhtError> {
    let (inbound_tx, inbound_rx) = mpsc::channel::<Datagram>(DEFAULT_DHT_CHANNEL_CAPACITY);
    let (outbound_tx, mut outbound_rx) = mpsc::channel::<Datagram>(DEFAULT_DHT_CHANNEL_CAPACITY);

    // UdpPacket → Datagram shim: the demux delivers its own type,
    // the DHT consumes its own. One pump task per DHT instance.
    let (shim_tx, mut shim_rx) = mpsc::channel::<UdpPacket>(DEFAULT_DHT_CHANNEL_CAPACITY);
    demux
        .register_dht(shim_tx)
        .map_err(|_| SpawnDhtError::AlreadyRegistered)?;
    tokio::spawn(async move {
        while let Some(pkt) = shim_rx.recv().await {
            if inbound_tx
                .send(Datagram {
                    data: pkt.data,
                    addr: pkt.from,
                })
                .await
                .is_err()
            {
                break;
            }
        }
    });

    // Spawn the DHT runtime.
    let (runtime, joins) = DhtRuntime::spawn(config, inbound_rx, outbound_tx, now)?;

    // Outbound send loop: drain the DHT's outbound channel onto the
    // shared socket via the demux.
    let send_demux = Arc::clone(&demux);
    let send_loop = tokio::spawn(async move {
        while let Some(dg) = outbound_rx.recv().await {
            if let Err(e) = send_demux.send_to(&dg.data, dg.addr).await {
                tracing::debug!(target = %dg.addr, error = %e, "dht outbound send failed");
            }
        }
    });

    Ok(DhtOnDemux {
        runtime,
        joins,
        send_loop,
    })
}

/// Failures for [`spawn_dht_on_demux`].
#[derive(Debug, thiserror::Error)]
pub enum SpawnDhtError {
    /// A DHT subscriber is already registered on the demux.
    #[error("DHT subscriber already registered on this demux")]
    AlreadyRegistered,
    /// The CSPRNG required by the token-secret factory failed.
    #[error("token-secret RNG failure: {0}")]
    Rng(getrandom::Error),
}

impl From<getrandom::Error> for SpawnDhtError {
    fn from(err: getrandom::Error) -> Self {
        Self::Rng(err)
    }
}

// ---------------------------------------------------------------------------
// Engine::attach_dht
// ---------------------------------------------------------------------------

impl Engine {
    /// Spawn a periodic DHT announce loop for `torrent_id`.
    ///
    /// On every round, the loop calls
    /// [`DhtRuntime::announce`](magpie_bt_dht::DhtRuntime::announce)
    /// with the torrent's info-hash and the configured
    /// [`AttachDhtConfig::private`] flag. Peers returned by the DHT
    /// are fed into [`Engine::add_peer`] (after the torrent's
    /// `PeerFilter`, same as the tracker path).
    ///
    /// When `cfg.private = true`, each round is still scheduled but
    /// emits zero KRPC traffic — `DhtRuntime::announce` returns an
    /// empty peer list. Inexpensive, keeps the single attach-point
    /// story simple.
    ///
    /// # Errors
    ///
    /// [`AddPeerError::UnknownTorrent`] if `torrent_id` isn't
    /// registered.
    pub async fn attach_dht(
        self: &Arc<Self>,
        torrent_id: TorrentId,
        dht: DhtRuntime,
        cfg: AttachDhtConfig,
    ) -> Result<(), AddPeerError> {
        let info_hash_bytes = self
            .torrent_state(torrent_id)
            .await
            .ok_or(AddPeerError::UnknownTorrent(torrent_id))?
            .info_hash;
        let info_hash = InfoHash::from_bytes(info_hash_bytes);

        let engine = Arc::clone(self);
        let alerts = Arc::clone(&engine.alerts);
        let task = tokio::spawn(async move {
            loop {
                match dht.announce(info_hash, cfg.listen_port, cfg.private).await {
                    Ok(peers) => {
                        tracing::info!(?torrent_id, peer_count = peers.len(), "dht announce ok");
                        // Fan out add_peer in parallel — a serial path
                        // would stack 5 s connect timeouts across
                        // dozens of returned peers.
                        for addr in peers {
                            let engine = Arc::clone(&engine);
                            let alerts = Arc::clone(&alerts);
                            tokio::spawn(async move {
                                if let Err(e) = engine.add_peer(torrent_id, addr).await
                                    && !matches!(e, AddPeerError::Filtered(_))
                                {
                                    tracing::debug!(
                                        %addr,
                                        error = %e,
                                        "dht add_peer failed"
                                    );
                                    alerts.push(Alert::Error {
                                        torrent: torrent_id,
                                        code: AlertErrorCode::PeerProtocol,
                                    });
                                }
                            });
                        }
                        tokio::time::sleep(cfg.announce_interval).await;
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "dht announce failed");
                        alerts.push(Alert::Error {
                            torrent: torrent_id,
                            code: AlertErrorCode::DhtAnnounceFailed,
                        });
                        tokio::time::sleep(cfg.error_backoff).await;
                    }
                }
            }
        });
        self.tasks.lock().await.push(task);
        Ok(())
    }
}
