//! HTTP(S) tracker transport built on `reqwest` + `rustls-tls`.
//!
//! BEP 3 is finicky about how the 20-byte binary `info_hash` and `peer_id` are
//! percent-encoded into the announce URL: every byte that is not in the
//! unreserved set `A-Za-z0-9.-_~` must be `%XX`-encoded. Most generic URL
//! encoders are lossy on binary input, so this module hand-rolls the
//! conversion in [`build_announce_url`].

use std::fmt::Write;
use std::time::Duration;

use futures_util::StreamExt;
use magpie_bt_bencode::{Value, decode};
use reqwest::Client;
use reqwest::redirect::Policy;

use crate::tracker::compact;
use crate::tracker::error::TrackerError;
use crate::tracker::{
    AnnounceFuture, AnnounceRequest, AnnounceResponse, ScrapeFile, ScrapeFuture, ScrapeResponse,
    Tracker, TrackerScrape,
};

/// Overall request budget (DNS + TLS + headers + body).
pub(super) const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);

/// Connect-only budget — defends against slow-loris trackers that accept the
/// TCP SYN but never finish the TLS handshake.
pub(super) const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// Hard cap on response body size. A tracker response containing 50 000 IPv4
/// peers is ≈300 kB; legitimate responses comfortably fit in 1 MiB.
/// 4 MiB leaves room for pathological-but-legal `peers6` mixes while
/// preventing OOM via attacker-controlled `Content-Length` (T1).
pub(super) const MAX_RESPONSE_BYTES: u64 = 4 * 1024 * 1024;

/// Maximum redirect chain we follow. HTTPS→HTTP downgrades are rejected
/// regardless of count (T4).
const MAX_REDIRECTS: usize = 3;

fn build_default_client() -> Result<Client, reqwest::Error> {
    Client::builder()
        .timeout(DEFAULT_TIMEOUT)
        .connect_timeout(DEFAULT_CONNECT_TIMEOUT)
        .redirect(Policy::custom(|attempt| {
            if attempt.previous().len() >= MAX_REDIRECTS {
                return attempt.error("too many redirects");
            }
            // Reject HTTPS→HTTP downgrades so a hostile tracker can't strip TLS.
            if let Some(prev) = attempt.previous().last()
                && prev.scheme() == "https"
                && attempt.url().scheme() == "http"
            {
                return attempt.error("HTTPS→HTTP redirect refused");
            }
            attempt.follow()
        }))
        .user_agent(concat!("magpie/", env!("CARGO_PKG_VERSION")))
        .build()
}

/// HTTP / HTTPS tracker client.
#[derive(Debug, Clone)]
pub struct HttpTracker {
    client: Client,
    base_url: String,
}

impl HttpTracker {
    /// Construct a tracker pinned to the given announce URL.
    ///
    /// # Errors
    ///
    /// Returns [`TrackerError::Transport`] if the underlying HTTP client
    /// cannot be built (typically a TLS configuration error).
    pub fn new(base_url: impl Into<String>) -> Result<Self, TrackerError> {
        let client = build_default_client().map_err(TrackerError::Transport)?;
        Ok(Self {
            client,
            base_url: base_url.into(),
        })
    }

    /// Construct with a caller-supplied [`reqwest::Client`].
    #[must_use]
    pub fn with_client(base_url: impl Into<String>, client: Client) -> Self {
        Self {
            client,
            base_url: base_url.into(),
        }
    }
}

impl Tracker for HttpTracker {
    fn announce<'a>(&'a self, req: AnnounceRequest<'a>) -> AnnounceFuture<'a> {
        Box::pin(async move {
            let url = build_announce_url(&self.base_url, &req)?;
            let response = self
                .client
                .get(&url)
                .send()
                .await
                .map_err(TrackerError::Transport)?
                .error_for_status()
                .map_err(TrackerError::Transport)?;
            let bytes = read_bounded_body(response, MAX_RESPONSE_BYTES).await?;
            parse_announce_response(&bytes)
        })
    }
}

impl TrackerScrape for HttpTracker {
    fn scrape<'a>(&'a self, info_hashes: &'a [[u8; 20]]) -> ScrapeFuture<'a> {
        Box::pin(async move {
            let url = build_scrape_url(&self.base_url, info_hashes)?;
            let response = self
                .client
                .get(&url)
                .send()
                .await
                .map_err(TrackerError::Transport)?
                .error_for_status()
                .map_err(TrackerError::Transport)?;
            let bytes = read_bounded_body(response, MAX_RESPONSE_BYTES).await?;
            parse_scrape_response(&bytes)
        })
    }
}

