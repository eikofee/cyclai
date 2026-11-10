//! HTTP side of the OAuth 2.1 authorization server (see `crate::oauth`):
//! discovery documents, dynamic client registration, the consent page and
//! the token endpoint. Served by the exposed API process next to `/mcp`.

use super::mcp::base_url;
use super::AppState;
use crate::oauth::{self, OAuthError};
use axum::extract::{Query, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::{Form, Json};
use serde::Deserialize;
use serde_json::{json, Value};

fn no_store(mut res: Response) -> Response {
    let h = res.headers_mut();
    h.insert(header::CACHE_CONTROL, "no-store".parse().unwrap());
    h.insert(header::PRAGMA, "no-cache".parse().unwrap());
    res
}

fn oauth_error(status: StatusCode, error: &str, description: &str) -> Response {
    no_store((status, Json(json!({ "error": error, "error_description": description }))).into_response())
}

// ───────────────────────── Discovery (RFC 9728 / RFC 8414) ─────────────────────────

/// `GET /.well-known/oauth-protected-resource` — tells the client which
/// authorization server protects `/mcp`.
pub(super) async fn protected_resource(headers: HeaderMap) -> Json<Value> {
    let base = base_url(&headers);
    Json(json!({
        "resource": format!("{base}/mcp"),
        "authorization_servers": [base],
        "scopes_supported": oauth::SCOPES,
        "bearer_methods_supported": ["header"],
        "resource_name": "CyclAI",
    }))
}

/// `GET /.well-known/oauth-authorization-server`
pub(super) async fn authorization_server(headers: HeaderMap) -> Json<Value> {
    let base = base_url(&headers);
    Json(json!({
        "issuer": base,
        "authorization_endpoint": format!("{base}/oauth/authorize"),
        "token_endpoint": format!("{base}/oauth/token"),
        "registration_endpoint": format!("{base}/oauth/register"),
        "response_types_supported": ["code"],
        "grant_types_supported": ["authorization_code", "refresh_token"],
        "code_challenge_methods_supported": ["S256"],
        "token_endpoint_auth_methods_supported": ["none"],
        "scopes_supported": oauth::SCOPES,
    }))
}

// ───────────────────────── Dynamic client registration (RFC 7591) ─────────────────────────

#[derive(Deserialize)]
pub(super) struct RegistrationRequest {
    client_name: Option<String>,
    #[serde(default)]
    redirect_uris: Vec<String>,
}

/// `POST /oauth/register` — public clients only (PKCE replaces a secret).
pub(super) async fn register(State(state): State<AppState>, Json(req): Json<RegistrationRequest>) -> Response {
    match oauth::register_client(&state.pool, req.client_name.as_deref(), &req.redirect_uris).await {
        Ok(client_id) => no_store(
            (
                StatusCode::CREATED,
                Json(json!({
                    "client_id": client_id,
                    "client_name": req.client_name,
                    "redirect_uris": req.redirect_uris,
                    "grant_types": ["authorization_code", "refresh_token"],
                    "response_types": ["code"],
                    "token_endpoint_auth_method": "none",
                    "scope": oauth::SCOPES.join(" "),
                })),
            )
                .into_response(),
        ),
        Err(OAuthError::InvalidRequest("invalid_redirect_uri")) => {
            oauth_error(StatusCode::BAD_REQUEST, "invalid_redirect_uri", "redirect_uris must be https, loopback http or an app scheme")
        }
        Err(OAuthError::InvalidRequest("too_many_clients")) => {
            oauth_error(StatusCode::TOO_MANY_REQUESTS, "invalid_client_metadata", "too many registered clients; revoke some with `cyclai oauth revoke`")
        }
        Err(_) => oauth_error(StatusCode::INTERNAL_SERVER_ERROR, "server_error", "registration failed"),
    }
}

// ───────────────────────── Consent page ─────────────────────────

#[derive(Deserialize, Default, Clone)]
pub(super) struct AuthorizeParams {
    response_type: Option<String>,
    client_id: Option<String>,
    redirect_uri: Option<String>,
    code_challenge: Option<String>,
    code_challenge_method: Option<String>,
    scope: Option<String>,
    state: Option<String>,
}

fn esc(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;").replace('\'', "&#39;")
}

fn page(status: StatusCode, body: &str) -> Response {
    let html = format!(
        r#"<!doctype html><html lang="fr"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1">
<title>CyclAI — autorisation</title>
<style>
 body{{font-family:system-ui,sans-serif;background:#0f1419;color:#e6edf3;display:flex;justify-content:center;padding:2rem 1rem}}
 main{{max-width:26rem;width:100%;background:#161b22;border:1px solid #30363d;border-radius:12px;padding:1.5rem}}
 h1{{font-size:1.2rem;margin:.2rem 0 1rem}} li{{margin:.3rem 0}} .muted{{color:#8b949e;font-size:.9rem}}
 input[type=password]{{width:100%;box-sizing:border-box;padding:.6rem;margin:.5rem 0 1rem;border-radius:8px;border:1px solid #30363d;background:#0d1117;color:inherit;font-size:1rem}}
 button{{padding:.6rem 1.1rem;border-radius:8px;border:0;font-size:1rem;cursor:pointer;margin-right:.5rem}}
 .allow{{background:#238636;color:#fff}} .deny{{background:#30363d;color:#e6edf3}} .err{{color:#ff7b72}}
</style></head><body><main>{body}</main></body></html>"#
    );
    let mut res = (status, Html(html)).into_response();
    let h = res.headers_mut();
    h.insert(header::CACHE_CONTROL, "no-store".parse().unwrap());
    h.insert(header::X_FRAME_OPTIONS, "DENY".parse().unwrap());
    h.insert(header::CONTENT_SECURITY_POLICY, "default-src 'none'; style-src 'unsafe-inline'; form-action 'self' https: http:; frame-ancestors 'none'".parse().unwrap());
    h.insert(header::REFERRER_POLICY, "no-referrer".parse().unwrap());
    res
}

fn error_page(status: StatusCode, message: &str) -> Response {
    page(status, &format!("<h1>Autorisation impossible</h1><p class=\"err\">{}</p>", esc(message)))
}

/// What a valid authorization request resolved to.
struct Validated {
    client: oauth::Client,
    redirect_uri: String,
    challenge: String,
    scope: String,
    state: Option<String>,
}

enum Invalid {
    /// The client or redirect URI is untrustworthy: show an error, never redirect.
    Page(String),
    /// Redirect back to the (verified) client with an OAuth error.
    Redirect { redirect_uri: String, state: Option<String>, error: &'static str },
}

async fn validate(pool: &sqlx::SqlitePool, p: &AuthorizeParams) -> Result<Validated, Invalid> {
    let page = |m: &str| Invalid::Page(m.to_string());
    let client_id = p.client_id.as_deref().ok_or_else(|| page("client_id manquant"))?;
    let client = oauth::client(pool, client_id).await.ok_or_else(|| page("Client inconnu ou révoqué."))?;
    let redirect_uri = p.redirect_uri.clone().ok_or_else(|| page("redirect_uri manquant"))?;
    if !client.redirect_uris.contains(&redirect_uri) {
        return Err(page("redirect_uri non enregistrée pour ce client."));
    }
    let fail = |error| Invalid::Redirect { redirect_uri: redirect_uri.clone(), state: p.state.clone(), error };
    if p.response_type.as_deref() != Some("code") {
        return Err(fail("unsupported_response_type"));
    }
    let challenge = p.code_challenge.clone().filter(|c| !c.is_empty()).ok_or_else(|| fail("invalid_request"))?;
    if p.code_challenge_method.as_deref() != Some("S256") {
        return Err(fail("invalid_request"));
    }
    let scope = oauth::normalize_scope(p.scope.as_deref()).ok_or_else(|| fail("invalid_scope"))?;
    Ok(Validated { client, redirect_uri, challenge, scope, state: p.state.clone() })
}

fn redirect_to(uri: &str, params: &[(&str, &str)]) -> Response {
    match reqwest::Url::parse(uri) {
        Ok(mut url) => {
            url.query_pairs_mut().extend_pairs(params);
            Redirect::to(url.as_str()).into_response()
        }
        Err(_) => error_page(StatusCode::BAD_REQUEST, "redirect_uri invalide"),
    }
}

fn invalid_response(inv: Invalid) -> Response {
    match inv {
        Invalid::Page(m) => error_page(StatusCode::BAD_REQUEST, &m),
        Invalid::Redirect { redirect_uri, state, error } => {
            let mut params = vec![("error", error)];
            if let Some(s) = state.as_deref() {
                params.push(("state", s));
            }
            redirect_to(&redirect_uri, &params)
        }
    }
}

fn scope_label(scope: &str) -> &'static str {
    match scope {
        "cyclai:read" => "Lire tes sorties, ta charge, ton plan et tes disponibilités",
        "cyclai:propose" => "Proposer des séances (tu valides dans le dashboard), saisir disponibilités, ressenti et note du coach",
        _ => "",
    }
}

fn consent_form(v: &Validated, p: &AuthorizeParams, error: Option<&str>) -> Response {
    let name = v.client.name.as_deref().filter(|n| !n.trim().is_empty()).unwrap_or("Une application");
    let scopes: String = v.scope.split_whitespace().map(|s| format!("<li>{}</li>", scope_label(s))).collect();
    let hidden = |k: &str, val: &Option<String>| format!(r#"<input type="hidden" name="{k}" value="{}">"#, esc(val.as_deref().unwrap_or("")));
    let error = error.map(|e| format!("<p class=\"err\">{}</p>", esc(e))).unwrap_or_default();
    let body = format!(
        r#"<h1>Autoriser « {name} » ?</h1>
<p class="muted">Cette application demande l'accès à CyclAI pour :</p><ul>{scopes}</ul>
<p class="muted">Elle ne peut rien supprimer. Tu peux révoquer l'accès avec <code>cyclai oauth revoke</code>.</p>
{error}
<form method="post" action="/oauth/authorize">
 {h1}{h2}{h3}{h4}{h5}{h6}{h7}
 <label>Mot de passe CyclAI<input type="password" name="password" autocomplete="current-password" autofocus></label>
 <button class="allow" type="submit" name="decision" value="allow">Autoriser</button>
 <button class="deny" type="submit" name="decision" value="deny" formnovalidate>Refuser</button>
</form>"#,
        name = esc(name),
        h1 = hidden("response_type", &p.response_type),
        h2 = hidden("client_id", &p.client_id),
        h3 = hidden("redirect_uri", &p.redirect_uri),
        h4 = hidden("code_challenge", &p.code_challenge),
        h5 = hidden("code_challenge_method", &p.code_challenge_method),
        h6 = hidden("scope", &p.scope),
        h7 = hidden("state", &p.state),
    );
    page(if error.is_empty() { StatusCode::OK } else { StatusCode::UNAUTHORIZED }, &body)
}

/// `GET /oauth/authorize` — the consent page.
pub(super) async fn authorize_get(State(state): State<AppState>, Query(p): Query<AuthorizeParams>) -> Response {
    let v = match validate(&state.pool, &p).await {
        Ok(v) => v,
        Err(inv) => return invalid_response(inv),
    };
    if !oauth::password_is_set(&state.pool).await {
        return error_page(StatusCode::SERVICE_UNAVAILABLE, "Aucun mot de passe n'est défini : lance `cyclai oauth set-password` sur le serveur.");
    }
    consent_form(&v, &p, None)
}

#[derive(Deserialize)]
pub(super) struct ConsentForm {
    #[serde(flatten)]
    params: AuthorizeParams,
    password: Option<String>,
    decision: Option<String>,
}

/// `POST /oauth/authorize` — the owner's decision. The request is validated
/// again: nothing from the form is trusted.
pub(super) async fn authorize_post(State(state): State<AppState>, Form(form): Form<ConsentForm>) -> Response {
    let p = form.params;
    let v = match validate(&state.pool, &p).await {
        Ok(v) => v,
        Err(inv) => return invalid_response(inv),
    };
    if form.decision.as_deref() != Some("allow") {
        return invalid_response(Invalid::Redirect { redirect_uri: v.redirect_uri, state: v.state, error: "access_denied" });
    }
    if oauth::consent_locked(&state.pool).await {
        return error_page(StatusCode::TOO_MANY_REQUESTS, "Trop de tentatives. Réessaie dans 15 minutes.");
    }
    let password = form.password.unwrap_or_default();
    if !oauth::verify_password(&state.pool, &password).await {
        oauth::record_failed_consent(&state.pool).await;
        tracing::warn!("oauth consent: wrong password");
        return consent_form(&v, &p, Some("Mot de passe incorrect."));
    }
    oauth::reset_failed_consents(&state.pool).await;
    match oauth::create_code(&state.pool, &v.client.id, &v.redirect_uri, &v.challenge, &v.scope).await {
        Ok(code) => {
            let mut params = vec![("code", code.as_str())];
            if let Some(s) = v.state.as_deref() {
                params.push(("state", s));
            }
            redirect_to(&v.redirect_uri, &params)
        }
        Err(e) => {
            tracing::error!(?e, "creating the authorization code failed");
            error_page(StatusCode::INTERNAL_SERVER_ERROR, "Erreur interne.")
        }
    }
}

// ───────────────────────── Token endpoint ─────────────────────────

#[derive(Deserialize)]
pub(super) struct TokenForm {
    grant_type: Option<String>,
    code: Option<String>,
    redirect_uri: Option<String>,
    code_verifier: Option<String>,
    client_id: Option<String>,
    refresh_token: Option<String>,
}

/// `POST /oauth/token` — authorization_code and refresh_token grants.
pub(super) async fn token(State(state): State<AppState>, Form(f): Form<TokenForm>) -> Response {
    fn need(v: &Option<String>) -> Option<&str> {
        v.as_deref().filter(|s| !s.is_empty())
    }
    let Some(client_id) = need(&f.client_id) else {
        return oauth_error(StatusCode::BAD_REQUEST, "invalid_client", "client_id is required");
    };
    let result = match f.grant_type.as_deref() {
        Some("authorization_code") => match (need(&f.code), need(&f.redirect_uri), need(&f.code_verifier)) {
            (Some(code), Some(redirect_uri), Some(verifier)) => {
                oauth::exchange_code(&state.pool, code, client_id, redirect_uri, verifier).await
            }
            _ => return oauth_error(StatusCode::BAD_REQUEST, "invalid_request", "code, redirect_uri and code_verifier are required"),
        },
        Some("refresh_token") => match need(&f.refresh_token) {
            Some(rt) => oauth::refresh(&state.pool, rt, client_id).await,
            None => return oauth_error(StatusCode::BAD_REQUEST, "invalid_request", "refresh_token is required"),
        },
        _ => return oauth_error(StatusCode::BAD_REQUEST, "unsupported_grant_type", "use authorization_code or refresh_token"),
    };
    match result {
        Ok(t) => no_store(
            Json(json!({
                "access_token": t.access_token,
                "token_type": "Bearer",
                "expires_in": t.expires_in,
                "refresh_token": t.refresh_token,
                "scope": t.scope,
            }))
            .into_response(),
        ),
        Err(OAuthError::InvalidGrant(m)) => oauth_error(StatusCode::BAD_REQUEST, "invalid_grant", m),
        Err(OAuthError::InvalidRequest(m)) => oauth_error(StatusCode::BAD_REQUEST, "invalid_request", m),
    }
}

#[cfg(test)]
mod tests {
    use super::super::api_router;
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use base64::Engine;
    use sha2::{Digest, Sha256};
    use sqlx::sqlite::SqlitePoolOptions;
    use tower::ServiceExt;

    const VERIFIER: &str = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
    const REDIRECT: &str = "https://claude.ai/api/mcp/auth_callback";

    async fn send(app: &axum::Router, req: Request<Body>) -> (StatusCode, HeaderMap, String) {
        let res = app.clone().oneshot(req).await.unwrap();
        let (status, headers) = (res.status(), res.headers().clone());
        let bytes = axum::body::to_bytes(res.into_body(), usize::MAX).await.unwrap();
        (status, headers, String::from_utf8_lossy(&bytes).to_string())
    }

    fn form_post(uri: &str, body: String) -> Request<Body> {
        Request::builder().method("POST").uri(uri).header("content-type", "application/x-www-form-urlencoded").header("host", "cyclai.example").body(Body::from(body)).unwrap()
    }

    fn enc(pairs: &[(&str, &str)]) -> String {
        reqwest::Url::parse_with_params("http://x", pairs).unwrap().query().unwrap().to_string()
    }

    fn location(h: &HeaderMap) -> reqwest::Url {
        reqwest::Url::parse(h[header::LOCATION].to_str().unwrap()).unwrap()
    }

    #[tokio::test]
    async fn discovery_documents_point_at_this_server() {
        let pool = SqlitePoolOptions::new().connect("sqlite::memory:").await.unwrap();
        crate::db::migrate(&pool).await.unwrap();
        let app = api_router(AppState { pool });
        let get = |u: &str| Request::builder().uri(u).header("host", "cyclai.example:8443").body(Body::empty()).unwrap();
        let (s, _, body) = send(&app, get("/.well-known/oauth-protected-resource")).await;
        let doc: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(s, StatusCode::OK);
        assert_eq!(doc["resource"], "https://cyclai.example:8443/mcp");
        assert_eq!(doc["authorization_servers"][0], "https://cyclai.example:8443");
        let (_, _, body) = send(&app, get("/.well-known/oauth-authorization-server")).await;
        let doc: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(doc["code_challenge_methods_supported"][0], "S256");
        assert_eq!(doc["token_endpoint"], "https://cyclai.example:8443/oauth/token");
        assert_eq!(doc["registration_endpoint"], "https://cyclai.example:8443/oauth/register");
    }

    /// The whole flow a connector performs: register, consent with the
    /// password, exchange the code, call /mcp, refresh.
    #[tokio::test]
    async fn full_flow_register_consent_token_mcp_refresh() {
        let pool = SqlitePoolOptions::new().connect("sqlite::memory:").await.unwrap();
        crate::db::migrate(&pool).await.unwrap();
        crate::oauth::set_password(&pool, "correct horse battery").await.unwrap();
        let app = api_router(AppState { pool: pool.clone() });
        let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(VERIFIER.as_bytes()));

        // 1. Dynamic registration (JSON).
        let req = Request::builder().method("POST").uri("/oauth/register").header("content-type", "application/json")
            .body(Body::from(json!({ "client_name": "Claude", "redirect_uris": [REDIRECT] }).to_string())).unwrap();
        let (s, _, body) = send(&app, req).await;
        assert_eq!(s, StatusCode::CREATED, "{body}");
        let client_id = serde_json::from_str::<Value>(&body).unwrap()["client_id"].as_str().unwrap().to_string();
        let bad = Request::builder().method("POST").uri("/oauth/register").header("content-type", "application/json")
            .body(Body::from(json!({ "redirect_uris": ["http://evil.example/cb"] }).to_string())).unwrap();
        assert_eq!(send(&app, bad).await.0, StatusCode::BAD_REQUEST);

        // 2. Consent page: GET shows the form; an unknown redirect_uri never redirects.
        let q = enc(&[("response_type", "code"), ("client_id", &client_id), ("redirect_uri", REDIRECT), ("code_challenge", &challenge), ("code_challenge_method", "S256"), ("state", "xyz")]);
        let (s, h, page) = send(&app, Request::builder().uri(format!("/oauth/authorize?{q}")).body(Body::empty()).unwrap()).await;
        assert_eq!(s, StatusCode::OK);
        assert!(page.contains("Mot de passe") && page.contains("Claude") && !page.contains("correct horse"));
        assert_eq!(h[header::X_FRAME_OPTIONS], "DENY");
        let evil = enc(&[("response_type", "code"), ("client_id", &client_id), ("redirect_uri", "https://evil.example/cb"), ("code_challenge", &challenge), ("code_challenge_method", "S256")]);
        let (s, h, _) = send(&app, Request::builder().uri(format!("/oauth/authorize?{evil}")).body(Body::empty()).unwrap()).await;
        assert_eq!(s, StatusCode::BAD_REQUEST);
        assert!(h.get(header::LOCATION).is_none());
        // PKCE is mandatory (plain is refused, via redirect with an error).
        let plain = enc(&[("response_type", "code"), ("client_id", &client_id), ("redirect_uri", REDIRECT), ("code_challenge", "abc"), ("code_challenge_method", "plain"), ("state", "s")]);
        let (s, h, _) = send(&app, Request::builder().uri(format!("/oauth/authorize?{plain}")).body(Body::empty()).unwrap()).await;
        assert!(s.is_redirection() && location(&h).query().unwrap().contains("error=invalid_request"));

        // 3. Wrong password → form again (401), right one → redirect with code + state.
        let consent = |password: &str, decision: &str| {
            form_post("/oauth/authorize", enc(&[("response_type", "code"), ("client_id", &client_id), ("redirect_uri", REDIRECT), ("code_challenge", &challenge), ("code_challenge_method", "S256"), ("scope", "cyclai:read cyclai:propose"), ("state", "xyz"), ("password", password), ("decision", decision)]))
        };
        let (s, _, page) = send(&app, consent("nope", "allow")).await;
        assert_eq!(s, StatusCode::UNAUTHORIZED);
        assert!(page.contains("incorrect"));
        let (s, h, _) = send(&app, consent("whatever", "deny")).await;
        assert!(s.is_redirection() && location(&h).query().unwrap().contains("error=access_denied"));
        let (s, h, _) = send(&app, consent("correct horse battery", "allow")).await;
        assert!(s.is_redirection(), "{s}");
        let loc = location(&h);
        assert_eq!(loc.host_str(), Some("claude.ai"));
        let pairs: std::collections::HashMap<_, _> = loc.query_pairs().into_owned().collect();
        assert_eq!(pairs["state"], "xyz");
        let code = pairs["code"].clone();

        // 4. Token exchange (PKCE) → bearer tokens; the code is single use.
        let exchange = |code: &str, verifier: &str| form_post("/oauth/token", enc(&[("grant_type", "authorization_code"), ("code", code), ("redirect_uri", REDIRECT), ("client_id", &client_id), ("code_verifier", verifier)]));
        let (s, h, body) = send(&app, exchange(&code, VERIFIER)).await;
        assert_eq!(s, StatusCode::OK, "{body}");
        assert_eq!(h[header::CACHE_CONTROL], "no-store");
        let tokens: Value = serde_json::from_str(&body).unwrap();
        assert_eq!((tokens["token_type"].as_str(), tokens["expires_in"].as_i64()), (Some("Bearer"), Some(3600)));
        assert_eq!(send(&app, exchange(&code, VERIFIER)).await.0, StatusCode::BAD_REQUEST);
        let (_, _, err) = send(&app, form_post("/oauth/token", enc(&[("grant_type", "password"), ("client_id", &client_id)]))).await;
        assert!(err.contains("unsupported_grant_type"));

        // 5. The access token opens /mcp with both scopes.
        let access = tokens["access_token"].as_str().unwrap();
        let mcp = |token: &str, body: Value| Request::builder().method("POST").uri("/mcp").header("content-type", "application/json").header("authorization", format!("Bearer {token}")).body(Body::from(body.to_string())).unwrap();
        let (s, _, body) = send(&app, mcp(access, json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/list" }))).await;
        assert_eq!(s, StatusCode::OK);
        assert!(body.contains("propose_session") && body.contains("search_workouts"));
        let (_, _, body) = send(&app, mcp(access, json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/call", "params": { "name": "get_rules", "arguments": {} } }))).await;
        assert!(body.contains("\"isError\":false"), "{body}");
        let audited: String = sqlx::query_scalar("SELECT client FROM mcp_audit LIMIT 1").fetch_one(&pool).await.unwrap();
        assert_eq!(audited, "Claude");

        // 6. Refresh rotates; the old refresh token is dead (and burns the family).
        let refresh = |rt: &str| form_post("/oauth/token", enc(&[("grant_type", "refresh_token"), ("refresh_token", rt), ("client_id", &client_id)]));
        let rt1 = tokens["refresh_token"].as_str().unwrap();
        let (s, _, body) = send(&app, refresh(rt1)).await;
        assert_eq!(s, StatusCode::OK, "{body}");
        let second: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(send(&app, refresh(rt1)).await.0, StatusCode::BAD_REQUEST);
        let (s, _, _) = send(&app, mcp(second["access_token"].as_str().unwrap(), json!({ "jsonrpc": "2.0", "id": 3, "method": "ping" }))).await;
        assert_eq!(s, StatusCode::UNAUTHORIZED, "reuse revoked the family");
    }

    #[tokio::test]
    async fn consent_is_refused_without_a_password_and_locks_after_failures() {
        let pool = SqlitePoolOptions::new().connect("sqlite::memory:").await.unwrap();
        crate::db::migrate(&pool).await.unwrap();
        let app = api_router(AppState { pool: pool.clone() });
        let id = crate::oauth::register_client(&pool, Some("c"), &[REDIRECT.to_string()]).await.unwrap();
        let q = enc(&[("response_type", "code"), ("client_id", &id), ("redirect_uri", REDIRECT), ("code_challenge", "abc"), ("code_challenge_method", "S256")]);
        let (s, _, page) = send(&app, Request::builder().uri(format!("/oauth/authorize?{q}")).body(Body::empty()).unwrap()).await;
        assert_eq!(s, StatusCode::SERVICE_UNAVAILABLE);
        assert!(page.contains("set-password"));

        crate::oauth::set_password(&pool, "correct horse battery").await.unwrap();
        let attempt = |pw: &str| form_post("/oauth/authorize", enc(&[("response_type", "code"), ("client_id", &id), ("redirect_uri", REDIRECT), ("code_challenge", "abc"), ("code_challenge_method", "S256"), ("password", pw), ("decision", "allow")]));
        for _ in 0..5 {
            assert_eq!(send(&app, attempt("wrong")).await.0, StatusCode::UNAUTHORIZED);
        }
        // Locked: even the right password is refused until the window passes.
        assert_eq!(send(&app, attempt("correct horse battery")).await.0, StatusCode::TOO_MANY_REQUESTS);
    }
}
