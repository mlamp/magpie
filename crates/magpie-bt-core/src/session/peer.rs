//! Per-peer task: owns a framed wire connection and shuttles messages between
//! the wire and the torrent actor.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use futures_util::{SinkExt, StreamExt};
use magpie_bt_wire::{
    Block, BlockRequest, ExtensionHandshake, ExtensionRegistry, HANDSHAKE_LEN, Handshake, Message,
    WireCodec, WireError,
};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::mpsc;
use tokio_util::codec::Framed;
use crate::session::messages::{DisconnectReason, PeerSlot, PeerToSession, SessionToPeer};

/// Default per-peer in-flight request ceiling. Single source of truth — the
/// torrent actor mirrors this when registering a peer if no explicit value is
/// given.
pub const DEFAULT_PER_PEER_IN_FLIGHT: u32 = 4;

/// Default handshake budget (S17). Defends against slow-loris peers that
/// dribble the 68 handshake bytes one byte at a time.
pub const DEFAULT_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

/// Default extension handshake timeout.
pub const DEFAULT_EXTENSION_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);

/// Maximum allowed extension message payload size (1 MiB). Messages exceeding
/// this are silently dropped with a debug log.
const MAX_EXTENSION_PAYLOAD: usize = 1_048_576;

/// Bounded inbox capacity for `PeerToSession`.
///
/// Sized to comfortably absorb `max_in_flight` block payloads plus a few
/// control events without blocking the peer task — when this fills, the
/// peer's `send().await` backpressures the underlying TCP read.
pub const PEER_TO_SESSION_CAPACITY: usize = 64;

/// Whether to act as the connection initiator (outgoing) or accept an incoming
/// peer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HandshakeRole {
    /// We initiated the connection — write our handshake first, then read
    /// theirs.
    Initiator,
    /// They initiated — read their handshake first, then reply.
    Responder,
}

/// Configuration handed to a [`PeerConn`] at spawn time.
#[derive(Debug, Clone)]
pub struct PeerConfig {
    /// Local 20-byte peer id sent in the handshake.
    pub peer_id: [u8; 20],
    /// Torrent info-hash sent / expected in the handshake.
    pub info_hash: [u8; 20],
    /// Whether to advertise BEP 6 Fast extension support.
    pub fast_ext: bool,
    /// Whether to advertise BEP 10 extension protocol support.
    pub extension_protocol: bool,
    /// Maximum requests we keep in-flight on this peer at once.
    pub max_in_flight: u32,
    /// `WireCodec` ceiling — sized by the session once the bitfield length is
    /// known.
    pub max_payload: u32,
    /// Maximum time to spend on the BEP 3 handshake exchange.
    pub handshake_timeout: Duration,
    /// Maximum time to wait for the peer's BEP 10 extension handshake.
    pub extension_handshake_timeout: Duration,
    /// Remote socket address, if known. Passed through to the session in
    /// `PeerToSession::Connected` for PEX.
    pub remote_addr: Option<std::net::SocketAddr>,
    /// BEP 9: size of the info dict in bytes, included in the BEP 10
    /// extension handshake so peers know our metadata size. `None` for
    /// magnet-link torrents that haven't fetched metadata yet.
    pub metadata_size: Option<u64>,
    /// BEP 10 `p` field: our TCP listen port, advertised in the extension
    /// handshake so peers know which port to dial back on (and so the
    /// remote's PEX rounds can advertise us with our reachable address
    /// rather than our outbound source port). `None` if we are not
    /// listening for inbound peers.
    pub local_listen_port: Option<u16>,
}

impl Default for PeerConfig {
    fn default() -> Self {
        Self {
            peer_id: [0; 20],
            info_hash: [0; 20],
            fast_ext: true,
            extension_protocol: true,
            max_in_flight: DEFAULT_PER_PEER_IN_FLIGHT,
            max_payload: magpie_bt_wire::DEFAULT_MAX_PAYLOAD,
            handshake_timeout: DEFAULT_HANDSHAKE_TIMEOUT,
            extension_handshake_timeout: DEFAULT_EXTENSION_HANDSHAKE_TIMEOUT,
            remote_addr: None,
            metadata_size: None,
            local_listen_port: None,
        }
    }
}

