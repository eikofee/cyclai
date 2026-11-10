//! OAuth 2.1 authorization server for the MCP connector (Claude web/mobile):
//! dynamic client registration, authorization code + PKCE (S256), access
//! tokens of one hour and rotating refresh tokens that expire after a period
//! of inactivity (90 days by default).
//!
//! The owner proves who they are on the consent page with a password set
//! from the command line (`cyclai oauth set-password`) — there is no user
//! database. Codes and tokens are random and stored only as SHA-256 hashes.

use argon2::password_hash::rand_core::OsRng;
use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use argon2::Argon2;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use rand::RngCore;
use sha2::{Digest, Sha256};
use sqlx::SqlitePool;

pub const SCOPES: [&str; 2] = ["cyclai:read", "cyclai:propose"];
pub const ACCESS_TTL_S: i64 = 3600;
const CODE_TTL_S: i64 = 300;
const MAX_CLIENTS: i64 = 50;
pub const MIN_PASSWORD_LEN: usize = 12;

#[derive(Debug, PartialEq, Eq)]
pub enum OAuthError {
    /// Maps to `invalid_grant` / `invalid_client` / `invalid_request` at the HTTP layer.
    InvalidGrant(&'static str),
    InvalidRequest(&'static str),
}

fn server<E>(_: E) -> OAuthError {
    OAuthError::InvalidRequest("server_error")
}

#[derive(Debug)]
pub struct Tokens {
    pub access_token: String,
    pub refresh_token: String,
    pub expires_in: i64,
    pub scope: String,
}

pub struct Client {
    pub id: String,
    pub name: Option<String>,
    pub redirect_uris: Vec<String>,
}

fn now() -> i64 {
    chrono::Utc::now().timestamp()
}

fn random_secret() -> String {
    let mut bytes = [0u8; 32];
    OsRng.fill_bytes(&mut bytes);
    URL_SAFE_NO_PAD.encode(bytes)
}

pub fn sha256_hex(s: &str) -> String {
    Sha256::digest(s.as_bytes()).iter().map(|b| format!("{b:02x}")).collect()
}

/// PKCE S256: `challenge == base64url(sha256(verifier))`.
pub fn pkce_matches(verifier: &str, challenge: &str) -> bool {
    // RFC 7636: the verifier is 43-128 unreserved characters.
    (43..=128).contains(&verifier.len())
        && verifier.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '.' | '_' | '~'))
        && URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes())) == challenge
}

/// Redirect URIs we accept at registration: https, loopback http, or an app
/// scheme — never `javascript:`/`data:`/`file:`, never a fragment.
pub fn valid_redirect_uri(uri: &str) -> bool {
    let Ok(url) = reqwest::Url::parse(uri) else { return false };
    if url.fragment().is_some() || uri.len() > 500 {
        return false;
    }
    match url.scheme() {
        "https" => url.host_str().is_some(),
        "http" => matches!(url.host_str(), Some("localhost" | "127.0.0.1" | "[::1]")),
        "javascript" | "data" | "file" | "vbscript" | "about" | "blob" => false,
        _ => true,
    }
}

/// Keeps only known scopes; empty → everything the server offers.
pub fn normalize_scope(requested: Option<&str>) -> Option<String> {
    let wanted: Vec<&str> = requested.unwrap_or("").split_whitespace().collect();
    if wanted.is_empty() {
        return Some(SCOPES.join(" "));
    }
    if wanted.iter().any(|s| !SCOPES.contains(s)) {
        return None;
    }
    let mut out: Vec<&str> = SCOPES.iter().copied().filter(|s| wanted.contains(s)).collect();
    out.dedup();
    Some(out.join(" "))
}

// ───────────────────────── Owner password ─────────────────────────

