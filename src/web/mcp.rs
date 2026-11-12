//! MCP server (Model Context Protocol, "streamable HTTP" transport, JSON-RPC
//! 2.0) served by the exposed API process at `POST /mcp`.
//!
//! Principle: the engine computes, the agent chooses and explains. Every tool
//! is a thin adapter over an `/api/v1` route — it is called *in-process*
//! through the same router, so validation, hard rules and limits are the ones
//! of the REST API, never re-implemented. There is deliberately no tool that
//! deletes anything, and proposals are only ever stored as `proposed`: the
//! user validates them in the dashboard.
//!
//! Access is scoped: `cyclai:read` (everything that only reads or simulates)
//! and `cyclai:propose` (proposals, availability, skip, check-in, coach note).

use super::{api_routes, AppState};
use axum::extract::State;
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::{json, Map, Value};
use std::collections::{HashMap, VecDeque};
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

pub const SCOPE_READ: &str = "cyclai:read";
pub const SCOPE_PROPOSE: &str = "cyclai:propose";

const LATEST_PROTOCOL: &str = "2025-06-18";
const SUPPORTED_PROTOCOLS: [&str; 3] = ["2025-06-18", "2025-03-26", "2024-11-05"];

const INSTRUCTIONS: &str = "CyclAI is a cycling coaching engine. The engine computes (load, rules, envelopes, \
projections); you choose and explain. Hard rules (CTL ramp, TSB floor, consecutive intense days, rest days) are \
enforced server-side: a proposal that breaks one is refused with code `rule_violation` and the rule that fired. \
Proposals (propose_session) are only stored as `proposed`; the athlete confirms or rejects them in the dashboard. \
Nothing can be deleted through this interface. Typical flow: get_status, get_availability and get_ride_envelope \
(or search_workouts), project_load to check, then propose_session with a short rationale.";

/// Who is calling and what they may do.
#[derive(Debug, Clone)]
pub struct Principal {
    pub client: String,
    pub scopes: Vec<&'static str>,
}

impl Principal {
    fn allows(&self, scope: &str) -> bool {
        self.scopes.iter().any(|s| *s == scope)
    }
}

// ───────────────────────── Tool catalogue ─────────────────────────

