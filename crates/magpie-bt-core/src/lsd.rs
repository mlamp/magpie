//! Local Service Discovery (BEP 14).
//!
//! LSD uses UDP multicast to discover peers on the local network without
//! requiring a tracker or DHT. A peer periodically announces the info hashes
//! it is interested in to the multicast group `239.192.152.143:6771`. Other
//! peers listening on the same group check whether they hold a matching torrent
//! and, if so, add the announcing peer.
//!
//! **Private torrents** (BEP 27) are never announced over LSD.
//!
//! # Wire format
//!
//! ```text
//! BT-SEARCH * HTTP/1.1\r\n
//! Host: 239.192.152.143:6771\r\n
//! Port: <our_listen_port>\r\n
//! Infohash: <40-char hex lowercase info_hash>\r\n
//! cookie: <optional unique id>\r\n
//! \r\n
//! ```
//!
//! Multiple `Infohash:` headers may appear in a single message.

use std::collections::HashSet;
use std::fmt::Write as _;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::time::Duration;

use socket2::{Domain, Protocol, Socket, Type};
use tokio::net::UdpSocket;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// Default IPv4 multicast group for BEP 14.
pub const LSD_MULTICAST_ADDR: Ipv4Addr = Ipv4Addr::new(239, 192, 152, 143);

/// Default multicast port for BEP 14.
pub const LSD_PORT: u16 = 6771;

/// Default announce interval (5 minutes).
pub const DEFAULT_ANNOUNCE_INTERVAL: Duration = Duration::from_secs(300);

/// Minimum announce interval (1 minute).
pub const MIN_ANNOUNCE_INTERVAL: Duration = Duration::from_secs(60);

/// Maximum receive buffer size for incoming LSD messages.
const MAX_MSG_SIZE: usize = 4096;

/// Maximum number of info hashes allowed in a single LSD message.
const MAX_INFOHASHES_PER_MESSAGE: usize = 100;

// ---------------------------------------------------------------------------
// Error type
// ---------------------------------------------------------------------------

/// Errors produced when parsing an LSD announce message.
#[derive(Debug, thiserror::Error)]
pub enum LsdError {
    /// The message does not conform to the expected BEP 14 format.
    #[error("invalid format: {0}")]
    InvalidFormat(String),

    /// The `Port:` header is missing.
    #[error("missing Port header")]
    MissingPort,

    /// No `Infohash:` header was found.
    #[error("missing Infohash header")]
    MissingInfoHash,

    /// An `Infohash:` value is not valid 40-char lowercase hex.
    #[error("invalid info hash: {0}")]
    InvalidInfoHash(String),

    /// The `Port:` value is not a valid u16.
    #[error("invalid port: {0}")]
    InvalidPort(String),
}

// ---------------------------------------------------------------------------
// Codec: LsdAnnounce
// ---------------------------------------------------------------------------

/// A parsed (or to-be-encoded) BEP 14 LSD announce message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LsdAnnounce {
    /// The TCP listen port of the announcing peer.
    pub port: u16,
    /// Info hashes the peer is interested in.
    pub info_hashes: Vec<[u8; 20]>,
    /// Optional cookie used to filter out our own announcements.
    pub cookie: Option<String>,
}

