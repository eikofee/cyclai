//! The HTTP side of a plugin: `serve` exposes a [`Plugin`] to CyclAI.

use crate::{verify, ListResponse, Plugin, PluginError, FIT_CONTENT_TYPE, HEADER_SIGNATURE, HEADER_TIMESTAMP, MIN_SECRET_LEN};
use crate::{Fetched, MAX_REQUEST_BODY};
use axum::body::{Body, Bytes};
use axum::extract::{Path, Query, Request, State};
use axum::http::{header, HeaderValue, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde::Deserialize;
use std::net::SocketAddr;
use std::sync::Arc;

#[derive(Clone)]
struct AppState {
    plugin: Arc<dyn Plugin>,
    secret: Arc<Vec<u8>>,
}

fn error_response(status: u16, message: &str, retry_after_s: Option<u64>) -> Response {
    let mut res = (
        StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
        Json(serde_json::json!({ "error": message })),
    )
        .into_response();
    if let Some(s) = retry_after_s
        && let Ok(v) = HeaderValue::from_str(&s.to_string())
    {
        res.headers_mut().insert(header::RETRY_AFTER, v);
    }
    res
}

impl IntoResponse for PluginError {
    fn into_response(self) -> Response {
        error_response(self.status, &self.message, self.retry_after_s)
    }
}

async fn authenticate(State(state): State<AppState>, req: Request, next: Next) -> Response {
    let (parts, body) = req.into_parts();
    let Ok(bytes) = axum::body::to_bytes(body, MAX_REQUEST_BODY).await else {
        return error_response(413, "request body too large", None);
    };
    let header = |name: &str| parts.headers.get(name).and_then(|v| v.to_str().ok());
    let path = parts.uri.path_and_query().map(|p| p.as_str()).unwrap_or("/");
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0);
    if let Err(e) = verify(&state.secret, now, header(HEADER_TIMESTAMP), header(HEADER_SIGNATURE), parts.method.as_str(), path, &bytes) {
        // Same answer whatever went wrong: nothing to learn for a caller without the secret.
        let _ = e;
        return error_response(401, "unauthorized", None);
    }
    next.run(Request::from_parts(parts, Body::from(bytes))).await
}

async fn manifest(State(state): State<AppState>) -> Response {
    Json(state.plugin.manifest().await).into_response()
}

#[derive(Deserialize)]
struct ListQuery {
    #[serde(default)]
    after: i64,
}

async fn list(State(state): State<AppState>, Query(q): Query<ListQuery>) -> Response {
    match state.plugin.list(q.after).await {
        Ok(activities) => Json(ListResponse { activities }).into_response(),
        Err(e) => e.into_response(),
    }
}

async fn fetch(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    match state.plugin.fetch(&id).await {
        Ok(Fetched::Activity(a)) => Json(a).into_response(),
        Ok(Fetched::Fit(bytes)) => ([(header::CONTENT_TYPE, FIT_CONTENT_TYPE)], Bytes::from(bytes)).into_response(),
        Ok(Fetched::NotAvailable) => error_response(404, "nothing to import for this activity", None),
        Err(e) => e.into_response(),
    }
}

fn router(plugin: Arc<dyn Plugin>, secret: &str) -> Router {
    let state = AppState { plugin, secret: Arc::new(secret.as_bytes().to_vec()) };
    Router::new()
        .route("/manifest", get(manifest))
        .route("/activities", get(list))
        .route("/activities/{id}", get(fetch))
        .layer(middleware::from_fn_with_state(state.clone(), authenticate))
        .with_state(state)
}

/// Serves the plugin until the process stops. `secret` is the one shared with
/// CyclAI (at least [`MIN_SECRET_LEN`] characters).
pub async fn serve(plugin: Arc<dyn Plugin>, addr: SocketAddr, secret: String) -> anyhow::Result<()> {
    if secret.len() < MIN_SECRET_LEN {
        anyhow::bail!("the shared secret must be at least {MIN_SECRET_LEN} characters");
    }
    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, router(plugin, &secret)).await?;
    Ok(())
}

/// For tests and embedding: the router without binding a port.
pub fn app(plugin: Arc<dyn Plugin>, secret: &str) -> Router {
    router(plugin, secret)
}