pub async fn set_password(pool: &SqlitePool, password: &str) -> anyhow::Result<()> {
    if password.chars().count() < MIN_PASSWORD_LEN {
        anyhow::bail!("the password must be at least {MIN_PASSWORD_LEN} characters long");
    }
    let hash = Argon2::default()
        .hash_password(password.as_bytes(), &SaltString::generate(&mut OsRng))
        .map_err(|e| anyhow::anyhow!("hashing failed: {e}"))?
        .to_string();
    sqlx::query("UPDATE oauth_settings SET password_hash = ? WHERE id = 1").bind(hash).execute(pool).await?;
    Ok(())
}

pub async fn password_is_set(pool: &SqlitePool) -> bool {
    sqlx::query_scalar::<_, Option<String>>("SELECT password_hash FROM oauth_settings WHERE id = 1")
        .fetch_one(pool)
        .await
        .ok()
        .flatten()
        .is_some()
}

pub async fn verify_password(pool: &SqlitePool, password: &str) -> bool {
    let hash: Option<String> = sqlx::query_scalar("SELECT password_hash FROM oauth_settings WHERE id = 1")
        .fetch_one(pool)
        .await
        .ok()
        .flatten();
    let Some(hash) = hash else { return false };
    let Ok(parsed) = PasswordHash::new(&hash) else { return false };
    Argon2::default().verify_password(password.as_bytes(), &parsed).is_ok()
}

// Failed consent attempts: five wrong passwords lock the page for 15 minutes.
// Kept in the database so a restart does not reset the counter.
const LOCK_WINDOW_S: i64 = 15 * 60;
const MAX_FAILURES: i64 = 5;

pub async fn consent_locked(pool: &SqlitePool) -> bool {
    let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM oauth_failures WHERE at > ?")
        .bind(now() - LOCK_WINDOW_S)
        .fetch_one(pool)
        .await
        .unwrap_or(MAX_FAILURES);
    n >= MAX_FAILURES
}

pub async fn record_failed_consent(pool: &SqlitePool) {
    let _ = sqlx::query("DELETE FROM oauth_failures WHERE at <= ?").bind(now() - LOCK_WINDOW_S).execute(pool).await;
    let _ = sqlx::query("INSERT INTO oauth_failures (at) VALUES (?)").bind(now()).execute(pool).await;
}

pub async fn reset_failed_consents(pool: &SqlitePool) {
    let _ = sqlx::query("DELETE FROM oauth_failures").execute(pool).await;
}

pub async fn refresh_days(pool: &SqlitePool) -> i64 {
    sqlx::query_scalar("SELECT refresh_days FROM oauth_settings WHERE id = 1").fetch_one(pool).await.unwrap_or(90).clamp(1, 3650)
}

// ───────────────────────── Clients ─────────────────────────

pub async fn register_client(pool: &SqlitePool, name: Option<&str>, redirect_uris: &[String]) -> Result<String, OAuthError> {
    if redirect_uris.is_empty() || redirect_uris.len() > 10 || !redirect_uris.iter().all(|u| valid_redirect_uri(u)) {
        return Err(OAuthError::InvalidRequest("invalid_redirect_uri"));
    }
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM oauth_clients WHERE revoked_at IS NULL")
        .fetch_one(pool)
        .await
        .map_err(|_| OAuthError::InvalidRequest("server_error"))?;
    if count >= MAX_CLIENTS {
        return Err(OAuthError::InvalidRequest("too_many_clients"));
    }
    let id = format!("cyclai-{}", &random_secret()[..22]);
    let name: Option<String> = name.map(|n| n.chars().take(100).collect());
    sqlx::query("INSERT INTO oauth_clients (client_id, client_name, redirect_uris_json) VALUES (?, ?, ?)")
        .bind(&id)
        .bind(name)
        .bind(serde_json::to_string(redirect_uris).unwrap_or_default())
        .execute(pool)
        .await
        .map_err(|_| OAuthError::InvalidRequest("server_error"))?;
    Ok(id)
}

pub async fn client(pool: &SqlitePool, id: &str) -> Option<Client> {
    let (name, uris): (Option<String>, String) =
        sqlx::query_as("SELECT client_name, redirect_uris_json FROM oauth_clients WHERE client_id = ? AND revoked_at IS NULL")
            .bind(id)
            .fetch_optional(pool)
            .await
            .ok()
            .flatten()?;
    Some(Client { id: id.to_string(), name, redirect_uris: serde_json::from_str(&uris).unwrap_or_default() })
}