impl LsdAnnounce {
    /// Encode this announce into the BEP 14 wire format.
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let mut buf = String::with_capacity(256);
        buf.push_str("BT-SEARCH * HTTP/1.1\r\n");
        let _ = write!(buf, "Host: {LSD_MULTICAST_ADDR}:{LSD_PORT}\r\n");
        let _ = write!(buf, "Port: {}\r\n", self.port);
        for hash in &self.info_hashes {
            let hex = hex_encode(hash);
            let _ = write!(buf, "Infohash: {hex}\r\n");
        }
        if let Some(ref cookie) = self.cookie {
            let _ = write!(buf, "cookie: {cookie}\r\n");
        }
        buf.push_str("\r\n");
        buf.into_bytes()
    }

    /// Decode a BEP 14 announce from raw bytes.
    ///
    /// This parser is lenient: it accepts both `\r\n` and bare `\n` line
    /// endings, which real-world clients emit interchangeably.
    pub fn decode(buf: &[u8]) -> Result<Self, LsdError> {
        let text = std::str::from_utf8(buf)
            .map_err(|e| LsdError::InvalidFormat(format!("not utf-8: {e}")))?;

        // Normalise line endings: replace \r\n with \n, then split on \n.
        let normalised = text.replace("\r\n", "\n");
        let mut lines = normalised.lines();

        // First line must be the request line.
        let request_line = lines
            .next()
            .ok_or_else(|| LsdError::InvalidFormat("empty message".into()))?;
        if !request_line.starts_with("BT-SEARCH") {
            return Err(LsdError::InvalidFormat(format!(
                "unexpected request line: {request_line}"
            )));
        }

        let mut port: Option<u16> = None;
        let mut info_hashes: Vec<[u8; 20]> = Vec::new();
        let mut cookie: Option<String> = None;

        for line in lines {
            let line = line.trim();
            if line.is_empty() {
                break;
            }
            if let Some((key, value)) = line.split_once(':') {
                let key = key.trim();
                let value = value.trim();
                match key.to_ascii_lowercase().as_str() {
                    "port" => {
                        port = Some(
                            value
                                .parse::<u16>()
                                .map_err(|_| LsdError::InvalidPort(value.to_owned()))?,
                        );
                    }
                    "infohash" => {
                        if info_hashes.len() >= MAX_INFOHASHES_PER_MESSAGE {
                            return Err(LsdError::InvalidFormat(
                                "too many info hashes".into(),
                            ));
                        }
                        let hash = hex_decode(value).map_err(|()| {
                            LsdError::InvalidInfoHash(value.to_owned())
                        })?;
                        info_hashes.push(hash);
                    }
                    "cookie" => {
                        cookie = Some(value.to_owned());
                    }
                    _ => { /* ignore unknown headers */ }
                }
            }
        }

        let port = port.ok_or(LsdError::MissingPort)?;
        if info_hashes.is_empty() {
            return Err(LsdError::MissingInfoHash);
        }

        Ok(Self {
            port,
            info_hashes,
            cookie,
        })
    }
}

// ---------------------------------------------------------------------------
// Hex helpers (no external dep)
// ---------------------------------------------------------------------------

fn hex_encode(bytes: &[u8; 20]) -> String {
    let mut s = String::with_capacity(40);
    for b in bytes {
        let _ = write!(s, "{b:02x}");
    }
    s
}

fn hex_decode(s: &str) -> Result<[u8; 20], ()> {
    let s = s.trim();
    if s.len() != 40 {
        return Err(());
    }
    let mut out = [0u8; 20];
    for (i, chunk) in s.as_bytes().chunks(2).enumerate() {
        let hi = hex_nibble(chunk[0]).ok_or(())?;
        let lo = hex_nibble(chunk[1]).ok_or(())?;
        out[i] = (hi << 4) | lo;
    }
    Ok(out)
}

const fn hex_nibble(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Discovery event
// ---------------------------------------------------------------------------

/// A peer discovered via LSD multicast.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LsdDiscovery {
    /// The info hash the peer announced.
    pub info_hash: [u8; 20],
    /// The socket address (sender IP + announced port) of the peer.
    pub addr: SocketAddr,
}

// ---------------------------------------------------------------------------
// Config
// ---------------------------------------------------------------------------

/// Configuration for the [`LsdService`].
#[derive(Debug, Clone)]
pub struct LsdConfig {
    /// Whether LSD is enabled at all.
    pub enabled: bool,
    /// How often to re-announce each info hash. Clamped to at least
    /// [`MIN_ANNOUNCE_INTERVAL`].
    pub announce_interval: Duration,
    /// The multicast socket address to use. Defaults to `239.192.152.143:6771`.
    pub multicast_addr: SocketAddr,
}