async fn read_bounded_body(response: reqwest::Response, cap: u64) -> Result<Vec<u8>, TrackerError> {
    if let Some(len) = response.content_length()
        && len > cap
    {
        return Err(TrackerError::MalformedResponse(format!(
            "response body length {len} exceeds cap {cap}"
        )));
    }
    let mut acc = Vec::with_capacity(
        usize::try_from(response.content_length().unwrap_or(0).min(cap)).unwrap_or(0),
    );
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(TrackerError::Transport)?;
        if (acc.len() as u64).saturating_add(chunk.len() as u64) > cap {
            return Err(TrackerError::MalformedResponse(format!(
                "response body exceeded cap {cap}"
            )));
        }
        acc.extend_from_slice(&chunk);
    }
    Ok(acc)
}

/// Compose the final announce URL for `req`, applying BEP 3 percent-encoding
/// to the binary `info_hash` and `peer_id`.
///
/// # Errors
///
/// Returns [`TrackerError::InvalidUrl`] if `base_url` is empty.
pub fn build_announce_url(
    base_url: &str,
    req: &AnnounceRequest<'_>,
) -> Result<String, TrackerError> {
    if base_url.is_empty() {
        return Err(TrackerError::InvalidUrl("empty announce URL".into()));
    }
    let separator = if base_url.contains('?') { '&' } else { '?' };
    let mut url = String::with_capacity(base_url.len() + 256);
    url.push_str(base_url);
    url.push(separator);
    url.push_str("info_hash=");
    encode_binary(&mut url, &req.info_hash);
    url.push_str("&peer_id=");
    encode_binary(&mut url, &req.peer_id);
    write!(url, "&port={}", req.port).expect("infallible");
    write!(url, "&uploaded={}", req.uploaded).expect("infallible");
    write!(url, "&downloaded={}", req.downloaded).expect("infallible");
    write!(url, "&left={}", req.left).expect("infallible");
    write!(url, "&compact={}", u8::from(req.compact)).expect("infallible");
    if let Some(n) = req.num_want {
        write!(url, "&numwant={n}").expect("infallible");
    }
    if let Some(ev) = req.event.as_str() {
        write!(url, "&event={ev}").expect("infallible");
    }
    if let Some(tid) = req.tracker_id {
        url.push_str("&trackerid=");
        encode_binary(&mut url, tid);
    }
    Ok(url)
}

fn encode_binary(out: &mut String, bytes: &[u8]) {
    for &b in bytes {
        if matches!(b, b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'.' | b'-' | b'_' | b'~') {
            out.push(b as char);
        } else {
            out.push('%');
            out.push(hex_nibble(b >> 4));
            out.push(hex_nibble(b & 0x0F));
        }
    }
}

const fn hex_nibble(n: u8) -> char {
    match n {
        0..=9 => (b'0' + n) as char,
        10..=15 => (b'A' + (n - 10)) as char,
        _ => '?',
    }
}

/// Parse a raw bencoded announce response. Public so consumers can drive a
/// custom HTTP transport (or test) and reuse the response decoder.
///
/// # Errors
///
/// Returns [`TrackerError::MalformedResponse`] for any structural failure,
/// [`TrackerError::Failure`] when the tracker reports `failure reason`, and
/// [`TrackerError::CompactPeersTruncated`] for malformed peer lists.
pub fn parse_response(bytes: &[u8]) -> Result<AnnounceResponse, TrackerError> {
    parse_announce_response(bytes)
}