#[derive(Clone, Copy)]
enum Route {
    Get(&'static str),
    Post(&'static str),
    Put(&'static str),
    /// Several GETs merged into one object, keyed by name.
    Status,
}

struct Tool {
    name: &'static str,
    description: &'static str,
    scope: &'static str,
    route: Route,
    input: Value,
}

fn obj(properties: Value, required: &[&str]) -> Value {
    json!({ "type": "object", "properties": properties, "required": required, "additionalProperties": false })
}

fn tools() -> Vec<Tool> {
    let date = |d: &str| json!({ "type": "string", "description": d, "pattern": "^\\d{4}-\\d{2}-\\d{2}$" });
    let int = |d: &str| json!({ "type": "integer", "description": d });
    let num = |d: &str| json!({ "type": "number", "description": d });
    let text = |d: &str| json!({ "type": "string", "description": d });
    vec![
        Tool {
            name: "get_status",
            description: "Overview in one call: last 7 days summary, 14 days of load (TSS/CTL/ATL/TSB), the sessions \
                          on the calendar (with status) and the coach note shown on the dashboard.",
            scope: SCOPE_READ,
            route: Route::Status,
            input: obj(json!({}), &[]),
        },
        Tool {
            name: "get_athlete_profile",
            description: "FTP, heart-rate zones, power zones, current form and goals of the athlete.",
            scope: SCOPE_READ,
            route: Route::Get("/api/v1/profile"),
            input: obj(json!({}), &[]),
        },
        Tool {
            name: "get_rules",
            description: "The hard training rules enforced by the server (CTL ramp cap, TSB floor, intense-day \
                          definition, max consecutive intense days, rest days). Adjustable only by the athlete.",
            scope: SCOPE_READ,
            route: Route::Get("/api/v1/rules"),
            input: obj(json!({}), &[]),
        },
        Tool {
            name: "get_load_history",
            description: "Daily series of TSS, CTL (fitness), ATL (fatigue) and TSB (form), oldest first.",
            scope: SCOPE_READ,
            route: Route::Get("/api/v1/load"),
            input: obj(json!({ "days": int("Window in days, default 90, max 1500.") }), &[]),
        },
        Tool {
            name: "list_activities",
            description: "Latest completed activities, newest first. Each has indoor / power_source \
                          (none|trainer|pedals) / has_hr, TSS, IF and power-zone seconds.",
            scope: SCOPE_READ,
            route: Route::Get("/api/v1/activities"),
            input: obj(
                json!({
                    "limit": int("Max items, default 10, max 200."),
                    "days": int("Only the last N days."),
                    "sport": json!({ "type": "string", "enum": ["ride", "run"] }),
                }),
                &[],
            ),
        },
        Tool {
            name: "get_activity",
            description: "One activity with laps, performance analysis, segment efforts and the generated review.",
            scope: SCOPE_READ,
            route: Route::Get("/api/v1/activities/{id}"),
            input: obj(json!({ "id": int("Activity id.") }), &["id"]),
        },
        Tool {
            name: "search_workouts",
            description: "Search the session library (incl. the Zwift catalogue). IF, TSS, power_zone_minutes and \
                          tags are computed at the athlete's CURRENT FTP. Results carry the exact session name, \
                          the plan of origin (`plan`) and `last_done`. Use `steps: true` only for a short list.",
            scope: SCOPE_READ,
            route: Route::Get("/api/v1/workouts"),
            input: obj(
                json!({
                    "q": text("Case-insensitive substring of the name."),
                    "category": text("Catalogue category, e.g. `Depuis Zwift`."),
                    "group": text("Plan key within a category."),
                    "source": text("manual, digest, zwift…"),
                    "sport": json!({ "type": "string", "enum": ["cycling", "running", "other"] }),
                    "min_duration_min": num("Minimum duration (min)."),
                    "max_duration_min": num("Maximum duration (min)."),
                    "min_tss": num("Minimum estimated TSS."),
                    "max_tss": num("Maximum estimated TSS."),
                    "min_if": num("Minimum intensity factor."),
                    "max_if": num("Maximum intensity factor."),
                    "min_zone_time": text("Minimum time per power zone, `zone:minutes[,zone:minutes]` (zones 1-7), e.g. `4:20` or `4:10,5:5`."),
                    "tag": text("recuperation, endurance, tempo, seuil, vo2, anaerobie (comma = any of)."),
                    "target_tss": num("Rank by closeness to this TSS."),
                    "exclude_done_within_days": int("Skip sessions already done in the last N days."),
                    "steps": json!({ "type": "boolean", "description": "Include the full step structure." }),
                    "limit": int("Default 200, max 500."),
                    "offset": int("Pagination offset."),
                }),
                &[],
            ),
        },
        Tool {
            name: "get_workout",
            description: "One library session with its full step structure (targets as % FTP / watts at the current FTP).",
            scope: SCOPE_READ,
            route: Route::Get("/api/v1/workouts/{id}"),
            input: obj(json!({ "id": int("Workout template id.") }), &["id"]),
        },
        Tool {
            name: "get_plan",
            description: "Sessions on the calendar (status planned|proposed|confirmed|done|skipped|rejected), \
                          with load estimates and, for done ones, the linked activity.",
            scope: SCOPE_READ,
            route: Route::Get("/api/v1/planned"),
            input: obj(
                json!({
                    "from": date("Inclusive, default a week back."),
                    "to": date("Exclusive, default six weeks ahead."),
                    "sport": json!({ "type": "string", "enum": ["cycling", "running", "other"] }),
                }),
                &[],
            ),
        },
        Tool {
            name: "get_availability",
            description: "Time the athlete can ride, day by day (weekday defaults + per-date exceptions).",
            scope: SCOPE_READ,
            route: Route::Get("/api/v1/availability"),
            input: obj(
                json!({ "from": date("Default today."), "to": date("Default from + 13 days, max 92-day range.") }),
                &[],
            ),
        },
        Tool {
            name: "get_ride_envelope",
            description: "What a ride on a given day could look like: scenarios that fit the available time \
                          (tranquille / moyenne / longue_denivele) with a typical TSS range learned from the \
                          athlete's history, projected TSB and feasibility (ok|limite|deconseille) vs the hard rules.",
            scope: SCOPE_READ,
            route: Route::Get("/api/v1/ride-envelope"),
            input: obj(
                json!({
                    "date": date("Today or later."),
                    "available_min": int("Defaults to the availability of that day."),
                }),
                &["date"],
            ),
        },
        Tool {
            name: "project_load",
            description: "Simulation only (nothing is written): projects CTL/ATL/TSB day by day for extra sessions \
                          plus the calendar and lists the hard-rule violations.",
            scope: SCOPE_READ,
            route: Route::Post("/api/v1/projection"),
            input: obj(
                json!({
                    "sessions": {
                        "type": "array",
                        "items": obj(
                            json!({
                                "date": date("Today or later."),
                                "workout_id": int("A library session…"),
                                "tss": num("…or a bare TSS."),
                                "intensity_factor": num("Optional with tss, for the intense-day rule."),
                            }),
                            &["date"],
                        ),
                    },
                    "include_planned": json!({ "type": "boolean", "description": "Count sessions already on the calendar (default true)." }),
                }),
                &[],
            ),
        },
        Tool {
            name: "propose_session",
            description: "Propose a session for a day (today included; the activity sources are assumed synced). Give workout_id \
                          (from search_workouts) OR free_ride_budget. Stored as `proposed`; the athlete validates it \
                          in the dashboard. Refused with `rule_violation` if it would newly break a hard rule. \
                          Retrying with the same idempotency_key returns the first proposal. Optionally offer `alternatives`.",
            scope: SCOPE_PROPOSE,
            route: Route::Post("/api/v1/proposals"),
            input: obj(
                json!({
                    "date": date("Today or later."),
                    "workout_id": int("Library session id."),
                    "free_ride_budget": obj(
                        json!({
                            "tss_range": json!({ "type": "array", "items": { "type": "number" }, "minItems": 2, "maxItems": 2, "description": "[min, max] TSS; the engine fills it when omitted." }),
                            "duration_max_min": int("Upper bound on the ride duration; must fit the day's availability."),
                            "intent": json!({ "type": "string", "enum": ["endurance", "denivele", "recuperation", "tempo", "intense", "course"], "description": "Kind of ride; also sets the default intensity ceiling." }),
                            "expected_if": json!({ "type": "number", "minimum": 0.3, "maximum": 1.2, "description": "Real intensity factor of the ride (e.g. ~0.9 for a race or a fast group ride). Overrides the intent's ceiling and is what the hard rules see (hard-day counting). Use it instead of tss_range." }),
                        }),
                        &["duration_max_min", "intent"],
                    ),
                    "rationale": text("Why this session on this day; shown to the athlete."),
                    "idempotency_key": text("Stable key to make retries safe."),
                    "alternatives": json!({
                        "type": "array",
                        "maxItems": 3,
                        "description": "Up to 3 other library sessions the athlete may prefer (only with workout_id). Each is checked against the hard rules like the main one; the athlete can swap in the dashboard.",
                        "items": obj(
                            json!({
                                "workout_id": int("Library session id."),
                                "note": text("What it trades off vs the main session. Max 200 chars."),
                            }),
                            &["workout_id"],
                        ),
                    }),
                }),
                &["date", "rationale"],
            ),
        },
        Tool {
            name: "set_availability",
            description: "Set the availability exception of one day (`date`) or a range (`from`/`to`). Weekday defaults are never touched.",
            scope: SCOPE_PROPOSE,
            route: Route::Put("/api/v1/availability"),
            input: obj(
                json!({
                    "date": date("One day…"),
                    "from": date("…or a range start."),
                    "to": date("…and its end (inclusive)."),
                    "available_min": int("0-720."),
                    "time_of_day": json!({ "type": "string", "enum": ["soir", "journee"] }),
                    "context": json!({ "type": "string", "enum": ["travail", "vacances", "autre"] }),
                    "note": text("Max 300 chars."),
                }),
                &["available_min", "time_of_day", "context"],
            ),
        },
        Tool {
            name: "mark_session_skipped",
            description: "Mark an upcoming session as skipped (kept in history, no longer counted in the projection).",
            scope: SCOPE_PROPOSE,
            route: Route::Post("/api/v1/planned/{id}/skip"),
            input: obj(json!({ "id": int("Planned session id."), "reason": text("Optional, max 300 chars.") }), &["id"]),
        },
        Tool {
            name: "log_checkin",
            description: "Record how the athlete feels on a day (replaces that day's previous check-in).",
            scope: SCOPE_PROPOSE,
            route: Route::Put("/api/v1/checkins/{date}"),
            input: obj(
                json!({
                    "date": date("The day."),
                    "legs": int("1 (bad) to 5 (great)."),
                    "motivation": int("1 (bad) to 5 (great)."),
                    "rhr_bpm": int("Resting heart rate."),
                    "note": text("Free text."),
                    "weather": text("Free text."),
                }),
                &["date", "legs", "motivation"],
            ),
        },
        Tool {
            name: "set_coach_note",
            description: "Replace the short message shown at the top of the athlete's dashboard (empty = clear).",
            scope: SCOPE_PROPOSE,
            route: Route::Put("/api/v1/coach-note"),
            input: obj(json!({ "message": text("Max 1000 chars."), "author": text("Shown next to the message.") }), &["message"]),
        },
    ]
}

// ───────────────────────── Errors ─────────────────────────

/// Structured tool error: `code`, `message`, and a `hint` telling the agent what to do next.
fn tool_error(code: &str, message: impl Into<String>, hint: &str, details: Option<Value>) -> Value {
    let mut e = json!({ "code": code, "message": message.into(), "hint": hint });
    if let Some(d) = details {
        e["details"] = d;
    }
    e
}

fn error_from_response(status: StatusCode, body: &Value) -> Value {
    let message = body.get("error").and_then(Value::as_str).unwrap_or("request failed").to_string();
    let details = body.as_object().filter(|o| o.len() > 1 || !o.contains_key("error")).map(|_| body.clone());
    match status {
        StatusCode::BAD_REQUEST => tool_error("invalid_argument", message, "Fix the argument named in the message and retry.", details),
        StatusCode::NOT_FOUND => tool_error("not_found", message, "Check the id (search_workouts, get_plan, list_activities).", details),
        StatusCode::CONFLICT => tool_error("conflict", message, "The target is in a state that does not allow this; re-read it with get_plan.", details),
        StatusCode::UNPROCESSABLE_ENTITY => tool_error(
            "rule_violation",
            message,
            "A hard rule would be newly broken. See details; pick a lighter session or another day (get_ride_envelope, project_load).",
            details,
        ),
        StatusCode::FORBIDDEN => tool_error("forbidden", message, "This credential may not do that.", details),
        _ => tool_error("internal_error", message, "Retry later; if it persists tell the athlete.", None),
    }
}

// ───────────────────────── Rate limiting ─────────────────────────

const WINDOW: Duration = Duration::from_secs(60);
const LIMIT_ALL: usize = 120;
const LIMIT_PROPOSE: usize = 20;

static CALLS: LazyLock<Mutex<HashMap<String, VecDeque<Instant>>>> = LazyLock::new(|| Mutex::new(HashMap::new()));

/// `Some(retry_after_s)` when `client` exceeded its budget for `bucket`.
fn rate_limited(client: &str, bucket: &str, limit: usize) -> Option<u64> {
    let mut map = CALLS.lock().unwrap_or_else(|e| e.into_inner());
    let q = map.entry(format!("{client}|{bucket}")).or_default();
    let now = Instant::now();
    while q.front().is_some_and(|t| now.duration_since(*t) > WINDOW) {
        q.pop_front();
    }
    if q.len() >= limit {
        let wait = WINDOW.saturating_sub(now.duration_since(*q.front().unwrap()));
        return Some(wait.as_secs().max(1));
    }
    q.push_back(now);
    None
}

// ───────────────────────── Dispatch to the REST router ─────────────────────────

async fn call_api(state: &AppState, method: &str, path: &str, query: &[(String, String)], body: Option<&Value>) -> (StatusCode, Value) {
    use tower::ServiceExt;
    let uri = if query.is_empty() {
        path.to_string()
    } else {
        match reqwest::Url::parse_with_params("http://internal", query) {
            Ok(u) => format!("{path}?{}", u.query().unwrap_or("")),
            Err(_) => path.to_string(),
        }
    };
    let mut req = axum::http::Request::builder().method(method).uri(uri);
    let body = match body {
        Some(b) => {
            req = req.header(header::CONTENT_TYPE, "application/json");
            axum::body::Body::from(b.to_string())
        }
        None => axum::body::Body::empty(),
    };
    let Ok(req) = req.body(body) else {
        return (StatusCode::BAD_REQUEST, json!({ "error": "invalid request" }));
    };
    let res = match api_routes().with_state(state.clone()).oneshot(req).await {
        Ok(r) => r,
        Err(never) => match never {},
    };
    let status = res.status();
    let bytes = axum::body::to_bytes(res.into_body(), 16 * 1024 * 1024).await.unwrap_or_default();
    // axum's own rejections (bad query/JSON) are plain text, not our `{error}` shape.
    let value = serde_json::from_slice(&bytes)
        .unwrap_or_else(|_| json!({ "error": String::from_utf8_lossy(&bytes).trim().to_string() }));
    (status, value)
}

fn scalar(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        Value::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}

/// Fills `{placeholders}` of a path from the arguments (consumed from `args`).
fn fill_path(template: &str, args: &mut Map<String, Value>) -> Result<String, String> {
    let mut out = String::new();
    let mut rest = template;
    while let Some(start) = rest.find('{') {
        let end = rest[start..].find('}').map(|e| start + e).ok_or("bad route")?;
        out.push_str(&rest[..start]);
        let key = &rest[start + 1..end];
        let value = args.remove(key).and_then(|v| scalar(&v)).ok_or_else(|| format!("`{key}` is required"))?;
        if value.is_empty() || !value.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
            return Err(format!("`{key}` has an invalid value"));
        }
        out.push_str(&value);
        rest = &rest[end + 1..];
    }
    out.push_str(rest);
    Ok(out)
}

/// Runs one tool; returns the structured result or a structured error.
async fn run_tool(state: &AppState, tool: &Tool, args: Value) -> Result<Value, (Option<StatusCode>, Value)> {
    let invalid = |m: String| (None, tool_error("invalid_argument", m, "Check the tool's input schema.", None));
    let mut args = match args {
        Value::Object(o) => o,
        Value::Null => Map::new(),
        _ => return Err(invalid("arguments must be an object".into())),
    };
    let known = tool.input["properties"].as_object().cloned().unwrap_or_default();
    if let Some(unknown) = args.keys().find(|k| !known.contains_key(*k)) {
        return Err(invalid(format!("unknown argument `{unknown}`")));
    }
    for required in tool.input["required"].as_array().into_iter().flatten().filter_map(Value::as_str) {
        if args.get(required).is_none_or(Value::is_null) {
            return Err(invalid(format!("`{required}` is required")));
        }
    }
    args.retain(|_, v| !v.is_null());

    let finish = |(status, body): (StatusCode, Value)| {
        if status.is_success() {
            Ok(body)
        } else {
            Err((Some(status), error_from_response(status, &body)))
        }
    };
    match tool.route {
        Route::Status => {
            let (s1, summary) = call_api(state, "GET", "/api/v1/summary", &[("days".into(), "7".into())], None).await;
            let (s2, load) = call_api(state, "GET", "/api/v1/load", &[("days".into(), "14".into())], None).await;
            let (s3, planned) = call_api(state, "GET", "/api/v1/planned", &[], None).await;
            let (s4, note) = call_api(state, "GET", "/api/v1/coach-note", &[], None).await;
            for (s, b) in [(s1, &summary), (s2, &load), (s3, &planned), (s4, &note)] {
                if !s.is_success() {
                    return Err((Some(s), error_from_response(s, b)));
                }
            }
            Ok(json!({ "summary": summary, "load": load, "planned": planned, "coach_note": note }))
        }
        Route::Get(template) => {
            let path = fill_path(template, &mut args).map_err(invalid)?;
            let mut query = Vec::new();
            for (k, v) in &args {
                query.push((k.clone(), scalar(v).ok_or_else(|| invalid(format!("`{k}` must be a scalar")))?));
            }
            finish(call_api(state, "GET", &path, &query, None).await)
        }
        Route::Post(template) | Route::Put(template) => {
            let path = fill_path(template, &mut args).map_err(invalid)?;
            let method = if matches!(tool.route, Route::Put(_)) { "PUT" } else { "POST" };
            finish(call_api(state, method, &path, &[], Some(&Value::Object(args))).await)
        }
    }
}

// ───────────────────────── Audit ─────────────────────────

async fn audit(state: &AppState, who: &Principal, tool: &str, args: &Value, status: Option<StatusCode>, code: Option<&str>) {
    let args: String = args.to_string().chars().take(2000).collect();
    let _ = sqlx::query("INSERT INTO mcp_audit (client, tool, args_json, is_error, status, code) VALUES (?, ?, ?, ?, ?, ?)")
        .bind(&who.client)
        .bind(tool)
        .bind(args)
        .bind(code.is_some())
        .bind(status.map(|s| s.as_u16() as i64))
        .bind(code)
        .execute(&state.pool)
        .await;
}

// ───────────────────────── Authentication ─────────────────────────

/// Public base URL of this server (what clients see), without trailing slash:
/// `PUBLIC_BASE_URL` if set, else derived from the request's Host.
pub(super) fn base_url(headers: &HeaderMap) -> String {
    if let Some(url) = std::env::var("PUBLIC_BASE_URL").ok().map(|u| u.trim().trim_end_matches('/').to_string()).filter(|u| !u.is_empty()) {
        return url;
    }
    let host = headers.get(header::HOST).and_then(|h| h.to_str().ok()).unwrap_or("localhost");
    let proto = headers.get("x-forwarded-proto").and_then(|h| h.to_str().ok()).unwrap_or("https");
    format!("{proto}://{host}")
}

fn unauthorized(headers: &HeaderMap, reason: &str) -> Response {
    let challenge = format!(
        r#"Bearer resource_metadata="{}/.well-known/oauth-protected-resource""#,
        base_url(headers)
    );
    (
        StatusCode::UNAUTHORIZED,
        [(header::WWW_AUTHENTICATE, challenge)],
        Json(json!({ "error": reason })),
    )
        .into_response()
}

/// Resolves a bearer token to a principal: an OAuth access token, or (for
/// scripts and tests) a signed RS256 API-key JWT.
async fn authenticate(state: &AppState, token: &str) -> Option<Principal> {
    use crate::apikeys::{authenticate as key_auth, AuthError};
    // OAuth access tokens are opaque; API-key JWTs contain dots.
    if !token.contains('.') {
        let (client, granted) = crate::oauth::verify_access(&state.pool, token).await?;
        let scopes = [SCOPE_READ, SCOPE_PROPOSE].into_iter().filter(|s| granted.iter().any(|g| g == s)).collect();
        return Some(Principal { client, scopes });
    }
    match key_auth(&state.pool, token, true).await {
        Ok(kid) => return Some(Principal { client: format!("key:{kid}"), scopes: vec![SCOPE_READ, SCOPE_PROPOSE] }),
        Err(AuthError::Forbidden) => {
            // Valid read-only key.
            if let Ok(kid) = key_auth(&state.pool, token, false).await {
                return Some(Principal { client: format!("key:{kid}"), scopes: vec![SCOPE_READ] });
            }
        }
        Err(AuthError::Invalid(_)) => {}
    }
    None
}

// ───────────────────────── JSON-RPC ─────────────────────────

fn rpc_error(id: Value, code: i64, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

fn rpc_result(id: Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

fn tool_result(payload: Value, is_error: bool) -> Value {
    json!({
        "content": [{ "type": "text", "text": payload.to_string() }],
        "structuredContent": payload,
        "isError": is_error,
    })
}

async fn handle_message(state: &AppState, who: &Principal, msg: Value) -> Option<Value> {
    let Some(obj) = msg.as_object() else {
        return Some(rpc_error(Value::Null, -32600, "invalid request"));
    };
    let method = obj.get("method").and_then(Value::as_str).unwrap_or("");
    // No `id` → notification: nothing to answer.
    let id = obj.get("id").cloned()?;
    let params = obj.get("params").cloned().unwrap_or(Value::Null);

    Some(match method {
        "initialize" => {
            let asked = params.get("protocolVersion").and_then(Value::as_str).unwrap_or("");
            let version = SUPPORTED_PROTOCOLS.iter().find(|v| **v == asked).copied().unwrap_or(LATEST_PROTOCOL);
            rpc_result(
                id,
                json!({
                    "protocolVersion": version,
                    "capabilities": { "tools": { "listChanged": false } },
                    "serverInfo": { "name": "cyclai", "version": env!("CARGO_PKG_VERSION") },
                    "instructions": INSTRUCTIONS,
                }),
            )
        }
        "ping" => rpc_result(id, json!({})),
        "tools/list" => {
            let list: Vec<Value> = tools()
                .into_iter()
                .filter(|t| who.allows(t.scope))
                .map(|t| json!({ "name": t.name, "description": t.description, "inputSchema": t.input }))
                .collect();
            rpc_result(id, json!({ "tools": list }))
        }
        "tools/call" => {
            let name = params.get("name").and_then(Value::as_str).unwrap_or("");
            let args = params.get("arguments").cloned().unwrap_or(Value::Null);
            let Some(tool) = tools().into_iter().find(|t| t.name == name) else {
                return Some(rpc_error(id, -32602, &format!("unknown tool `{name}`")));
            };
            if !who.allows(tool.scope) {
                let e = tool_error(
                    "insufficient_scope",
                    format!("`{name}` needs the `{}` scope", tool.scope),
                    "Ask the athlete to reconnect CyclAI with this scope.",
                    None,
                );
                audit(state, who, name, &args, Some(StatusCode::FORBIDDEN), Some("insufficient_scope")).await;
                return Some(rpc_result(id, tool_result(e, true)));
            }
            let (bucket, limit) = if tool.scope == SCOPE_PROPOSE { ("propose", LIMIT_PROPOSE) } else { ("all", LIMIT_ALL) };
            let limited = rate_limited(&who.client, "all", LIMIT_ALL).or_else(|| {
                (bucket == "propose").then(|| rate_limited(&who.client, bucket, limit)).flatten()
            });
            if let Some(wait) = limited {
                let e = tool_error("rate_limited", "too many calls", &format!("Wait about {wait} s before retrying."), None);
                audit(state, who, name, &args, Some(StatusCode::TOO_MANY_REQUESTS), Some("rate_limited")).await;
                return Some(rpc_result(id, tool_result(e, true)));
            }
            match run_tool(state, &tool, args.clone()).await {
                Ok(v) => {
                    audit(state, who, name, &args, Some(StatusCode::OK), None).await;
                    rpc_result(id, tool_result(v, false))
                }
                Err((status, e)) => {
                    let code = e["code"].as_str().unwrap_or("error").to_string();
                    audit(state, who, name, &args, status, Some(&code)).await;
                    rpc_result(id, tool_result(e, true))
                }
            }
        }
        _ => rpc_error(id, -32601, &format!("method not found: {method}")),
    })
}

/// `POST /mcp`
pub(super) async fn endpoint(State(state): State<AppState>, headers: HeaderMap, body: axum::body::Bytes) -> Response {
    let token = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "));
    let Some(token) = token else {
        return unauthorized(&headers, "missing bearer token");
    };
    let Some(who) = authenticate(&state, token).await else {
        return unauthorized(&headers, "invalid token");
    };

    let Ok(parsed) = serde_json::from_slice::<Value>(&body) else {
        return Json(rpc_error(Value::Null, -32700, "parse error")).into_response();
    };
    let reply = match parsed {
        Value::Array(batch) => {
            let mut out = Vec::new();
            for m in batch {
                out.extend(handle_message(&state, &who, m).await);
            }
            (!out.is_empty()).then(|| Value::Array(out))
        }
        single => handle_message(&state, &who, single).await,
    };
    match reply {
        Some(v) => Json(v).into_response(),
        None => StatusCode::ACCEPTED.into_response(),
    }
}

/// `GET`/`DELETE /mcp` — no server-initiated stream and no sessions.
/// Without a valid token the answer is the OAuth challenge (clients probe
/// with GET first and need it to start the authorization flow).
pub(super) async fn method_not_allowed(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let token = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "));
    match token {
        None => return unauthorized(&headers, "missing bearer token"),
        Some(t) if authenticate(&state, t).await.is_none() => return unauthorized(&headers, "invalid token"),
        _ => {}
    }
    (StatusCode::METHOD_NOT_ALLOWED, [(header::ALLOW, "POST")]).into_response()
}

#[cfg(test)]
mod tests {
    use super::super::api_router;
    use super::*;
    use crate::apikeys::{add, mint_token, Scope};
    use axum::body::Body;
    use axum::http::Request;
    use sqlx::sqlite::SqlitePoolOptions;
    use tower::ServiceExt;

    const PRIVATE: &str = include_str!("../../tests/fixtures/test_rsa_private.pem");
    const PUBLIC: &str = include_str!("../../tests/fixtures/test_rsa_public.pem");

    async fn setup(scope: Scope) -> (axum::Router, sqlx::SqlitePool, String) {
        let pool = SqlitePoolOptions::new().connect("sqlite::memory:").await.unwrap();
        crate::db::migrate(&pool).await.unwrap();
        crate::db::seed_athlete_config(
            &pool,
            &crate::config::Config {
                database_url: "sqlite::memory:".into(),
                anthropic_api_key: String::new(),
                ftp_watts: 200,
                hr_max: 190,
                hr_rest: 50,
                athlete_profile: String::new(),
            },
        )
        .await
        .unwrap();
        add(&pool, "k", PUBLIC, scope).await.unwrap();
        let token = mint_token(PRIVATE, "k", 60).unwrap();
        (api_router(AppState { pool: pool.clone() }), pool, token)
    }

    async fn rpc(app: &axum::Router, token: Option<&str>, body: Value) -> (StatusCode, Value) {
        let mut req = Request::builder().method("POST").uri("/mcp").header("content-type", "application/json");
        if let Some(t) = token {
            req = req.header("authorization", format!("Bearer {t}"));
        }
        let res = app.clone().oneshot(req.body(Body::from(body.to_string())).unwrap()).await.unwrap();
        let status = res.status();
        let bytes = axum::body::to_bytes(res.into_body(), usize::MAX).await.unwrap();
        (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
    }

    fn call_tool(id: i64, name: &str, args: Value) -> Value {
        json!({ "jsonrpc": "2.0", "id": id, "method": "tools/call", "params": { "name": name, "arguments": args } })
    }

    #[tokio::test]
    async fn mcp_requires_a_token_and_points_to_the_oauth_metadata() {
        let (app, _, _) = setup(Scope::Read).await;
        let req = Request::builder().method("POST").uri("/mcp").body(Body::from("{}")).unwrap();
        let res = app.clone().oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
        let challenge = res.headers()[header::WWW_AUTHENTICATE].to_str().unwrap();
        assert!(challenge.contains("resource_metadata=") && challenge.contains("/.well-known/oauth-protected-resource"), "{challenge}");
        assert_eq!(rpc(&app, Some("garbage"), json!({})).await.0, StatusCode::UNAUTHORIZED);
        let get = Request::builder().method("GET").uri("/mcp").body(Body::empty()).unwrap();
        let res = app.clone().oneshot(get).await.unwrap();
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
        assert!(res.headers().contains_key(header::WWW_AUTHENTICATE));
    }

    #[tokio::test]
    async fn mcp_handshake_lists_tools_by_scope_and_ignores_notifications() {
        let (app, _, token) = setup(Scope::Read).await;
        let (s, init) = rpc(&app, Some(&token), json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": { "protocolVersion": "2025-03-26" } })).await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(init["result"]["protocolVersion"], "2025-03-26");
        assert_eq!(init["result"]["serverInfo"]["name"], "cyclai");
        assert!(init["result"]["instructions"].as_str().unwrap().contains("Nothing can be deleted"));

        let (s, _) = rpc(&app, Some(&token), json!({ "jsonrpc": "2.0", "method": "notifications/initialized" })).await;
        assert_eq!(s, StatusCode::ACCEPTED);

        let (_, list) = rpc(&app, Some(&token), json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list" })).await;
        let names: Vec<&str> = list["result"]["tools"].as_array().unwrap().iter().map(|t| t["name"].as_str().unwrap()).collect();
        assert!(names.contains(&"search_workouts") && names.contains(&"get_ride_envelope"));
        assert!(!names.contains(&"propose_session"), "a read key must not even see write tools: {names:?}");
        assert!(!names.iter().any(|n| n.contains("delete")), "no deletion tool");
        assert!(list["result"]["tools"][0]["inputSchema"]["type"] == "object");

        let (_, unknown) = rpc(&app, Some(&token), json!({ "jsonrpc": "2.0", "id": 3, "method": "nope" })).await;
        assert_eq!(unknown["error"]["code"], -32601);

        // A batch answers only its requests.
        let (_, batch) = rpc(
            &app,
            Some(&token),
            json!([
                { "jsonrpc": "2.0", "id": 4, "method": "ping" },
                { "jsonrpc": "2.0", "method": "notifications/initialized" },
            ]),
        )
        .await;
        assert_eq!(batch.as_array().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn mcp_tools_run_through_the_api_with_scopes_errors_and_audit() {
        let (app, pool, token) = setup(Scope::Write).await;
        let step = r#"{"type":"interval","duration":{"type":"time","value":"60min"},"target":{"type":"ftp_pct_range","low":65,"high":65}}"#;
        sqlx::query("INSERT INTO workouts (id, name, steps_json) VALUES (1, 'Z2', ?)")
            .bind(format!(r#"{{"name":"Z2","steps":[{step}]}}"#))
            .execute(&pool)
            .await
            .unwrap();
        let day = (chrono::Utc::now() - chrono::Duration::days(5)).format("%Y-%m-%dT08:00:00Z").to_string();
        sqlx::query("INSERT INTO activities (id, started_at, duration_s, distance_m, elevation_gain_m, tss, intensity_factor) VALUES (1, ?, 3600, 30000, 100, 60, 0.7)")
            .bind(day)
            .execute(&pool)
            .await
            .unwrap();
        crate::metrics::recompute_daily_load(&pool).await.unwrap();
        let tomorrow = (chrono::Utc::now().date_naive() + chrono::Duration::days(1)).to_string();

        let structured = |v: &Value| v["result"]["structuredContent"].clone();

        // Reading tools.
        let (_, found) = rpc(&app, Some(&token), call_tool(1, "search_workouts", json!({ "tag": "endurance" }))).await;
        assert_eq!(found["result"]["isError"], false, "{found}");
        assert_eq!(structured(&found)["workouts"][0]["name"], "Z2");
        let (_, status) = rpc(&app, Some(&token), call_tool(2, "get_status", json!({}))).await;
        assert!(structured(&status)["load"]["series"].is_array() && structured(&status)["summary"].is_object(), "{status}");

        // Proposal through the engine: stored as `proposed`, idempotent.
        let args = json!({ "date": tomorrow, "workout_id": 1, "rationale": "easy day", "idempotency_key": "x" });
        let (_, p1) = rpc(&app, Some(&token), call_tool(3, "propose_session", args.clone())).await;
        assert_eq!(p1["result"]["isError"], false, "{p1}");
        assert_eq!(structured(&p1)["status"], "proposed");
        let (_, p2) = rpc(&app, Some(&token), call_tool(4, "propose_session", args)).await;
        assert_eq!(structured(&p2)["id"], structured(&p1)["id"]);

        // Structured errors with code + hint.
        let (_, bad) = rpc(&app, Some(&token), call_tool(5, "propose_session", json!({ "date": "nope", "workout_id": 1, "rationale": "r" }))).await;
        assert_eq!(bad["result"]["isError"], true);
        assert_eq!(structured(&bad)["code"], "invalid_argument");
        assert!(structured(&bad)["hint"].is_string() && structured(&bad)["message"].is_string());
        let (_, missing) = rpc(&app, Some(&token), call_tool(6, "get_workout", json!({ "id": 999 }))).await;
        assert_eq!(structured(&missing)["code"], "not_found");
        let (_, unknown_arg) = rpc(&app, Some(&token), call_tool(7, "get_workout", json!({ "id": 1, "oops": 1 }))).await;
        assert_eq!(structured(&unknown_arg)["code"], "invalid_argument");
        let (_, no_id) = rpc(&app, Some(&token), call_tool(8, "get_workout", json!({}))).await;
        assert_eq!(structured(&no_id)["code"], "invalid_argument");
        let (_, nope) = rpc(&app, Some(&token), call_tool(9, "nope", json!({}))).await;
        assert_eq!(nope["error"]["code"], -32602);

        // Hard rules apply: a huge bare-TSS projection reports violations without writing.
        let (_, proj) = rpc(&app, Some(&token), call_tool(10, "project_load", json!({ "sessions": [{ "date": tomorrow, "tss": 400 }] }))).await;
        assert_eq!(structured(&proj)["ok"], false, "{proj}");

        // Check-in and skip go through the same validation as REST.
        let (_, ci) = rpc(&app, Some(&token), call_tool(11, "log_checkin", json!({ "date": tomorrow, "legs": 4, "motivation": 5 }))).await;
        assert_eq!(ci["result"]["isError"], false, "{ci}");
        let (_, ci_bad) = rpc(&app, Some(&token), call_tool(12, "log_checkin", json!({ "date": tomorrow, "legs": 9, "motivation": 5 }))).await;
        assert_eq!(structured(&ci_bad)["code"], "invalid_argument");
        let id = structured(&p1)["id"].as_i64().unwrap();
        let (_, skip) = rpc(&app, Some(&token), call_tool(13, "mark_session_skipped", json!({ "id": id, "reason": "malade" }))).await;
        assert_eq!(structured(&skip)["status"], "skipped", "{skip}");

        // Everything was audited, errors included.
        let (total, errors): (i64, i64) = sqlx::query_as("SELECT COUNT(*), COALESCE(SUM(is_error), 0) FROM mcp_audit").fetch_one(&pool).await.unwrap();
        assert!(total >= 12 && errors >= 4, "{total} {errors}");
        let client: String = sqlx::query_scalar("SELECT client FROM mcp_audit LIMIT 1").fetch_one(&pool).await.unwrap();
        assert_eq!(client, "key:k");
    }

    #[tokio::test]
    async fn mcp_read_credentials_cannot_propose_and_calls_are_rate_limited() {
        let (app, pool, token) = setup(Scope::Read).await;
        let (_, denied) = rpc(&app, Some(&token), call_tool(1, "propose_session", json!({ "date": "2030-01-01", "workout_id": 1, "rationale": "r" }))).await;
        assert_eq!(denied["result"]["isError"], true);
        assert_eq!(denied["result"]["structuredContent"]["code"], "insufficient_scope");
        let planned: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM planned_workouts").fetch_one(&pool).await.unwrap();
        assert_eq!(planned, 0);

        // A dedicated client name so the shared limiter doesn't leak between tests.
        for _ in 0..LIMIT_ALL + 5 {
            rate_limited("rate-test", "all", LIMIT_ALL);
        }
        assert!(rate_limited("rate-test", "all", LIMIT_ALL).is_some());
        assert!(rate_limited("someone-else", "all", LIMIT_ALL).is_none());
    }
}