impl Default for LsdConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            announce_interval: DEFAULT_ANNOUNCE_INTERVAL,
            multicast_addr: SocketAddr::V4(SocketAddrV4::new(LSD_MULTICAST_ADDR, LSD_PORT)),
        }
    }
}

// ---------------------------------------------------------------------------
// Shared state between the handle and the task
// ---------------------------------------------------------------------------

/// Commands sent from the [`LsdService`] handle to the background task.
#[derive(Clone, Copy)]
enum LsdCommand {
    Register { info_hash: [u8; 20] },
    Unregister { info_hash: [u8; 20] },
}

// ---------------------------------------------------------------------------
// LSD service (handle + actor)
// ---------------------------------------------------------------------------

/// Handle to the LSD background service.
///
/// Created via [`LsdService::new`]; the caller should spawn the returned
/// service's [`run`](LsdService::run) method as a Tokio task.
///
/// Use [`register`](LsdService::register) and
/// [`unregister`](LsdService::unregister) to manage which info hashes are
/// announced over multicast.
pub struct LsdService {
    config: LsdConfig,
    listen_port: u16,
    token: CancellationToken,
    cookie: String,
    cmd_tx: mpsc::UnboundedSender<LsdCommand>,
    cmd_rx: mpsc::UnboundedReceiver<LsdCommand>,
    discovery_tx: mpsc::Sender<LsdDiscovery>,
}

impl LsdService {
    /// Create a new LSD service handle and the discovery receiver channel.
    ///
    /// The `listen_port` is the TCP port we advertise to other LSD peers. The
    /// `token` controls graceful shutdown of the background task.
    ///
    /// Returns `(service, discovery_rx)`. The caller must spawn
    /// `service.run()` as a Tokio task and consume `LsdDiscovery` events from
    /// `discovery_rx`.
    #[must_use]
    pub fn new(
        config: LsdConfig,
        listen_port: u16,
        token: CancellationToken,
    ) -> (Self, mpsc::Receiver<LsdDiscovery>) {
        let (discovery_tx, discovery_rx) = mpsc::channel(64);
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();

        // Generate a random cookie to identify our own announcements.
        let cookie = generate_cookie();

        let service = Self {
            config,
            listen_port,
            token,
            cookie,
            cmd_tx,
            cmd_rx,
            discovery_tx,
        };
        (service, discovery_rx)
    }

    /// Register an info hash for periodic LSD announcement.
    ///
    /// If `private` is `true` the hash is silently ignored — BEP 27 forbids
    /// LSD for private torrents.
    pub fn register(&self, info_hash: [u8; 20], private: bool) {
        if private {
            tracing::debug!(
                info_hash = %hex_encode(&info_hash),
                "skipping LSD registration for private torrent"
            );
            return;
        }
        let _ = self.cmd_tx.send(LsdCommand::Register { info_hash });
    }

    /// Remove an info hash from periodic LSD announcement.
    pub fn unregister(&self, info_hash: [u8; 20]) {
        let _ = self.cmd_tx.send(LsdCommand::Unregister { info_hash });
    }