/// Errors produced during the handshake exchange.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum HandshakeError {
    /// I/O error while reading or writing the handshake bytes.
    #[error(transparent)]
    Io(#[from] std::io::Error),
    /// Handshake bytes parsed but failed validation.
    #[error("handshake decode: {0}")]
    Decode(#[from] WireError),
    /// Remote info-hash did not match the one we expected.
    #[error("info-hash mismatch: expected {expected:?}, got {actual:?}")]
    InfoHashMismatch {
        /// Hash we sent.
        expected: [u8; 20],
        /// Hash the peer sent.
        actual: [u8; 20],
    },
    /// Handshake exceeded `PeerConfig::handshake_timeout`.
    #[error("handshake timed out after {0:?}")]
    Timeout(Duration),
}

/// Drive the BEP 3 handshake on `stream`. Returns the peer's [`Handshake`]
/// (with reserved bits) on success.
///
/// The exchange is bounded by [`PeerConfig::handshake_timeout`] (S17 hardening:
/// defends against slow-loris peers that dribble the 68-byte handshake).
///
/// # Errors
///
/// Surfaces [`HandshakeError`] on I/O failure, malformed handshake bytes,
/// info-hash mismatch, or timeout.
pub async fn perform_handshake<S>(
    stream: &mut S,
    config: &PeerConfig,
    role: HandshakeRole,
) -> Result<Handshake, HandshakeError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let timeout = config.handshake_timeout;
    tokio::time::timeout(timeout, perform_handshake_inner(stream, config, role))
        .await
        .map_or(Err(HandshakeError::Timeout(timeout)), |res| res)
}

async fn perform_handshake_inner<S>(
    stream: &mut S,
    config: &PeerConfig,
    role: HandshakeRole,
) -> Result<Handshake, HandshakeError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let mut remote_bytes = [0u8; HANDSHAKE_LEN];
    match role {
        HandshakeRole::Initiator => {
            write_handshake_bytes(stream, config).await?;
            stream.read_exact(&mut remote_bytes).await?;
        }
        HandshakeRole::Responder => {
            stream.read_exact(&mut remote_bytes).await?;
            write_handshake_bytes(stream, config).await?;
        }
    }
    let remote = Handshake::decode(&remote_bytes)?;
    if remote.info_hash != config.info_hash {
        return Err(HandshakeError::InfoHashMismatch {
            expected: config.info_hash,
            actual: remote.info_hash,
        });
    }
    Ok(remote)
}

/// Read the peer's [`Handshake`] without replying.
///
/// Used by the inbound-TCP accept loop: we must know the peer's `info_hash`
/// before we can look up the target torrent and pick the right [`PeerConfig`]
/// for our reply. Bounded by `timeout` (same slow-loris defence as
/// [`perform_handshake`]).
///
/// # Errors
///
/// Surfaces [`HandshakeError`] on I/O failure, malformed handshake bytes, or
/// timeout. Does **not** check `info_hash` — the caller matches against the
/// registry.
pub async fn read_handshake<S>(
    stream: &mut S,
    timeout: Duration,
) -> Result<Handshake, HandshakeError>
where
    S: AsyncRead + Unpin,
{
    tokio::time::timeout(timeout, read_handshake_inner(stream))
        .await
        .map_or(Err(HandshakeError::Timeout(timeout)), |res| res)
}

async fn read_handshake_inner<S>(stream: &mut S) -> Result<Handshake, HandshakeError>
where
    S: AsyncRead + Unpin,
{
    let mut remote_bytes = [0u8; HANDSHAKE_LEN];
    stream.read_exact(&mut remote_bytes).await?;
    Ok(Handshake::decode(&remote_bytes)?)
}

/// Write our [`Handshake`] to the peer. Pairs with [`read_handshake`] on the
/// inbound-TCP path: after matching `peer.info_hash` to a torrent in the
/// registry, call this with that torrent's [`PeerConfig`].
///
/// Bounded by `timeout`.
///
/// # Errors
///
/// Surfaces [`HandshakeError::Io`] on I/O failure or [`HandshakeError::Timeout`]
/// if the write doesn't complete in time.
pub async fn write_handshake<S>(
    stream: &mut S,
    config: &PeerConfig,
    timeout: Duration,
) -> Result<(), HandshakeError>
where
    S: AsyncWrite + Unpin,
{
    tokio::time::timeout(timeout, write_handshake_bytes(stream, config))
        .await
        .map_or(Err(HandshakeError::Timeout(timeout)), |res| res)
}