// ───────────────────────── Codes and tokens ─────────────────────────

pub async fn create_code(pool: &SqlitePool, client_id: &str, redirect_uri: &str, challenge: &str, scope: &str) -> anyhow::Result<String> {
    let code = random_secret();
    // Opportunistic cleanup of expired codes.
    sqlx::query("DELETE FROM oauth_codes WHERE expires_at < ?").bind(now()).execute(pool).await?;
    sqlx::query("INSERT INTO oauth_codes (code_hash, client_id, redirect_uri, code_challenge, scope, expires_at) VALUES (?, ?, ?, ?, ?, ?)")
        .bind(sha256_hex(&code))
        .bind(client_id)
        .bind(redirect_uri)
        .bind(challenge)
        .bind(scope)
        .bind(now() + CODE_TTL_S)
        .execute(pool)
        .await?;
    Ok(code)
}

async fn issue(pool: &SqlitePool, client_id: &str, scope: &str) -> anyhow::Result<Tokens> {
    let (access, refresh) = (random_secret(), random_secret());
    let t = now();
    sqlx::query("DELETE FROM oauth_access_tokens WHERE expires_at < ?").bind(t).execute(pool).await?;
    sqlx::query("INSERT INTO oauth_access_tokens (token_hash, client_id, scope, expires_at) VALUES (?, ?, ?, ?)")
        .bind(sha256_hex(&access))
        .bind(client_id)
        .bind(scope)
        .bind(t + ACCESS_TTL_S)
        .execute(pool)
        .await?;
    sqlx::query("INSERT INTO oauth_refresh_tokens (token_hash, client_id, scope, expires_at) VALUES (?, ?, ?, ?)")
        .bind(sha256_hex(&refresh))
        .bind(client_id)
        .bind(scope)
        .bind(t + refresh_days(pool).await * 86_400)
        .execute(pool)
        .await?;
    Ok(Tokens { access_token: access, refresh_token: refresh, expires_in: ACCESS_TTL_S, scope: scope.to_string() })
}

/// Authorization-code grant. The code is single use: it is consumed even when
/// a later check fails.
pub async fn exchange_code(pool: &SqlitePool, code: &str, client_id: &str, redirect_uri: &str, verifier: &str) -> Result<Tokens, OAuthError> {
    let row: Option<(String, String, String, String, i64)> = sqlx::query_as(
        "DELETE FROM oauth_codes WHERE code_hash = ? RETURNING client_id, redirect_uri, code_challenge, scope, expires_at",
    )
    .bind(sha256_hex(code))
    .fetch_optional(pool)
    .await
    .map_err(server)?;
    let (c_client, c_redirect, challenge, scope, expires_at) = row.ok_or(OAuthError::InvalidGrant("unknown or already used code"))?;
    if expires_at < now() {
        return Err(OAuthError::InvalidGrant("expired code"));
    }
    if c_client != client_id || c_redirect != redirect_uri {
        return Err(OAuthError::InvalidGrant("code was issued to another client or redirect_uri"));
    }
    if !pkce_matches(verifier, &challenge) {
        return Err(OAuthError::InvalidGrant("PKCE verification failed"));
    }
    if client(pool, client_id).await.is_none() {
        return Err(OAuthError::InvalidGrant("client revoked"));
    }
    issue(pool, client_id, &scope).await.map_err(|e| {
        tracing::error!(?e, "issuing tokens failed");
        OAuthError::InvalidRequest("server_error")
    })
}

