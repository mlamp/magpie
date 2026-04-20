//! `announce_peer` tokens — stateless SHA-1 HMAC with dual-secret
//! rotation, per ADR-0026.
//!
//! A DHT peer sends `get_peers(info_hash)`; our reply carries a
//! token that binds the peer's IP to a short-lived secret. When the
//! peer later sends `announce_peer(…, token)` the handler checks
//! the token against the current + previous secret. This gives a
//! 30-minute validity window (two 15-min rotations) without any
//! per-peer bookkeeping — restart loses the window and peers
//! transparently re-`get_peers`.

use std::net::IpAddr;
use std::time::{Duration, Instant};

use sha1::{Digest, Sha1};

// ---------------------------------------------------------------------------
// Constants (ADR-0026)
// ---------------------------------------------------------------------------

/// Rotation interval between `current` → `previous` secret swaps.
pub const TOKEN_ROTATION_INTERVAL: Duration = Duration::from_secs(15 * 60);

/// Bytes of token material placed on the wire (first 8 bytes of the
/// SHA-1 digest).
pub const TOKEN_LENGTH: usize = 8;

/// Per-secret byte length (SHA-1 block input seed).
const SECRET_LENGTH: usize = 20;

// ---------------------------------------------------------------------------
// TokenSecrets
// ---------------------------------------------------------------------------

/// Dual-secret factory for `announce_peer` tokens.
///
/// Construct with [`TokenSecrets::new`], rotate on the
/// [`TOKEN_ROTATION_INTERVAL`] cadence via [`TokenSecrets::rotate`],
/// mint tokens via [`TokenSecrets::make_token`] and verify inbound
/// tokens via [`TokenSecrets::validate`].
#[derive(Debug, Clone)]
pub struct TokenSecrets {
    current: [u8; SECRET_LENGTH],
    previous: Option<[u8; SECRET_LENGTH]>,
    rotated_at: Instant,
}

impl TokenSecrets {
    /// Initialise with a fresh random `current` secret and no
    /// `previous`. `now` is recorded as the rotation baseline.
    ///
    /// # Errors
    ///
    /// Propagates any failure from [`getrandom::fill`]. Platforms
    /// where the CSPRNG is unavailable at DHT boot must handle this;
    /// the DHT refuses to run without secrets.
    pub fn new(now: Instant) -> Result<Self, getrandom::Error> {
        let mut current = [0u8; SECRET_LENGTH];
        getrandom::fill(&mut current)?;
        Ok(Self {
            current,
            previous: None,
            rotated_at: now,
        })
    }

    /// Rotate: `previous = current; current = random()`. The caller
    /// should invoke this from a scheduler at
    /// [`TOKEN_ROTATION_INTERVAL`] cadence.
    ///
    /// # Errors
    ///
    /// Propagates any failure from [`getrandom::fill`].
    pub fn rotate(&mut self, now: Instant) -> Result<(), getrandom::Error> {
        let mut fresh = [0u8; SECRET_LENGTH];
        getrandom::fill(&mut fresh)?;
        self.previous = Some(self.current);
        self.current = fresh;
        self.rotated_at = now;
        Ok(())
    }

    /// True when `now − rotated_at ≥ interval`. Scheduler convenience.
    #[must_use]
    pub fn needs_rotation(&self, now: Instant, interval: Duration) -> bool {
        now.saturating_duration_since(self.rotated_at) >= interval
    }

    /// Instant the current secret was minted.
    #[must_use]
    pub const fn rotated_at(&self) -> Instant {
        self.rotated_at
    }

    /// Compute the token to issue to `peer_ip` against the current
    /// secret.
    #[must_use]
    pub fn make_token(&self, peer_ip: IpAddr) -> [u8; TOKEN_LENGTH] {
        compute_token(&self.current, peer_ip)
    }

    /// Validate an inbound token: accepts either the current or the
    /// previous secret's digest for `peer_ip`.
    #[must_use]
    pub fn validate(&self, token: &[u8], peer_ip: IpAddr) -> bool {
        if token.len() != TOKEN_LENGTH {
            return false;
        }
        let current = compute_token(&self.current, peer_ip);
        if constant_time_eq(token, &current) {
            return true;
        }
        if let Some(prev) = &self.previous {
            let previous = compute_token(prev, peer_ip);
            if constant_time_eq(token, &previous) {
                return true;
            }
        }
        false
    }
}