async fn write_handshake_bytes<S>(stream: &mut S, config: &PeerConfig) -> Result<(), HandshakeError>
where
    S: AsyncWrite + Unpin,
{
    let mut local = Handshake::new(config.info_hash, config.peer_id);
    if config.fast_ext {
        local = local.with_fast_ext();
    }
    if config.extension_protocol {
        local = local.with_extension_protocol();
    }
    let local_bytes = local.to_bytes();
    stream.write_all(&local_bytes).await?;
    Ok(())
}

/// Per-peer connection task.
///
/// Owns the framed transport plus a small piece of per-connection state
/// (in-flight request set, choke / interest flags). Push events to the
/// session via the *bounded* `tx_to_session` channel (S1 hardening) — when
/// full, the `send().await` naturally backpressures the wire reader and TCP.
/// Commands flow back via `rx_from_session` (kept unbounded because session
/// command rate is naturally bounded by `max_in_flight` per peer).
pub struct PeerConn<S>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    framed: Framed<S, WireCodec>,
    slot: PeerSlot,
    config: PeerConfig,
    tx_to_session: mpsc::Sender<PeerToSession>,
    rx_from_session: mpsc::UnboundedReceiver<SessionToPeer>,
    in_flight: HashSet<BlockRequest>,
    /// Whether *we* are interested in the peer.
    am_interested: bool,
    /// Peer-tier shaper bucket pair (#22). Cached at startup; `try_consume`
    /// on the hot path is lock-free. `None` for tests that bypass the
    /// shaper (legacy callers of `PeerConn::new`).
    ///
    /// **Plan invariant (ADR-0013 / #22)**: peer tier only on the hot
    /// path. Session + torrent tiers are touched by the refiller via
    /// demand aggregation.
    shaper_buckets: Option<Arc<crate::session::shaper::DuplexBuckets>>,
    /// Per-peer upload/download counters (ADR-0014). Cached so
    /// `add_uploaded`/`add_downloaded` on the hot path are lock-free.
    /// `None` for tests that don't track stats.
    peer_stats: Option<Arc<crate::session::stats::PeerStats>>,
    /// Per-peer extension ID registry (BEP 10). Populated after the
    /// extension handshake exchange; `None` if the peer doesn't support
    /// extensions.
    extension_registry: Option<ExtensionRegistry>,
}