/// Compose a BEP 48 scrape URL from an announce URL by rewriting the
/// last path segment `announce` → `scrape`, then appending
/// `info_hash=<binary>` query parameters (percent-encoded).
///
/// # Errors
///
/// [`TrackerError::InvalidUrl`] if `base_url` is empty, has no
/// `announce` path segment (tracker doesn't advertise scrape per BEP
/// 48 §Spec), or is otherwise malformed.
pub fn build_scrape_url(
    base_url: &str,
    info_hashes: &[[u8; 20]],
) -> Result<String, TrackerError> {
    if base_url.is_empty() {
        return Err(TrackerError::InvalidUrl("empty tracker URL".into()));
    }
    if info_hashes.is_empty() {
        return Err(TrackerError::InvalidUrl(
            "scrape request has zero info_hashes".into(),
        ));
    }
    // Split off the query string if present, rewrite the path
    // component, then reattach.
    let (path_url, query) = base_url.find('?').map_or((base_url, None), |i| {
        (&base_url[..i], Some(&base_url[i + 1..]))
    });
    // The rewrite targets the last path segment. Accept common
    // endings: `/announce`, `/announce.php`, etc. — per BEP 48 the
    // scrape URL is formed "by replacing the last occurrence of
    // 'announce' with 'scrape'".
    let scrape_path = match path_url.rfind("announce") {
        Some(pos) => {
            let mut s = String::with_capacity(path_url.len() + 1);
            s.push_str(&path_url[..pos]);
            s.push_str("scrape");
            s.push_str(&path_url[pos + "announce".len()..]);
            s
        }
        None => {
            return Err(TrackerError::InvalidUrl(
                "announce URL has no 'announce' segment — tracker does not support BEP 48 scrape"
                    .into(),
            ));
        }
    };

    let mut url = scrape_path;
    // Preserve any pre-existing query (tracker-specific keys).
    if let Some(q) = query {
        url.push('?');
        url.push_str(q);
    }
    let mut first = query.is_none();
    for hash in info_hashes {
        url.push(if first { '?' } else { '&' });
        first = false;
        url.push_str("info_hash=");
        encode_binary(&mut url, hash);
    }
    Ok(url)
}

/// Parse a BEP 48 scrape response body.
///
/// # Errors
///
/// [`TrackerError::MalformedResponse`] on structural failure or
/// missing required fields. [`TrackerError::Failure`] when the
/// tracker returned a `failure reason` field.
pub fn parse_scrape_response(bytes: &[u8]) -> Result<ScrapeResponse, TrackerError> {
    let value = decode(bytes).map_err(|e| TrackerError::MalformedResponse(e.to_string()))?;
    let dict = value
        .as_dict()
        .ok_or_else(|| TrackerError::MalformedResponse("response is not a dict".into()))?;

    if let Some(reason) = dict.get(&b"failure reason"[..]).and_then(Value::as_bytes) {
        return Err(TrackerError::Failure(
            String::from_utf8_lossy(reason).into_owned(),
        ));
    }

    let files_val = dict.get(&b"files"[..]).ok_or_else(|| {
        TrackerError::MalformedResponse("scrape response missing 'files'".into())
    })?;
    let files_dict = files_val
        .as_dict()
        .ok_or_else(|| TrackerError::MalformedResponse("'files' is not a dict".into()))?;

    let mut files: std::collections::HashMap<[u8; 20], ScrapeFile> =
        std::collections::HashMap::with_capacity(files_dict.len());
    for (key, entry) in files_dict {
        if key.len() != 20 {
            return Err(TrackerError::MalformedResponse(format!(
                "scrape 'files' key has len {} (want 20)",
                key.len()
            )));
        }
        let entry_dict = entry
            .as_dict()
            .ok_or_else(|| TrackerError::MalformedResponse("'files' value is not a dict".into()))?;
        let complete = entry_dict
            .get(&b"complete"[..])
            .and_then(Value::as_int)
            .and_then(|i| u64::try_from(i).ok())
            .ok_or_else(|| {
                TrackerError::MalformedResponse("scrape file missing 'complete'".into())
            })?;
        let incomplete = entry_dict
            .get(&b"incomplete"[..])
            .and_then(Value::as_int)
            .and_then(|i| u64::try_from(i).ok())
            .ok_or_else(|| {
                TrackerError::MalformedResponse("scrape file missing 'incomplete'".into())
            })?;
        let downloaded = entry_dict
            .get(&b"downloaded"[..])
            .and_then(Value::as_int)
            .and_then(|i| u64::try_from(i).ok())
            .ok_or_else(|| {
                TrackerError::MalformedResponse("scrape file missing 'downloaded'".into())
            })?;
        let name = entry_dict
            .get(&b"name"[..])
            .and_then(Value::as_bytes)
            .map(|b| String::from_utf8_lossy(b).into_owned());
        let mut info_hash = [0u8; 20];
        info_hash.copy_from_slice(key);
        files.insert(
            info_hash,
            ScrapeFile {
                complete,
                incomplete,
                downloaded,
                name,
            },
        );
    }

    Ok(ScrapeResponse {
        files,
        failure_reason: None,
    })
}