    /// Run the LSD service loop.
    ///
    /// This method drives the multicast listener and periodic announcer. It
    /// runs until the [`CancellationToken`] is cancelled.
    ///
    /// # Errors
    ///
    /// Returns `Err` if the UDP socket cannot be bound or the multicast group
    /// cannot be joined.
    pub async fn run(mut self) -> std::io::Result<()> {
        if !self.config.enabled {
            tracing::info!("LSD disabled by configuration");
            self.token.cancelled().await;
            return Ok(());
        }

        let multicast_addr = match self.config.multicast_addr {
            SocketAddr::V4(v4) => v4,
            SocketAddr::V6(_) => {
                tracing::warn!("IPv6 multicast not supported for LSD; disabling");
                self.token.cancelled().await;
                return Ok(());
            }
        };

        let socket = bind_multicast(multicast_addr)?;

        tracing::info!(
            addr = %self.config.multicast_addr,
            port = self.listen_port,
            "LSD service started"
        );

        let announce_interval = self.config.announce_interval.max(MIN_ANNOUNCE_INTERVAL);
        let mut registered: HashSet<[u8; 20]> = HashSet::new();
        let mut announce_tick = tokio::time::interval(announce_interval);
        // The first tick fires immediately; skip it so we don't announce before
        // any hashes are registered.
        announce_tick.tick().await;

        let mut recv_buf = vec![0u8; MAX_MSG_SIZE];
        let dest = self.config.multicast_addr;

        self.run_loop(&socket, &mut registered, &mut announce_tick, &mut recv_buf, dest)
            .await;

        // Leave the multicast group on shutdown.
        let _ = socket.leave_multicast_v4(*multicast_addr.ip(), Ipv4Addr::UNSPECIFIED);
        Ok(())
    }

    /// Inner select loop, factored out to stay within the line-count lint.
    async fn run_loop(
        &mut self,
        socket: &UdpSocket,
        registered: &mut HashSet<[u8; 20]>,
        announce_tick: &mut tokio::time::Interval,
        recv_buf: &mut [u8],
        dest: SocketAddr,
    ) {
        loop {
            tokio::select! {
                () = self.token.cancelled() => {
                    tracing::info!("LSD service shutting down");
                    break;
                }

                _ = announce_tick.tick() => {
                    self.send_announce(socket, registered, dest).await;
                }

                result = socket.recv_from(recv_buf) => {
                    if self.handle_recv(result, recv_buf).await.is_err() {
                        break;
                    }
                }

                cmd = self.cmd_rx.recv() => {
                    if !Self::handle_command(cmd, registered) {
                        break;
                    }
                }
            }
        }
    }

    /// Build and send an LSD announce for all registered hashes.
    async fn send_announce(
        &self,
        socket: &UdpSocket,
        registered: &HashSet<[u8; 20]>,
        dest: SocketAddr,
    ) {
        if registered.is_empty() {
            return;
        }
        let hashes: Vec<[u8; 20]> = registered.iter().copied().collect();
        let msg = LsdAnnounce {
            port: self.listen_port,
            info_hashes: hashes,
            cookie: Some(self.cookie.clone()),
        };
        let encoded = msg.encode();
        if let Err(e) = socket.send_to(&encoded, dest).await {
            tracing::warn!(error = %e, "failed to send LSD announce");
        } else {
            tracing::debug!(count = msg.info_hashes.len(), "sent LSD announce");
        }
    }

    /// Process a received datagram. Returns `Err(())` if the discovery channel
    /// is closed and the loop should exit.
    async fn handle_recv(
        &self,
        result: std::io::Result<(usize, SocketAddr)>,
        recv_buf: &[u8],
    ) -> Result<(), ()> {
        let (len, sender_addr) = match result {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(error = %e, "LSD recv error");
                return Ok(());
            }
        };

        let announce = match LsdAnnounce::decode(&recv_buf[..len]) {
            Ok(a) => a,
            Err(e) => {
                tracing::trace!(error = %e, sender = %sender_addr, "ignoring malformed LSD message");
                return Ok(());
            }
        };

        // Filter out our own announcements.
        if announce.cookie.as_deref() == Some(&self.cookie) {
            return Ok(());
        }