/// Refresh-token grant with rotation: the old token dies, a new pair is
/// issued and the inactivity window restarts. Presenting an already-rotated
/// token means it leaked, so every token of that client is revoked.
pub async fn refresh(pool: &SqlitePool, token: &str, client_id: &str) -> Result<Tokens, OAuthError> {
    let hash = sha256_hex(token);
    let row: Option<(String, String, i64, Option<String>)> =
        sqlx::query_as("SELECT client_id, scope, expires_at, revoked_at FROM oauth_refresh_tokens WHERE token_hash = ?")
            .bind(&hash)
            .fetch_optional(pool)
            .await
            .map_err(server)?;
    let (c_client, scope, expires_at, revoked_at) = row.ok_or(OAuthError::InvalidGrant("unknown refresh token"))?;
    if c_client != client_id {
        return Err(OAuthError::InvalidGrant("refresh token belongs to another client"));
    }
    if revoked_at.is_some() {
        tracing::warn!(client_id, "refresh token reuse detected: revoking the client's tokens");
        revoke_tokens(pool, client_id).await.map_err(server)?;
        return Err(OAuthError::InvalidGrant("refresh token already used"));
    }
    if expires_at < now() {
        return Err(OAuthError::InvalidGrant("refresh token expired after inactivity"));
    }
    if client(pool, client_id).await.is_none() {
        return Err(OAuthError::InvalidGrant("client revoked"));
    }
    sqlx::query("UPDATE oauth_refresh_tokens SET revoked_at = datetime('now'), last_used_at = datetime('now') WHERE token_hash = ?")
        .bind(&hash)
        .execute(pool)
        .await
        .map_err(server)?;
    issue(pool, client_id, &scope).await.map_err(|e| {
        tracing::error!(?e, "issuing tokens failed");
        OAuthError::InvalidRequest("server_error")
    })
}

/// Resolves an access token to `(client label, scopes)`.
pub async fn verify_access(pool: &SqlitePool, token: &str) -> Option<(String, Vec<String>)> {
    let (client_id, scope, name): (String, String, Option<String>) = sqlx::query_as(
        "SELECT t.client_id, t.scope, c.client_name FROM oauth_access_tokens t \
         JOIN oauth_clients c ON c.client_id = t.client_id \
         WHERE t.token_hash = ? AND t.expires_at > ? AND c.revoked_at IS NULL",
    )
    .bind(sha256_hex(token))
    .bind(now())
    .fetch_optional(pool)
    .await
    .ok()
    .flatten()?;
    let label = name.filter(|n| !n.trim().is_empty()).unwrap_or(client_id);
    Some((label, scope.split_whitespace().map(String::from).collect()))
}

async fn revoke_tokens(pool: &SqlitePool, client_id: &str) -> anyhow::Result<()> {
    sqlx::query("DELETE FROM oauth_access_tokens WHERE client_id = ?").bind(client_id).execute(pool).await?;
    sqlx::query("UPDATE oauth_refresh_tokens SET revoked_at = COALESCE(revoked_at, datetime('now')) WHERE client_id = ?")
        .bind(client_id)
        .execute(pool)
        .await?;
    Ok(())
}

// ───────────────────────── Administration (CLI) ─────────────────────────

pub struct ClientSummary {
    pub id: String,
    pub name: Option<String>,
    pub created_at: String,
    pub last_used_at: Option<String>,
    pub revoked: bool,
}

pub async fn list(pool: &SqlitePool) -> anyhow::Result<Vec<ClientSummary>> {
    let rows: Vec<(String, Option<String>, String, Option<String>, Option<String>)> = sqlx::query_as(
        "SELECT c.client_id, c.client_name, c.created_at, \
                (SELECT MAX(last_used_at) FROM oauth_refresh_tokens r WHERE r.client_id = c.client_id), c.revoked_at \
         FROM oauth_clients c ORDER BY c.created_at",
    )
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(id, name, created_at, last_used_at, revoked)| ClientSummary { id, name, created_at, last_used_at, revoked: revoked.is_some() })
        .collect())
}