fn parse_announce_response(bytes: &[u8]) -> Result<AnnounceResponse, TrackerError> {
    let value = decode(bytes).map_err(|e| TrackerError::MalformedResponse(e.to_string()))?;
    let dict = value
        .as_dict()
        .ok_or_else(|| TrackerError::MalformedResponse("response is not a dict".into()))?;

    if let Some(reason) = dict.get(&b"failure reason"[..]).and_then(Value::as_bytes) {
        return Err(TrackerError::Failure(
            String::from_utf8_lossy(reason).into_owned(),
        ));
    }

    let interval_secs = dict
        .get(&b"interval"[..])
        .and_then(Value::as_int)
        .ok_or_else(|| TrackerError::MalformedResponse("missing 'interval'".into()))?;
    // T2: reject non-positive intervals — a hostile tracker can otherwise drive
    // a re-announce hot loop and amplify our outbound bandwidth against itself.
    if interval_secs <= 0 {
        return Err(TrackerError::MalformedResponse(format!(
            "'interval' must be positive, got {interval_secs}"
        )));
    }
    let interval = Duration::from_secs(interval_secs.cast_unsigned());
    let min_interval = match dict.get(&b"min interval"[..]).and_then(Value::as_int) {
        Some(n) if n <= 0 => {
            return Err(TrackerError::MalformedResponse(format!(
                "'min interval' must be positive, got {n}"
            )));
        }
        Some(n) => Some(Duration::from_secs(n.cast_unsigned())),
        None => None,
    };
    let tracker_id = dict
        .get(&b"tracker id"[..])
        .and_then(Value::as_bytes)
        .map(<[u8]>::to_vec);
    let complete = dict
        .get(&b"complete"[..])
        .and_then(Value::as_int)
        .and_then(|n| u32::try_from(n).ok());
    let incomplete = dict
        .get(&b"incomplete"[..])
        .and_then(Value::as_int)
        .and_then(|n| u32::try_from(n).ok());
    let warning = dict
        .get(&b"warning message"[..])
        .and_then(Value::as_bytes)
        .map(|b| String::from_utf8_lossy(b).into_owned());

    let mut peers = Vec::new();
    if let Some(p) = dict.get(&b"peers"[..]) {
        match p {
            Value::Bytes(b) => peers.extend(compact::decode_v4(b)?),
            Value::List(items) => peers.extend(parse_dict_peers(items)?),
            _ => {
                return Err(TrackerError::MalformedResponse(
                    "'peers' is not bytes or list".into(),
                ));
            }
        }
    }
    if let Some(Value::Bytes(b)) = dict.get(&b"peers6"[..]) {
        peers.extend(compact::decode_v6(b)?);
    }

    Ok(AnnounceResponse {
        interval,
        min_interval,
        peers,
        tracker_id,
        complete,
        incomplete,
        warning,
    })
}