        let peer_addr = SocketAddr::new(sender_addr.ip(), announce.port);
        for info_hash in &announce.info_hashes {
            let discovery = LsdDiscovery {
                info_hash: *info_hash,
                addr: peer_addr,
            };
            if self.discovery_tx.send(discovery).await.is_err() {
                tracing::debug!("LSD discovery channel closed");
                return Err(());
            }
        }
        Ok(())
    }

    /// Process a register/unregister command. Returns `false` if the channel
    /// is closed and the loop should exit.
    fn handle_command(cmd: Option<LsdCommand>, registered: &mut HashSet<[u8; 20]>) -> bool {
        match cmd {
            Some(LsdCommand::Register { info_hash }) => {
                tracing::debug!(info_hash = %hex_encode(&info_hash), "registered info hash for LSD");
                registered.insert(info_hash);
                true
            }
            Some(LsdCommand::Unregister { info_hash }) => {
                tracing::debug!(info_hash = %hex_encode(&info_hash), "unregistered info hash from LSD");
                registered.remove(&info_hash);
                true
            }
            None => {
                tracing::debug!("LSD command channel closed");
                false
            }
        }
    }
}

/// Bind a UDP socket to `INADDR_ANY` on the LSD port and join the multicast
/// group.
///
/// Uses `socket2` to set `SO_REUSEADDR` (and `SO_REUSEPORT` on macOS/iOS) so
/// that multiple LSD listeners can coexist on the same host. The multicast TTL
/// is set to 1 to prevent announcements from leaking beyond the local network.
fn bind_multicast(addr: SocketAddrV4) -> std::io::Result<UdpSocket> {
    let socket = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
    socket.set_reuse_address(true)?;
    #[cfg(any(target_os = "macos", target_os = "ios"))]
    socket.set_reuse_port(true)?;
    socket.set_nonblocking(true)?;
    socket.bind(&SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, addr.port()).into())?;
    let std_socket: std::net::UdpSocket = socket.into();
    let tokio_socket = UdpSocket::from_std(std_socket)?;
    tokio_socket.set_multicast_ttl_v4(1)?;
    tokio_socket.join_multicast_v4(*addr.ip(), Ipv4Addr::UNSPECIFIED)?;
    Ok(tokio_socket)
}

/// Generate a short random cookie string for self-identification.
fn generate_cookie() -> String {
    let mut buf = [0u8; 8];
    getrandom::fill(&mut buf).expect("getrandom failed");
    hex_encode_slice(&buf)
}