/// Revokes a client (`client_id`) or all of them (`"all"`); returns how many.
pub async fn revoke(pool: &SqlitePool, target: &str) -> anyhow::Result<u64> {
    let ids: Vec<String> = if target == "all" {
        sqlx::query_scalar("SELECT client_id FROM oauth_clients WHERE revoked_at IS NULL").fetch_all(pool).await?
    } else {
        sqlx::query_scalar("SELECT client_id FROM oauth_clients WHERE client_id = ? AND revoked_at IS NULL")
            .bind(target)
            .fetch_all(pool)
            .await?
    };
    for id in &ids {
        sqlx::query("UPDATE oauth_clients SET revoked_at = datetime('now') WHERE client_id = ?").bind(id).execute(pool).await?;
        revoke_tokens(pool, id).await?;
    }
    Ok(ids.len() as u64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::sqlite::SqlitePoolOptions;

    async fn pool() -> SqlitePool {
        let pool = SqlitePoolOptions::new().connect("sqlite::memory:").await.unwrap();
        crate::db::migrate(&pool).await.unwrap();
        pool
    }

    fn challenge(verifier: &str) -> String {
        URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
    }

    const VERIFIER: &str = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";

    #[test]
    fn pkce_follows_rfc_7636() {
        // Appendix B of the RFC.
        assert!(pkce_matches(VERIFIER, "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"));
        assert!(!pkce_matches(VERIFIER, "wrong"));
        assert!(!pkce_matches("short", &challenge("short")), "verifier too short");
    }

    #[test]
    fn redirect_uris_and_scopes_are_validated() {
        assert!(valid_redirect_uri("https://claude.ai/api/mcp/auth_callback"));
        assert!(valid_redirect_uri("http://localhost:6274/callback"));
        assert!(valid_redirect_uri("claude://oauth/callback"));
        assert!(!valid_redirect_uri("http://evil.example/cb"));
        assert!(!valid_redirect_uri("javascript:alert(1)"));
        assert!(!valid_redirect_uri("https://claude.ai/cb#frag"));
        assert!(!valid_redirect_uri("nope"));
        assert_eq!(normalize_scope(None).unwrap(), "cyclai:read cyclai:propose");
        assert_eq!(normalize_scope(Some("cyclai:read")).unwrap(), "cyclai:read");
        assert!(normalize_scope(Some("admin")).is_none());
    }

    #[tokio::test]
    async fn password_is_hashed_and_verified() {
        let pool = pool().await;
        assert!(!password_is_set(&pool).await);
        assert!(!verify_password(&pool, "anything").await);
        assert!(set_password(&pool, "short").await.is_err());
        set_password(&pool, "correct horse battery").await.unwrap();
        assert!(password_is_set(&pool).await);
        assert!(verify_password(&pool, "correct horse battery").await);
        assert!(!verify_password(&pool, "wrong horse battery").await);
        let stored: String = sqlx::query_scalar("SELECT password_hash FROM oauth_settings").fetch_one(&pool).await.unwrap();
        assert!(stored.starts_with("$argon2") && !stored.contains("horse"));
    }

    #[tokio::test]
    async fn code_flow_issues_tokens_once_and_checks_everything() {
        let pool = pool().await;
        let uris = vec!["https://claude.ai/cb".to_string()];
        let id = register_client(&pool, Some("Claude"), &uris).await.unwrap();
        assert_eq!(client(&pool, &id).await.unwrap().redirect_uris, uris);
        assert!(register_client(&pool, None, &["http://evil.example".to_string()]).await.is_err());

        let new_code = || async { create_code(&pool, &id, "https://claude.ai/cb", &challenge(VERIFIER), "cyclai:read").await.unwrap() };

        // Wrong verifier / redirect / client: refused, and the code is burnt.
        let code = new_code().await;
        assert!(exchange_code(&pool, &code, &id, "https://claude.ai/cb", &"x".repeat(50)).await.is_err());
        assert!(exchange_code(&pool, &code, &id, "https://claude.ai/cb", VERIFIER).await.is_err(), "burnt");
        let code = new_code().await;
        assert!(exchange_code(&pool, &code, &id, "https://claude.ai/other", VERIFIER).await.is_err());
        let code = new_code().await;
        assert!(exchange_code(&pool, &code, "someone-else", "https://claude.ai/cb", VERIFIER).await.is_err());

        // The good one works once.
        let code = new_code().await;
        let t = exchange_code(&pool, &code, &id, "https://claude.ai/cb", VERIFIER).await.unwrap();
        assert_eq!((t.scope.as_str(), t.expires_in), ("cyclai:read", 3600));
        assert!(exchange_code(&pool, &code, &id, "https://claude.ai/cb", VERIFIER).await.is_err());
        let (label, scopes) = verify_access(&pool, &t.access_token).await.unwrap();
        assert_eq!((label.as_str(), scopes), ("Claude", vec!["cyclai:read".to_string()]));
        assert!(verify_access(&pool, "nope").await.is_none());

        // Only hashes are stored.
        let stored: String = sqlx::query_scalar("SELECT token_hash FROM oauth_access_tokens").fetch_one(&pool).await.unwrap();
        assert_ne!(stored, t.access_token);
    }

    #[tokio::test]
    async fn refresh_rotates_expires_on_inactivity_and_detects_reuse() {
        let pool = pool().await;
        let id = register_client(&pool, Some("Claude"), &["https://claude.ai/cb".to_string()]).await.unwrap();
        let code = create_code(&pool, &id, "https://claude.ai/cb", &challenge(VERIFIER), "cyclai:read cyclai:propose").await.unwrap();
        let first = exchange_code(&pool, &code, &id, "https://claude.ai/cb", VERIFIER).await.unwrap();

        let second = refresh(&pool, &first.refresh_token, &id).await.unwrap();
        assert_ne!(second.refresh_token, first.refresh_token);
        assert_eq!(second.scope, "cyclai:read cyclai:propose");
        assert!(refresh(&pool, &second.refresh_token, "another-client").await.is_err());

        // Replaying the rotated token revokes everything of that client.
        assert_eq!(refresh(&pool, &first.refresh_token, &id).await.err(), Some(OAuthError::InvalidGrant("refresh token already used")));
        assert!(verify_access(&pool, &second.access_token).await.is_none());
        assert!(refresh(&pool, &second.refresh_token, &id).await.is_err());

        // Inactivity: a token past its expiry is refused.
        let code = create_code(&pool, &id, "https://claude.ai/cb", &challenge(VERIFIER), "cyclai:read").await.unwrap();
        let t = exchange_code(&pool, &code, &id, "https://claude.ai/cb", VERIFIER).await.unwrap();
        sqlx::query("UPDATE oauth_refresh_tokens SET expires_at = ? WHERE token_hash = ?")
            .bind(now() - 1)
            .bind(sha256_hex(&t.refresh_token))
            .execute(&pool)
            .await
            .unwrap();
        assert_eq!(refresh(&pool, &t.refresh_token, &id).await.err(), Some(OAuthError::InvalidGrant("refresh token expired after inactivity")));
    }

    #[tokio::test]
    async fn refresh_window_is_configurable_and_revoking_a_client_kills_its_tokens() {
        let pool = pool().await;
        sqlx::query("UPDATE oauth_settings SET refresh_days = 30").execute(&pool).await.unwrap();
        let id = register_client(&pool, None, &["https://claude.ai/cb".to_string()]).await.unwrap();
        let code = create_code(&pool, &id, "https://claude.ai/cb", &challenge(VERIFIER), "cyclai:read").await.unwrap();
        let t = exchange_code(&pool, &code, &id, "https://claude.ai/cb", VERIFIER).await.unwrap();
        let expires: i64 = sqlx::query_scalar("SELECT expires_at FROM oauth_refresh_tokens").fetch_one(&pool).await.unwrap();
        assert!((expires - now() - 30 * 86_400).abs() < 5);

        assert_eq!(list(&pool).await.unwrap().len(), 1);
        assert_eq!(revoke(&pool, &id).await.unwrap(), 1);
        assert!(verify_access(&pool, &t.access_token).await.is_none());
        assert!(refresh(&pool, &t.refresh_token, &id).await.is_err());
        assert_eq!(revoke(&pool, "all").await.unwrap(), 0);
        assert!(list(&pool).await.unwrap()[0].revoked);
    }
}