impl<S> PeerConn<S>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    /// Wrap an already-handshaken transport into a peer task ready to run.
    /// No shaper attached — uses pass-through semantics on the hot path.
    /// For engine-spawned peers, prefer [`Self::with_shaper`].
    #[must_use]
    pub fn new(
        stream: S,
        slot: PeerSlot,
        config: PeerConfig,
        tx_to_session: mpsc::Sender<PeerToSession>,
        rx_from_session: mpsc::UnboundedReceiver<SessionToPeer>,
    ) -> Self {
        let codec = WireCodec::new(config.max_payload);
        let framed = Framed::new(stream, codec);
        Self {
            framed,
            slot,
            config,
            tx_to_session,
            rx_from_session,
            in_flight: HashSet::new(),
            am_interested: false,
            shaper_buckets: None,
            peer_stats: None,
            extension_registry: None,
        }
    }

    /// Same as [`Self::new`] but carries a shaper bucket handle for
    /// hot-path `try_consume`. `shaper_buckets` is typically obtained via
    /// `Shaper::peer_buckets(slot)` after `register_peer`.
    #[must_use]
    pub fn with_shaper(
        stream: S,
        slot: PeerSlot,
        config: PeerConfig,
        tx_to_session: mpsc::Sender<PeerToSession>,
        rx_from_session: mpsc::UnboundedReceiver<SessionToPeer>,
        shaper_buckets: Arc<crate::session::shaper::DuplexBuckets>,
    ) -> Self {
        let mut conn = Self::new(stream, slot, config, tx_to_session, rx_from_session);
        conn.shaper_buckets = Some(shaper_buckets);
        conn
    }

    /// Attach a peer-stats handle (ADR-0014). Counters are incremented
    /// on the hot path at the same sites as `shaper.try_consume`.
    #[must_use]
    pub fn with_peer_stats(mut self, peer_stats: Arc<crate::session::stats::PeerStats>) -> Self {
        self.peer_stats = Some(peer_stats);
        self
    }

    /// Run the message loop until the peer disconnects, the session shuts us
    /// down, or a fatal error is observed.
    pub async fn run(mut self, peer_handshake: Handshake) {
        let span = tracing::info_span!(
            "peer",
            slot = self.slot.0,
            supports_fast = peer_handshake.supports_fast_ext(),
        );
        let _enter = span.enter();
        tracing::debug!("peer task started");
        if self
            .tx_to_session
            .send(PeerToSession::Connected {
                slot: self.slot,
                peer_id: peer_handshake.peer_id,
                supports_fast: peer_handshake.supports_fast_ext(),
                addr: self.config.remote_addr,
            })
            .await
            .is_err()
        {
            // Session is gone before we even reported in. Nothing to do.
            return;
        }

        // BEP 10: exchange extension handshakes if both sides support it.
        if self.config.extension_protocol
            && peer_handshake.supports_extension_protocol()
            && let Err(e) = self.exchange_extension_handshake().await
        {
            tracing::debug!(error = %e, "extension handshake failed; continuing without extensions");
        }

        let reason = self.message_loop().await;
        tracing::debug!(?reason, "peer task exiting");

        // Best-effort shutdown notification — session may already be gone.
        let _ = self
            .tx_to_session
            .send(PeerToSession::Disconnected {
                slot: self.slot,
                reason,
            })
            .await;
    }

    /// BEP 10: send our extension handshake and wait (with timeout) for the
    /// peer's response. Lenient: if the first message received is not an
    /// extension handshake, we skip extension support and feed the message
    /// back into normal handling.
    async fn exchange_extension_handshake(&mut self) -> Result<(), String> {
        // Build local extension ID assignments.
        let local: HashMap<String, u8> = [
            ("ut_metadata".to_owned(), 1u8),
            ("ut_pex".to_owned(), 2u8),
        ]
        .into_iter()
        .collect();
        let mut registry = ExtensionRegistry::new(local);

        // Encode and send our handshake.
        let mut hs = registry.our_handshake();
        hs.metadata_size = self.config.metadata_size;
        hs.listen_port = self.config.local_listen_port;
        let payload = Bytes::from(hs.encode());
        self.framed
            .send(Message::Extended { id: 0, payload })
            .await
            .map_err(|e| e.to_string())?;

        // Wait for the peer's extension handshake with a configurable timeout.
        let timeout = self.config.extension_handshake_timeout;
        let maybe_msg = tokio::time::timeout(timeout, self.framed.next()).await;

        match maybe_msg {
            Ok(Some(Ok(Message::Extended { id: 0, payload }))) => {
                let peer_hs = ExtensionHandshake::decode(&payload)
                    .map_err(|e| e.to_string())?;
                registry.set_remote(&peer_hs);
                let extensions = peer_hs.extensions.clone();
                let metadata_size = peer_hs.metadata_size;
                let client = peer_hs.client.clone();
                let listen_port = peer_hs.listen_port;
                self.extension_registry = Some(registry);

                // Report to the session.
                let _ = self
                    .tx_to_session
                    .send(PeerToSession::ExtensionHandshake {
                        slot: self.slot,
                        extensions,
                        metadata_size,
                        client,
                        listen_port,
                    })
                    .await;
                Ok(())
            }
            Ok(Some(Ok(other_msg))) => {
                // Peer sent something else first — store the registry without
                // remote mappings and process the message normally.
                self.extension_registry = Some(registry);
                if let Err(reason) = self.handle_inbound(other_msg).await {
                    return Err(format!("{reason:?}"));
                }
                Ok(())
            }
            Ok(Some(Err(e))) => Err(e.to_string()),
            Ok(None) => Err("peer closed connection".to_owned()),
            Err(_) => {
                // Timeout — continue without extensions.
                self.extension_registry = Some(registry);
                Ok(())
            }
        }
    }

    #[allow(clippy::too_many_lines)]
    async fn message_loop(&mut self) -> DisconnectReason {
        loop {
            tokio::select! {
                cmd = self.rx_from_session.recv() => match cmd {
                    None | Some(SessionToPeer::Shutdown) => return DisconnectReason::Shutdown,
                    Some(SessionToPeer::SetInterested(state)) => {
                        if self.am_interested != state {
                            self.am_interested = state;
                            let m = if state { Message::Interested } else { Message::NotInterested };
                            if let Err(e) = self.framed.send(m).await {
                                return io_or_protocol(e);
                            }
                        }
                    }
                    Some(SessionToPeer::Request(req)) => {
                        let in_flight_u32 = u32::try_from(self.in_flight.len()).unwrap_or(u32::MAX);
                        if in_flight_u32 >= self.config.max_in_flight {
                            // Drop silently; the session is responsible for
                            // not over-issuing.
                            continue;
                        }
                        if !self.in_flight.insert(req) {
                            continue;
                        }
                        if let Err(e) = self.framed.send(Message::Request(req)).await {
                            return io_or_protocol(e);
                        }
                    }
                    Some(SessionToPeer::Cancel(req)) => {
                        if self.in_flight.remove(&req)
                            && let Err(e) = self.framed.send(Message::Cancel(req)).await {
                            return io_or_protocol(e);
                        }
                    }
                    Some(SessionToPeer::Have(piece)) => {
                        if let Err(e) = self.framed.send(Message::Have(piece)).await {
                            return io_or_protocol(e);
                        }
                    }
                    Some(SessionToPeer::SendBitfield(bytes)) => {
                        if let Err(e) = self.framed.send(Message::Bitfield(bytes.into())).await {
                            return io_or_protocol(e);
                        }
                    }
                    Some(SessionToPeer::SendHaveAll) => {
                        if let Err(e) = self.framed.send(Message::HaveAll).await {
                            return io_or_protocol(e);
                        }
                    }
                    Some(SessionToPeer::SendHaveNone) => {
                        if let Err(e) = self.framed.send(Message::HaveNone).await {
                            return io_or_protocol(e);
                        }
                    }
                    Some(SessionToPeer::Choke) => {
                        if let Err(e) = self.framed.send(Message::Choke).await {
                            return io_or_protocol(e);
                        }
                    }
                    Some(SessionToPeer::Unchoke) => {
                        if let Err(e) = self.framed.send(Message::Unchoke).await {
                            return io_or_protocol(e);
                        }
                    }
                    Some(SessionToPeer::BlockReady { req, data }) => {
                        // Plan invariant (ADR-0013 / #22): peer tier only
                        // on the hot path. Park on `wait_for_refill` on
                        // denial — Refiller's `notify_refill` wakes us
                        // exactly when tokens arrive (no polling).
                        let bytes = data.len() as u64;
                        if let Some(b) = self.shaper_buckets.as_ref() {
                            while !b.up.try_consume(bytes) {
                                b.up.wait_for_refill().await;
                            }
                        }
                        let block = Block::new(req.piece, req.offset, data);
                        if let Err(e) = self.framed.send(Message::Piece(block)).await {
                            return io_or_protocol(e);
                        }
                        // ADR-0014 per-peer upload accounting. Bumped
                        // after a successful send so a wire error isn't
                        // counted as bytes-sent.
                        if let Some(s) = self.peer_stats.as_ref() {
                            s.add_uploaded(bytes);
                        }
                    }
                    Some(SessionToPeer::SendExtended { extension_name, payload }) => {
                        if let Some(ref registry) = self.extension_registry {
                            if let Some(remote_id) = registry.remote_id(&extension_name) {
                                let msg = Message::Extended { id: remote_id, payload };
                                if let Err(e) = self.framed.send(msg).await {
                                    return io_or_protocol(e);
                                }
                            } else {
                                tracing::debug!(
                                    extension = %extension_name,
                                    "peer does not support extension; dropping SendExtended"
                                );
                            }
                        } else {
                            tracing::debug!(
                                extension = %extension_name,
                                "no extension registry; dropping SendExtended"
                            );
                        }
                    }
                    Some(SessionToPeer::RejectRequest(req)) => {
                        // RejectRequest is BEP 6 (Fast Extension). Skip if the
                        // peer doesn't support it — the request simply never
                        // gets a reply, which is within spec.
                        if self.config.fast_ext
                            && let Err(e) = self.framed.send(Message::RejectRequest(req)).await
                        {
                            return io_or_protocol(e);
                        }
                    }
                },
                frame = self.framed.next() => match frame {
                    None => return DisconnectReason::Eof,
                    Some(Err(WireError::Io(e))) => return DisconnectReason::Io(e.to_string()),
                    Some(Err(e)) => return DisconnectReason::ProtocolError(e.to_string()),
                    Some(Ok(msg)) => {
                        if let Err(reason) = self.handle_inbound(msg).await {
                            return reason;
                        }
                    }
                },
            }
        }
    }

    /// Forward an inbound wire message into the session, treating a closed
    /// session inbox as a terminal condition (S16 hardening).
    async fn handle_inbound(&mut self, msg: Message) -> Result<(), DisconnectReason> {
        // Several arms intentionally `{}`; the explicit pattern documents
        // *why* a given message is ignored in M1 (no upload side, BEP 6 hints
        // not yet wired into the picker). Clippy's `match_same_arms` would
        // have us collapse them — keep the docs.
        #[allow(clippy::match_same_arms)]
        let event = match msg {
            Message::KeepAlive => return Ok(()),
            Message::Choke => {
                // BEP 3: outstanding requests are implicitly dropped on Choke
                // unless Fast extension was negotiated.
                if !self.config.fast_ext {
                    self.in_flight.clear();
                }
                PeerToSession::Choked { slot: self.slot }
            }
            Message::Unchoke => PeerToSession::Unchoked { slot: self.slot },
            Message::Interested => PeerToSession::Interested { slot: self.slot },
            Message::NotInterested => PeerToSession::NotInterested { slot: self.slot },
            Message::Have(piece) => PeerToSession::Have {
                slot: self.slot,
                piece,
            },
            Message::Bitfield(bytes) => PeerToSession::Bitfield {
                slot: self.slot,
                bytes,
            },
            Message::HaveAll => PeerToSession::HaveAll { slot: self.slot },
            Message::HaveNone => PeerToSession::HaveNone { slot: self.slot },
            Message::Request(req) => PeerToSession::BlockRequested {
                slot: self.slot,
                req,
            },
            Message::Cancel(req) => PeerToSession::RequestCancelled {
                slot: self.slot,
                req,
            },
            Message::Piece(block) => {
                let len = u32::try_from(block.data.len()).unwrap_or(u32::MAX);
                let req = BlockRequest::new(block.piece, block.offset, len);
                self.in_flight.remove(&req);
                // Download-side consume is **accounting only** — the
                // bytes are already in the socket / decoded frame. Real
                // download shaping would gate `framed.next()` before the
                // read, not after. Updating `consumed`/`denied` here
                // keeps demand signals coherent for the refiller and
                // unblocks the M2 Prometheus `magpie_shaper_*` exposure
                // (follow-up). No await here — passthrough `try_consume`
                // always succeeds.
                if let Some(b) = self.shaper_buckets.as_ref() {
                    let _ = b.down.try_consume(u64::from(len));
                }
                // ADR-0014 per-peer download accounting (bytes received).
                if let Some(s) = self.peer_stats.as_ref() {
                    s.add_downloaded(u64::from(len));
                }
                PeerToSession::BlockReceived {
                    slot: self.slot,
                    piece: block.piece,
                    offset: block.offset,
                    data: block.data,
                }
            }
            Message::RejectRequest(req) => {
                self.in_flight.remove(&req);
                PeerToSession::Rejected {
                    slot: self.slot,
                    req,
                }
            }
            Message::Extended { id, payload } => {
                // MAJOR #3: reject oversized extension payloads.
                if payload.len() > MAX_EXTENSION_PAYLOAD {
                    tracing::debug!(
                        len = payload.len(),
                        max = MAX_EXTENSION_PAYLOAD,
                        "dropping oversized extension message"
                    );
                    return Ok(());
                }
                if id == 0 {
                    // Late extension handshake — some peers send it after other
                    // messages. Just update the registry if we have one.
                    if let Some(ref mut registry) = self.extension_registry
                        && let Ok(hs) = ExtensionHandshake::decode(&payload)
                    {
                        registry.set_remote(&hs);
                    }
                    return Ok(());
                }
                // Look up the canonical name for this extension ID.
                // The peer sent us a message using OUR local ID.
                if let Some(ref registry) = self.extension_registry
                    && let Some(extension_name) = registry.local_name_for_id(id)
                {
                    self.tx_to_session
                        .send(PeerToSession::ExtensionMessage {
                            slot: self.slot,
                            extension_name: extension_name.to_owned(),
                            payload,
                        })
                        .await
                        .map_err(|_| DisconnectReason::Shutdown)?;
                }
                return Ok(());
            }
            // BEP 6 hints (SuggestPiece, AllowedFast) are accepted but not yet
            // wired into the picker. `Message` is `#[non_exhaustive]`, so the
            // trailing wildcard absorbs any future variants without breaking the
            // connection.
            _ => return Ok(()),
        };
        if self.tx_to_session.send(event).await.is_err() {
            return Err(DisconnectReason::Shutdown);
        }
        Ok(())
    }
}

fn io_or_protocol(e: WireError) -> DisconnectReason {
    match e {
        WireError::Io(io) => DisconnectReason::Io(io.to_string()),
        other => DisconnectReason::ProtocolError(other.to_string()),
    }
}
