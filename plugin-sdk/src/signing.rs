//! Request signing. CyclAI signs every request to a plugin with HMAC-SHA256
//! over `timestamp \n METHOD \n path-and-query \n sha256(body)`, using a
//! secret that only CyclAI and that plugin know, so a plugin only answers
//! CyclAI, and a plugin that leaks cannot be used to call another one.

use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};
use std::fmt;

type HmacSha256 = Hmac<Sha256>;

/// Requests older (or newer) than this are refused, which bounds replays.
pub const MAX_SKEW_SECONDS: i64 = 60;
/// A shorter secret is refused on both sides.
pub const MIN_SECRET_LEN: usize = 16;

#[derive(Debug, PartialEq, Eq)]
pub enum AuthError {
    MissingHeaders,
    BadTimestamp,
    Expired,
    BadSignature,
}

impl fmt::Display for AuthError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            AuthError::MissingHeaders => "missing signature headers",
            AuthError::BadTimestamp => "invalid timestamp",
            AuthError::Expired => "request too old or too far in the future",
            AuthError::BadSignature => "invalid signature",
        })
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn unhex(s: &str) -> Option<Vec<u8>> {
    if s.len() % 2 != 0 || !s.is_ascii() {
        return None;
    }
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).ok()).collect()
}

fn mac(secret: &[u8], timestamp: i64, method: &str, path_and_query: &str, body: &[u8]) -> HmacSha256 {
    let mut mac = HmacSha256::new_from_slice(secret).expect("HMAC accepts any key length");
    mac.update(format!("{timestamp}\n{}\n{path_and_query}\n{}", method.to_uppercase(), hex(&Sha256::digest(body))).as_bytes());
    mac
}

/// Hex signature of a request.
pub fn sign(secret: &[u8], timestamp: i64, method: &str, path_and_query: &str, body: &[u8]) -> String {
    hex(&mac(secret, timestamp, method, path_and_query, body).finalize().into_bytes())
}

/// Checks the headers of a received request (constant-time comparison).
pub fn verify(
    secret: &[u8],
    now: i64,
    timestamp_header: Option<&str>,
    signature_header: Option<&str>,
    method: &str,
    path_and_query: &str,
    body: &[u8],
) -> Result<(), AuthError> {
    let (Some(ts), Some(sig)) = (timestamp_header, signature_header) else {
        return Err(AuthError::MissingHeaders);
    };
    let timestamp: i64 = ts.trim().parse().map_err(|_| AuthError::BadTimestamp)?;
    if (now - timestamp).abs() > MAX_SKEW_SECONDS {
        return Err(AuthError::Expired);
    }
    let expected = unhex(sig.trim()).ok_or(AuthError::BadSignature)?;
    mac(secret, timestamp, method, path_and_query, body).verify_slice(&expected).map_err(|_| AuthError::BadSignature)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SECRET: &[u8] = b"0123456789abcdef-secret";

    #[test]
    fn a_signed_request_verifies_and_any_change_breaks_it() {
        let sig = sign(SECRET, 1_000, "get", "/activities?after=5", b"");
        let ok = |now, ts: &str, sig: &str, m: &str, p: &str, b: &[u8]| verify(SECRET, now, Some(ts), Some(sig), m, p, b);
        assert_eq!(ok(1_010, "1000", &sig, "GET", "/activities?after=5", b""), Ok(()), "method case does not matter");
        assert_eq!(ok(1_010, "1000", &sig, "GET", "/activities?after=6", b""), Err(AuthError::BadSignature), "query is signed");
        assert_eq!(ok(1_010, "1000", &sig, "POST", "/activities?after=5", b""), Err(AuthError::BadSignature), "method is signed");
        assert_eq!(ok(1_010, "1000", &sig, "GET", "/activities?after=5", b"x"), Err(AuthError::BadSignature), "body is signed");
        assert_eq!(ok(1_010, "1001", &sig, "GET", "/activities?after=5", b""), Err(AuthError::BadSignature), "timestamp is signed");
        assert_eq!(ok(1_010, "1000", "zz", "GET", "/activities?after=5", b""), Err(AuthError::BadSignature));
        let other = sign(b"another-secret-value", 1_000, "GET", "/activities?after=5", b"");
        assert_eq!(ok(1_010, "1000", &other, "GET", "/activities?after=5", b""), Err(AuthError::BadSignature), "other secret");
    }

    #[test]
    fn stale_future_and_missing_credentials_are_refused() {
        let sig = sign(SECRET, 1_000, "GET", "/manifest", b"");
        let at = |now| verify(SECRET, now, Some("1000"), Some(&sig), "GET", "/manifest", b"");
        assert_eq!(at(1_000 + MAX_SKEW_SECONDS), Ok(()));
        assert_eq!(at(1_000 + MAX_SKEW_SECONDS + 1), Err(AuthError::Expired));
        assert_eq!(at(1_000 - MAX_SKEW_SECONDS - 1), Err(AuthError::Expired));
        assert_eq!(verify(SECRET, 1_000, None, Some(&sig), "GET", "/manifest", b""), Err(AuthError::MissingHeaders));
        assert_eq!(verify(SECRET, 1_000, Some("1000"), None, "GET", "/manifest", b""), Err(AuthError::MissingHeaders));
        assert_eq!(verify(SECRET, 1_000, Some("abc"), Some(&sig), "GET", "/manifest", b""), Err(AuthError::BadTimestamp));
    }
}