fn parse_dict_peers(items: &[Value<'_>]) -> Result<Vec<std::net::SocketAddr>, TrackerError> {
    use std::net::{IpAddr, SocketAddr};
    let mut out = Vec::with_capacity(items.len());
    for item in items {
        let d = item.as_dict().ok_or_else(|| {
            TrackerError::MalformedResponse("dict-peer entry is not a dict".into())
        })?;
        let ip_bytes = d
            .get(&b"ip"[..])
            .and_then(Value::as_bytes)
            .ok_or_else(|| TrackerError::MalformedResponse("dict-peer missing 'ip'".into()))?;
        let port_int = d
            .get(&b"port"[..])
            .and_then(Value::as_int)
            .ok_or_else(|| TrackerError::MalformedResponse("dict-peer missing 'port'".into()))?;
        let port = u16::try_from(port_int)
            .map_err(|_| TrackerError::MalformedResponse("dict-peer port out of range".into()))?;
        let s = std::str::from_utf8(ip_bytes)
            .map_err(|_| TrackerError::MalformedResponse("dict-peer 'ip' is not UTF-8".into()))?;
        let ip: IpAddr = s
            .parse()
            .map_err(|_| TrackerError::MalformedResponse(format!("dict-peer 'ip' invalid: {s}")))?;
        if port != 0 {
            out.push(SocketAddr::new(ip, port));
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tracker::AnnounceEvent;

    fn req() -> AnnounceRequest<'static> {
        AnnounceRequest {
            info_hash: [0xAA; 20],
            peer_id: *b"-Mg0001-abcdefghijkl",
            port: 6881,
            uploaded: 0,
            downloaded: 0,
            left: 1024,
            event: AnnounceEvent::Started,
            num_want: Some(50),
            compact: true,
            tracker_id: None,
        }
    }

    #[test]
    fn url_encodes_binary_hash_byte_for_byte() {
        let url = build_announce_url("http://t.example/announce", &req()).unwrap();
        // Each 0xAA byte must appear as `%AA`; 20 of them in info_hash:
        let info_hash_segment = "%AA".repeat(20);
        assert!(url.contains(&format!("info_hash={info_hash_segment}")));
        // Peer-id contains ASCII unreserved chars + leading `-`; verify a sample.
        assert!(url.contains("peer_id=-Mg0001-abcdefghijkl"));
        assert!(url.contains("&port=6881"));
        assert!(url.contains("&compact=1"));
        assert!(url.contains("&numwant=50"));
        assert!(url.contains("&event=started"));
    }

    #[test]
    fn url_uses_ampersand_when_base_has_query() {
        let url = build_announce_url("http://t.example/announce?passkey=xyz", &req()).unwrap();
        assert!(url.starts_with("http://t.example/announce?passkey=xyz&info_hash="));
    }

    #[test]
    fn url_rejects_empty_base() {
        let err = build_announce_url("", &req()).unwrap_err();
        assert!(matches!(err, TrackerError::InvalidUrl(_)));
    }

    fn parse_for_test(bytes: &[u8]) -> Result<AnnounceResponse, TrackerError> {
        parse_announce_response(bytes)
    }

    #[test]
    fn parse_compact_v4_response() {
        // d8:completei5e10:incompletei2e8:intervali1800e5:peers12: ... e
        let mut payload = Vec::new();
        payload.extend_from_slice(b"d8:completei5e10:incompletei2e8:intervali1800e5:peers12:");
        payload.extend_from_slice(&[10, 0, 0, 1, 0x1A, 0xE1, 192, 168, 1, 2, 0xC0, 0x35]);
        payload.push(b'e');
        let resp = parse_for_test(&payload).unwrap();
        assert_eq!(resp.interval, Duration::from_secs(1800));
        assert_eq!(resp.complete, Some(5));
        assert_eq!(resp.incomplete, Some(2));
        assert_eq!(resp.peers.len(), 2);
    }

    #[test]
    fn rejects_zero_interval() {
        let payload = b"d8:intervali0e5:peers0:e";
        let err = parse_for_test(payload).unwrap_err();
        assert!(matches!(err, TrackerError::MalformedResponse(s) if s.contains("'interval'")));
    }

    #[test]
    fn rejects_negative_interval() {
        let payload = b"d8:intervali-5e5:peers0:e";
        let err = parse_for_test(payload).unwrap_err();
        assert!(matches!(err, TrackerError::MalformedResponse(s) if s.contains("'interval'")));
    }

    #[test]
    fn rejects_zero_min_interval() {
        let payload = b"d8:intervali900e12:min intervali0e5:peers0:e";
        let err = parse_for_test(payload).unwrap_err();
        assert!(matches!(err, TrackerError::MalformedResponse(s) if s.contains("'min interval'")));
    }

    #[test]
    fn parse_failure_reason() {
        let payload = b"d14:failure reason12:tracker downe";
        let err = parse_for_test(payload).unwrap_err();
        assert!(matches!(err, TrackerError::Failure(s) if s == "tracker down"));
    }

    #[test]
    fn parse_dict_peers() {
        let payload = b"d8:intervali900e5:peersld2:ip9:127.0.0.14:porti6881eeee";
        let resp = parse_for_test(payload).unwrap();
        assert_eq!(resp.peers.len(), 1);
        assert_eq!(resp.peers[0].to_string(), "127.0.0.1:6881");
    }

    // ----- BEP 48 scrape -----

    #[test]
    fn build_scrape_url_rewrites_announce_segment() {
        let url = build_scrape_url("http://tr.example/announce", &[[0xAA; 20]]).unwrap();
        assert!(url.starts_with("http://tr.example/scrape?info_hash="));
    }

    #[test]
    fn build_scrape_url_preserves_query_string() {
        let url = build_scrape_url(
            "http://tr.example/announce?passkey=secret",
            &[[0xAA; 20]],
        )
        .unwrap();
        assert!(
            url.starts_with("http://tr.example/scrape?passkey=secret&info_hash="),
            "url = {url}"
        );
    }

    #[test]
    fn build_scrape_url_rejects_url_without_announce_segment() {
        let err = build_scrape_url("http://tr.example/custompath", &[[0xAA; 20]]).unwrap_err();
        assert!(matches!(err, TrackerError::InvalidUrl(_)));
    }

    #[test]
    fn build_scrape_url_encodes_multiple_hashes() {
        let hashes = &[[0xAA; 20], [0xBB; 20], [0xCC; 20]];
        let url = build_scrape_url("http://tr.example/announce", hashes).unwrap();
        // Three info_hash params: first after `?`, other two after `&`.
        assert_eq!(url.matches("info_hash=").count(), 3);
    }

    #[test]
    fn build_scrape_url_rejects_empty_base() {
        let err = build_scrape_url("", &[[0xAA; 20]]).unwrap_err();
        assert!(matches!(err, TrackerError::InvalidUrl(_)));
    }

    #[test]
    fn build_scrape_url_rejects_empty_hashes() {
        let err = build_scrape_url("http://tr.example/announce", &[]).unwrap_err();
        assert!(matches!(err, TrackerError::InvalidUrl(_)));
    }

    #[test]
    fn parse_scrape_response_basic() {
        // d5:filesd20:<hash>d8:completei10e10:downloadedi50e10:incompletei5eeee
        let mut payload = Vec::new();
        payload.extend_from_slice(b"d5:filesd20:");
        payload.extend_from_slice(&[0xAAu8; 20]);
        payload.extend_from_slice(b"d8:completei10e10:downloadedi50e10:incompletei5eeee");
        let resp = parse_scrape_response(&payload).unwrap();
        assert_eq!(resp.files.len(), 1);
        let entry = &resp.files[&[0xAA; 20]];
        assert_eq!(entry.complete, 10);
        assert_eq!(entry.incomplete, 5);
        assert_eq!(entry.downloaded, 50);
        assert!(entry.name.is_none());
    }

    #[test]
    fn parse_scrape_response_with_name() {
        // d5:files d 20:<hash> d 8:completei1e 10:downloadedi2e 10:incompletei3e 4:name6:ubuntu e e e
        let mut payload = Vec::new();
        payload.extend_from_slice(b"d5:filesd20:");
        payload.extend_from_slice(&[0xABu8; 20]);
        payload.extend_from_slice(
            b"d8:completei1e10:downloadedi2e10:incompletei3e4:name6:ubuntuee",
        );
        payload.extend_from_slice(b"e"); // close outer response dict
        let resp = parse_scrape_response(&payload).unwrap();
        let entry = &resp.files[&[0xAB; 20]];
        assert_eq!(entry.name.as_deref(), Some("ubuntu"));
    }

    #[test]
    fn parse_scrape_response_surfaces_failure() {
        let payload = b"d14:failure reason17:torrent not founde";
        let err = parse_scrape_response(payload).unwrap_err();
        assert!(matches!(err, TrackerError::Failure(ref s) if s == "torrent not found"));
    }

    #[test]
    fn parse_scrape_response_rejects_missing_files() {
        let payload = b"de"; // empty dict
        let err = parse_scrape_response(payload).unwrap_err();
        assert!(matches!(err, TrackerError::MalformedResponse(_)));
    }

    #[test]
    fn parse_scrape_response_rejects_bad_key_length() {
        // files key is 19 bytes, not 20
        let mut payload = Vec::new();
        payload.extend_from_slice(b"d5:filesd19:");
        payload.extend_from_slice(&[0xAAu8; 19]);
        payload.extend_from_slice(b"d8:completei0e10:downloadedi0e10:incompletei0eeeee");
        let err = parse_scrape_response(&payload).unwrap_err();
        assert!(matches!(err, TrackerError::MalformedResponse(_)));
    }

    #[test]
    fn parse_scrape_response_rejects_missing_counter_fields() {
        // Missing `incomplete`
        let mut payload = Vec::new();
        payload.extend_from_slice(b"d5:filesd20:");
        payload.extend_from_slice(&[0xAAu8; 20]);
        payload.extend_from_slice(b"d8:completei1e10:downloadedi2eeee");
        let err = parse_scrape_response(&payload).unwrap_err();
        assert!(matches!(err, TrackerError::MalformedResponse(_)));
    }
}