fn compute_token(secret: &[u8; SECRET_LENGTH], ip: IpAddr) -> [u8; TOKEN_LENGTH] {
    let mut hasher = Sha1::new();
    hasher.update(secret);
    match ip {
        IpAddr::V4(v4) => hasher.update(v4.octets()),
        IpAddr::V6(v6) => hasher.update(v6.octets()),
    }
    let digest = hasher.finalize();
    let mut out = [0u8; TOKEN_LENGTH];
    out.copy_from_slice(&digest[..TOKEN_LENGTH]);
    out
}

/// Constant-time byte comparison — defence against timing-side-
/// channel extraction of the secret from `validate` latency.
/// Short-circuits on length mismatch *before* the compare so token
/// length itself is not a secret (it's a protocol constant).
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{Ipv4Addr, Ipv6Addr};

    fn v4(a: u8, b: u8, c: u8, d: u8) -> IpAddr {
        IpAddr::V4(Ipv4Addr::new(a, b, c, d))
    }

    #[test]
    fn token_round_trips_within_current_secret() {
        let now = Instant::now();
        let secrets = TokenSecrets::new(now).unwrap();
        let ip = v4(203, 0, 113, 42);
        let token = secrets.make_token(ip);
        assert!(secrets.validate(&token, ip));
    }

    #[test]
    fn token_valid_across_one_rotation() {
        let t0 = Instant::now();
        let mut secrets = TokenSecrets::new(t0).unwrap();
        let ip = v4(203, 0, 113, 42);
        let token = secrets.make_token(ip);
        secrets.rotate(t0 + TOKEN_ROTATION_INTERVAL).unwrap();
        assert!(
            secrets.validate(&token, ip),
            "previous-secret token must still validate"
        );
    }

    #[test]
    fn token_invalid_after_two_rotations() {
        let t0 = Instant::now();
        let mut secrets = TokenSecrets::new(t0).unwrap();
        let ip = v4(203, 0, 113, 42);
        let token = secrets.make_token(ip);
        secrets.rotate(t0 + TOKEN_ROTATION_INTERVAL).unwrap();
        secrets.rotate(t0 + TOKEN_ROTATION_INTERVAL * 2).unwrap();
        assert!(
            !secrets.validate(&token, ip),
            "token more than one rotation old must not validate"
        );
    }

    #[test]
    fn token_invalid_for_different_ip() {
        let secrets = TokenSecrets::new(Instant::now()).unwrap();
        let ip_a = v4(203, 0, 113, 42);
        let ip_b = v4(198, 51, 100, 7);
        let token = secrets.make_token(ip_a);
        assert!(!secrets.validate(&token, ip_b));
    }

    #[test]
    fn token_round_trips_for_ipv6() {
        let secrets = TokenSecrets::new(Instant::now()).unwrap();
        let ip = IpAddr::V6(Ipv6Addr::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, 1));
        let token = secrets.make_token(ip);
        assert!(secrets.validate(&token, ip));
    }

    #[test]
    fn wrong_length_token_rejected() {
        let secrets = TokenSecrets::new(Instant::now()).unwrap();
        let ip = v4(203, 0, 113, 42);
        assert!(!secrets.validate(&[], ip));
        assert!(!secrets.validate(&[0; TOKEN_LENGTH - 1], ip));
        assert!(!secrets.validate(&[0; TOKEN_LENGTH + 1], ip));
    }

    #[test]
    fn needs_rotation_fires_at_interval() {
        let t0 = Instant::now();
        let secrets = TokenSecrets::new(t0).unwrap();
        assert!(!secrets.needs_rotation(t0 + Duration::from_secs(60), TOKEN_ROTATION_INTERVAL));
        assert!(secrets.needs_rotation(
            t0 + TOKEN_ROTATION_INTERVAL + Duration::from_secs(1),
            TOKEN_ROTATION_INTERVAL,
        ));
    }

    #[test]
    fn forged_token_with_wrong_secret_rejected() {
        let secrets = TokenSecrets::new(Instant::now()).unwrap();
        let ip = v4(203, 0, 113, 42);
        let forged = [0x00u8; TOKEN_LENGTH];
        assert!(!secrets.validate(&forged, ip));
    }

    #[test]
    fn rotate_preserves_previous_current_swap() {
        let t0 = Instant::now();
        let mut secrets = TokenSecrets::new(t0).unwrap();
        let original = secrets.current;
        secrets.rotate(t0 + TOKEN_ROTATION_INTERVAL).unwrap();
        assert_eq!(secrets.previous, Some(original));
        assert_ne!(secrets.current, original);
    }
}