/// Hex-encode an arbitrary-length byte slice.
fn hex_encode_slice(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(s, "{b:02x}");
    }
    s
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_hash() -> [u8; 20] {
        let mut h = [0u8; 20];
        for (i, b) in h.iter_mut().enumerate() {
            *b = u8::try_from(i).unwrap();
        }
        h
    }

    fn sample_hash_2() -> [u8; 20] {
        let mut h = [0xffu8; 20];
        h[0] = 0xab;
        h[19] = 0xcd;
        h
    }

    // -- Codec unit tests ---------------------------------------------------

    #[test]
    fn encode_decode_roundtrip() {
        let announce = LsdAnnounce {
            port: 6881,
            info_hashes: vec![sample_hash()],
            cookie: Some("test-cookie".into()),
        };
        let encoded = announce.encode();
        let decoded = LsdAnnounce::decode(&encoded).expect("decode failed");
        assert_eq!(announce, decoded);
    }

    #[test]
    fn encode_decode_roundtrip_no_cookie() {
        let announce = LsdAnnounce {
            port: 51413,
            info_hashes: vec![sample_hash()],
            cookie: None,
        };
        let encoded = announce.encode();
        let decoded = LsdAnnounce::decode(&encoded).expect("decode failed");
        assert_eq!(decoded.port, 51413);
        assert_eq!(decoded.info_hashes, vec![sample_hash()]);
        assert_eq!(decoded.cookie, None);
    }

    #[test]
    fn parse_realistic_announce() {
        let msg = b"BT-SEARCH * HTTP/1.1\r\n\
            Host: 239.192.152.143:6771\r\n\
            Port: 6881\r\n\
            Infohash: 000102030405060708090a0b0c0d0e0f10111213\r\n\
            cookie: abc123\r\n\
            \r\n";
        let ann = LsdAnnounce::decode(msg).expect("decode failed");
        assert_eq!(ann.port, 6881);
        assert_eq!(ann.info_hashes, vec![sample_hash()]);
        assert_eq!(ann.cookie.as_deref(), Some("abc123"));
    }

    #[test]
    fn parse_multiple_info_hashes() {
        let msg = format!(
            "BT-SEARCH * HTTP/1.1\r\n\
             Host: 239.192.152.143:6771\r\n\
             Port: 12345\r\n\
             Infohash: {}\r\n\
             Infohash: {}\r\n\
             \r\n",
            hex_encode(&sample_hash()),
            hex_encode(&sample_hash_2()),
        );
        let ann = LsdAnnounce::decode(msg.as_bytes()).expect("decode failed");
        assert_eq!(ann.port, 12345);
        assert_eq!(ann.info_hashes.len(), 2);
        assert_eq!(ann.info_hashes[0], sample_hash());
        assert_eq!(ann.info_hashes[1], sample_hash_2());
    }

    #[test]
    fn missing_port_error() {
        let msg = b"BT-SEARCH * HTTP/1.1\r\n\
            Host: 239.192.152.143:6771\r\n\
            Infohash: 000102030405060708090a0b0c0d0e0f10111213\r\n\
            \r\n";
        let err = LsdAnnounce::decode(msg).unwrap_err();
        assert!(matches!(err, LsdError::MissingPort));
    }

    #[test]
    fn missing_info_hash_error() {
        let msg = b"BT-SEARCH * HTTP/1.1\r\n\
            Host: 239.192.152.143:6771\r\n\
            Port: 6881\r\n\
            \r\n";
        let err = LsdAnnounce::decode(msg).unwrap_err();
        assert!(matches!(err, LsdError::MissingInfoHash));
    }

    #[test]
    fn lenient_line_endings_lf_only() {
        let msg = "BT-SEARCH * HTTP/1.1\n\
            Host: 239.192.152.143:6771\n\
            Port: 6881\n\
            Infohash: 000102030405060708090a0b0c0d0e0f10111213\n\
            \n";
        let ann = LsdAnnounce::decode(msg.as_bytes()).expect("decode failed");
        assert_eq!(ann.port, 6881);
        assert_eq!(ann.info_hashes, vec![sample_hash()]);
    }

    #[test]
    fn lenient_line_endings_mixed() {
        // Mix \r\n and \n in the same message.
        let msg = "BT-SEARCH * HTTP/1.1\r\n\
            Host: 239.192.152.143:6771\n\
            Port: 9999\r\n\
            Infohash: 000102030405060708090a0b0c0d0e0f10111213\n\
            \r\n";
        let ann = LsdAnnounce::decode(msg.as_bytes()).expect("decode failed");
        assert_eq!(ann.port, 9999);
    }

    #[test]
    fn invalid_port_error() {
        let msg = b"BT-SEARCH * HTTP/1.1\r\n\
            Port: notanumber\r\n\
            Infohash: 000102030405060708090a0b0c0d0e0f10111213\r\n\
            \r\n";
        let err = LsdAnnounce::decode(msg).unwrap_err();
        assert!(matches!(err, LsdError::InvalidPort(_)));
    }

    #[test]
    fn invalid_info_hash_error() {
        let msg = b"BT-SEARCH * HTTP/1.1\r\n\
            Port: 6881\r\n\
            Infohash: tooshort\r\n\
            \r\n";
        let err = LsdAnnounce::decode(msg).unwrap_err();
        assert!(matches!(err, LsdError::InvalidInfoHash(_)));
    }

    #[test]
    fn invalid_request_line() {
        let msg = b"GET / HTTP/1.1\r\nPort: 6881\r\nInfohash: 000102030405060708090a0b0c0d0e0f10111213\r\n\r\n";
        let err = LsdAnnounce::decode(msg).unwrap_err();
        assert!(matches!(err, LsdError::InvalidFormat(_)));
    }

    // -- Actor integration tests --------------------------------------------

    /// Two LSD services on loopback: one announces, the other discovers.
    ///
    /// Multicast on loopback is unreliable on some CI platforms (particularly
    /// macOS containers and some Linux configurations). This test is marked
    /// `#[ignore]` so it does not block CI; run it explicitly with
    /// `cargo test -p magpie-bt-core -- --ignored lsd_loopback_discovery`.
    #[tokio::test]
    #[ignore = "multicast on loopback may not work in all environments"]
    async fn lsd_loopback_discovery() {
        let token = CancellationToken::new();

        // -- Peer A: announcer -----------------------------------------------
        let config_a = LsdConfig::default();
        let (service_a, _rx_a) = LsdService::new(
            LsdConfig {
                announce_interval: Duration::from_secs(1), // fast for testing (clamped to MIN)
                ..config_a
            },
            6881,
            token.clone(),
        );
        // -- Peer B: listener ------------------------------------------------
        let config_b = LsdConfig::default();
        let (service_b, mut rx_b) = LsdService::new(
            LsdConfig {
                announce_interval: Duration::from_secs(300),
                ..config_b
            },
            6882,
            token.clone(),
        );

        let hash = sample_hash();

        // Register hash on A.
        service_a.register(hash, false);

        // Spawn both services.
        let handle_a = tokio::spawn(service_a.run());
        let handle_b = tokio::spawn(service_b.run());

        // Wait for B to discover A's announcement.
        let discovery = tokio::time::timeout(Duration::from_secs(30), rx_b.recv())
            .await
            .expect("timeout waiting for LSD discovery")
            .expect("discovery channel closed");

        assert_eq!(discovery.info_hash, hash);
        assert_eq!(discovery.addr.port(), 6881);

        // Shutdown.
        token.cancel();
        let _ = handle_a.await;
        let _ = handle_b.await;
    }

    #[test]
    fn too_many_info_hashes_rejected() {
        let mut msg = String::from("BT-SEARCH * HTTP/1.1\r\nPort: 6881\r\n");
        // Insert MAX_INFOHASHES_PER_MESSAGE + 1 info hashes.
        for i in 0..=MAX_INFOHASHES_PER_MESSAGE {
            let mut hash = [0u8; 20];
            #[allow(clippy::cast_possible_truncation)]
            {
                hash[0] = (i & 0xff) as u8;
                hash[1] = ((i >> 8) & 0xff) as u8;
            }
            let hex = hex_encode(&hash);
            let _ = write!(msg, "Infohash: {hex}\r\n");
        }
        msg.push_str("\r\n");
        let err = LsdAnnounce::decode(msg.as_bytes()).unwrap_err();
        assert!(
            matches!(err, LsdError::InvalidFormat(ref s) if s.contains("too many info hashes")),
            "expected InvalidFormat(too many info hashes), got {err:?}"
        );
    }

    #[tokio::test]
    async fn bind_multicast_reuse_addr() {
        // Binding twice to the same multicast port proves SO_REUSEADDR works.
        let addr = SocketAddrV4::new(LSD_MULTICAST_ADDR, LSD_PORT);
        let sock1 = bind_multicast(addr).expect("first bind failed");
        let sock2 = bind_multicast(addr).expect("second bind failed (SO_REUSEADDR missing?)");
        drop(sock1);
        drop(sock2);
    }

    /// Private torrents must not be announced.
    #[test]
    fn private_torrent_not_registered() {
        let token = CancellationToken::new();
        let (service, _rx) = LsdService::new(LsdConfig::default(), 6881, token);

        // Register as private — the command channel should receive nothing.
        service.register(sample_hash(), true);

        // Register a non-private hash — only this one should arrive.
        service.register(sample_hash_2(), false);

        // Destructure to get access to the channel internals.
        let LsdService { cmd_tx, mut cmd_rx, .. } = service;
        drop(cmd_tx);

        let mut count = 0;
        while cmd_rx.try_recv().is_ok() {
            count += 1;
        }
        // Only the non-private hash should have been sent.
        assert_eq!(count, 1);
    }
}
