//! The side CyclAI uses to call a plugin.

use crate::{sign, Activity, Fetched, ListResponse, ListedActivity, Manifest, FIT_CONTENT_TYPE, HEADER_SIGNATURE, HEADER_TIMESTAMP, MIN_SECRET_LEN, PROTOCOL_VERSION};
use crate::{MAX_ACTIVITY_BYTES, MAX_LIST_BYTES, MAX_MANIFEST_BYTES};
use std::fmt;
use std::time::Duration;

#[derive(Debug)]
pub enum ClientError {
    /// Missing or unusable URL / secret.
    Config(String),
    /// The plugin could not be reached.
    Unreachable(String),
    /// The plugin answered with an error status.
    Rejected { status: u16, message: String, retry_after_s: Option<u64> },
    /// The plugin answered something that is not the protocol.
    Invalid(String),
}

impl fmt::Display for ClientError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ClientError::Config(m) | ClientError::Unreachable(m) | ClientError::Invalid(m) => f.write_str(m),
            ClientError::Rejected { status, message, .. } => write!(f, "{message} (HTTP {status})"),
        }
    }
}

impl std::error::Error for ClientError {}

#[derive(Clone)]
pub struct Client {
    http: reqwest::Client,
    base: String,
    secret: Vec<u8>,
}

/// Percent-encodes what is not safe in a path segment.
fn encode_segment(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => (b as char).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}

impl Client {
    pub fn new(base_url: &str, secret: &str) -> Result<Self, ClientError> {
        let base = base_url.trim().trim_end_matches('/').to_string();
        if !(base.starts_with("http://") || base.starts_with("https://")) {
            return Err(ClientError::Config(format!("plugin URL must start with http:// or https:// ({base_url})")));
        }
        if secret.len() < MIN_SECRET_LEN {
            return Err(ClientError::Config(format!("the shared secret must be at least {MIN_SECRET_LEN} characters")));
        }
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(5))
            // Redirects could carry the signature to another host.
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|e| ClientError::Config(e.to_string()))?;
        Ok(Self { http, base, secret: secret.as_bytes().to_vec() })
    }

    async fn get(&self, path_and_query: &str, timeout: Duration, max_bytes: usize) -> Result<(reqwest::StatusCode, Option<String>, Vec<u8>), ClientError> {
        let ts = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0);
        let mut res = self
            .http
            .get(format!("{}{path_and_query}", self.base))
            .header(HEADER_TIMESTAMP, ts.to_string())
            .header(HEADER_SIGNATURE, sign(&self.secret, ts, "GET", path_and_query, b""))
            .timeout(timeout)
            .send()
            .await
            .map_err(|e| ClientError::Unreachable(format!("plugin unreachable: {}", e.without_url())))?;
        let status = res.status();
        let content_type = res.headers().get(reqwest::header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).map(|v| v.split(';').next().unwrap_or("").trim().to_ascii_lowercase());
        let retry_after = res.headers().get(reqwest::header::RETRY_AFTER).and_then(|v| v.to_str().ok()).and_then(|v| v.trim().parse::<u64>().ok());
        let mut body = Vec::new();
        while let Some(chunk) = res.chunk().await.map_err(|e| ClientError::Unreachable(format!("plugin interrupted: {}", e.without_url())))? {
            if body.len() + chunk.len() > max_bytes {
                return Err(ClientError::Invalid(format!("plugin answer larger than {max_bytes} bytes")));
            }
            body.extend_from_slice(&chunk);
        }
        if !status.is_success() && status != reqwest::StatusCode::NOT_FOUND {
            let message = serde_json::from_slice::<serde_json::Value>(&body)
                .ok()
                .and_then(|v| v.get("error").and_then(|e| e.as_str()).map(str::to_string))
                .unwrap_or_else(|| "plugin error".to_string());
            return Err(ClientError::Rejected { status: status.as_u16(), message: message.chars().take(300).collect(), retry_after_s: retry_after });
        }
        Ok((status, content_type, body))
    }

    pub async fn manifest(&self) -> Result<Manifest, ClientError> {
        let (status, _, body) = self.get("/manifest", Duration::from_secs(10), MAX_MANIFEST_BYTES).await?;
        if !status.is_success() {
            return Err(ClientError::Invalid("plugin has no manifest".into()));
        }
        let m: Manifest = serde_json::from_slice(&body).map_err(|e| ClientError::Invalid(format!("unreadable manifest: {e}")))?;
        if m.protocol != PROTOCOL_VERSION {
            return Err(ClientError::Invalid(format!("plugin speaks protocol {}, CyclAI speaks {PROTOCOL_VERSION}", m.protocol)));
        }
        if m.id.is_empty() || m.id.len() > 32 || !m.id.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit()) {
            return Err(ClientError::Invalid("the plugin id must be 1-32 lowercase letters or digits".into()));
        }
        Ok(m)
    }

    pub async fn list(&self, after_epoch: i64) -> Result<Vec<ListedActivity>, ClientError> {
        let (status, _, body) = self.get(&format!("/activities?after={}", after_epoch.max(0)), Duration::from_secs(900), MAX_LIST_BYTES).await?;
        if !status.is_success() {
            return Err(ClientError::Invalid("unexpected answer to the listing".into()));
        }
        let r: ListResponse = serde_json::from_slice(&body).map_err(|e| ClientError::Invalid(format!("unreadable listing: {e}")))?;
        Ok(r.activities)
    }

    pub async fn fetch(&self, external_id: &str) -> Result<Fetched, ClientError> {
        let (status, content_type, body) = self.get(&format!("/activities/{}", encode_segment(external_id)), Duration::from_secs(600), MAX_ACTIVITY_BYTES).await?;
        if status == reqwest::StatusCode::NOT_FOUND || status == reqwest::StatusCode::NO_CONTENT {
            return Ok(Fetched::NotAvailable);
        }
        if content_type.as_deref() == Some(FIT_CONTENT_TYPE) {
            return Ok(Fetched::Fit(body));
        }
        let a: Activity = serde_json::from_slice(&body).map_err(|e| ClientError::Invalid(format!("unreadable activity: {e}")))?;
        Ok(Fetched::Activity(a))
    }
}
