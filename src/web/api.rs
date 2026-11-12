//! JSON API (`/api/v1/...`) over the data the dashboard shows, for external
//! tools (home dashboards, scripts, other assistants). Everything is read-only
//! except scheduling: `POST/DELETE /planned` put sessions on the calendar.
//! Served by `web::serve_api` only, behind the RS256 bearer-token guard in
//! `web::auth`.

use super::detail::{HR_ZONE_BOUNDS, HR_ZONE_NAMES};
use super::{tsb_zone_index, AppState, TSB_ZONES, ZONE_BOUNDS, ZONE_NAMES};
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Value};
use sqlx::SqlitePool;

pub(super) struct ApiError(StatusCode, String);

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.0, Json(json!({ "error": self.1 }))).into_response()
    }
}

impl From<anyhow::Error> for ApiError {
    fn from(err: anyhow::Error) -> Self {
        tracing::error!(?err, "api request failed");
        ApiError(StatusCode::INTERNAL_SERVER_ERROR, "internal error".to_string())
    }
}

impl From<sqlx::Error> for ApiError {
    fn from(err: sqlx::Error) -> Self {
        anyhow::Error::from(err).into()
    }
}

type ApiResult = Result<Json<Value>, ApiError>;

fn round(v: f64, digits: i32) -> f64 {
    let p = 10f64.powi(digits);
    // `+ 0.0` turns -0.0 (a tiny negative rounded away) into plain 0.0.
    (v * p).round() / p + 0.0
}

/// ISO date `days` days before today (UTC) — `started_at` is ISO-8601, so a
/// plain string comparison against it is exact and can use the index.
fn days_ago(days: i64) -> String {
    (chrono::Utc::now().date_naive() - chrono::Duration::days(days)).to_string()
}

/// `GET /api/v1/profile` — FTP/HR settings with the derived zone tables, plus
/// the current training load (CTL/ATL/TSB) from the precomputed summary.
pub(super) async fn profile(State(state): State<AppState>) -> ApiResult {
    let (ftp, hr_max, hr_rest): (i64, i64, i64) =
        sqlx::query_as("SELECT ftp_watts, hr_max, hr_rest FROM athlete_config WHERE id = 1")
            .fetch_one(&state.pool)
            .await?;
    let load = crate::metrics::dashboard_summary(&state.pool).await?;
    let hr_cfg = crate::metrics::get_hr_config(&state.pool).await?;
    let ath = crate::workout::Athlete::load(&state.pool).await;

    let power_zones: Vec<Value> = (0..7)
        .map(|i| {
            let high = ZONE_BOUNDS[i + 1];
            json!({
                "zone": i + 1,
                "name": ZONE_NAMES[i],
                "min_w": (ZONE_BOUNDS[i] * ftp as f64).round(),
                "max_w": if high.is_finite() { json!((high * ftp as f64).round()) } else { Value::Null },
            })
        })
        .collect();

    let hrr = (hr_max - hr_rest) as f64;
    let hr_zones: Vec<Value> = (0..5)
        .map(|i| {
            let high = HR_ZONE_BOUNDS[i + 1];
            json!({
                "zone": i + 1,
                "name": HR_ZONE_NAMES[i],
                "min_bpm": (hr_rest as f64 + HR_ZONE_BOUNDS[i] * hrr).round(),
                "max_bpm": if high.is_finite() { json!((hr_rest as f64 + high * hrr).round()) } else { Value::Null },
            })
        })
        .collect();

    let total_zone_s: i64 = load.zone_secs.iter().sum();
    Ok(Json(json!({
        "ftp_watts": ftp,
        "hr_max": hr_max,
        "hr_rest": hr_rest,
        "hr_threshold_bpm": hr_cfg.lthr.round(),
        "hr_tss_coefficient": hr_cfg.tss_coef,
        "run_threshold_pace": if ath.run_threshold_pace_s > 0.0 { Some(super::format::format_pace_secs(ath.run_threshold_pace_s)) } else { None },
        "run_threshold_pace_s_per_km": if ath.run_threshold_pace_s > 0.0 { Some(ath.run_threshold_pace_s) } else { None },
        "power_zones": power_zones,
        "hr_zones": hr_zones,
        "load": {
            "ctl": round(load.ctl, 1),
            "atl": round(load.atl, 1),
            "tsb": round(load.tsb, 1),
            "tsb_status": TSB_ZONES[tsb_zone_index(load.tsb)].1,
            "rest_days_to_next_tsb_band": load.rest_days_to_next_band,
            "tss_28d": round(load.tss_period, 0),
            "power_zone_seconds_28d": load.zone_secs,
            "power_zone_pct_28d": load.zone_secs.iter()
                .map(|s| if total_zone_s > 0 { round(*s as f64 / total_zone_s as f64 * 100.0, 1) } else { 0.0 })
                .collect::<Vec<_>>(),
        },
    })))
}

#[derive(Deserialize)]
pub(super) struct LoadQuery {
    days: Option<i64>,
}

/// `GET /api/v1/load?days=90` — daily TSS/CTL/ATL/TSB series, oldest first.
pub(super) async fn load(State(state): State<AppState>, Query(q): Query<LoadQuery>) -> ApiResult {
    let days = q.days.unwrap_or(90).clamp(1, 1500);
    // Goes through the summary first so daily_load is topped up to today
    // (a rest streak since the last sync is part of the series).
    crate::metrics::dashboard_summary(&state.pool).await?;
    let rows: Vec<(String, f64, f64, f64, f64)> = sqlx::query_as(
        "SELECT date, tss, ctl, atl, tsb FROM daily_load WHERE date >= ? ORDER BY date",
    )
    .bind(days_ago(days - 1))
    .fetch_all(&state.pool)
    .await?;
    let series: Vec<Value> = rows
        .into_iter()
        .map(|(date, tss, ctl, atl, tsb)| {
            json!({ "date": date, "tss": round(tss, 1), "ctl": round(ctl, 1), "atl": round(atl, 1), "tsb": round(tsb, 1) })
        })
        .collect();
    Ok(Json(json!({ "days": days, "series": series })))
}

#[derive(sqlx::FromRow)]
struct ActivityRecord {
    id: i64,
    sport_type: String,
    started_at: String,
    title: Option<String>,
    distance_m: f64,
    duration_s: i64,
    elevation_gain_m: f64,
    avg_power_w: Option<f64>,
    normalized_power_w: Option<f64>,
    avg_hr: Option<f64>,
    max_hr: Option<f64>,
    tss: Option<f64>,
    tss_source: Option<String>,
    intensity_factor: Option<f64>,
    zone1_s: i64,
    zone2_s: i64,
    zone3_s: i64,
    zone4_s: i64,
    zone5_s: i64,
    zone6_s: i64,
    zone7_s: i64,
}

const ACTIVITY_COLUMNS: &str = "id, sport_type, started_at, title, distance_m, duration_s, elevation_gain_m, \
     avg_power_w, normalized_power_w, avg_hr, max_hr, tss, tss_source, intensity_factor, \
     zone1_s, zone2_s, zone3_s, zone4_s, zone5_s, zone6_s, zone7_s";

/// Where the power figures come from: `none` (no power meter data kept),
/// `trainer` (indoor ride — smart trainer) or `pedals` (outdoor power meter).
/// Sources only say whether power comes from a device, so this is derived from the sport type.
fn power_source(a: &ActivityRecord) -> &'static str {
    match (a.avg_power_w.is_some(), a.sport_type == "VirtualRide") {
        (false, _) => "none",
        (true, true) => "trainer",
        (true, false) => "pedals",
    }
}

/// `coef` = the hrTSS weight (see `metrics::HrConfig::tss_coef`): `tss` stays
/// the activity's own score, `tss_load` is what it adds to the total load.
fn activity_json(a: &ActivityRecord, coef: f64) -> Value {
    let is_run = crate::metrics::is_run(&a.sport_type);
    let km = a.distance_m / 1000.0;
    // Runs are expressed as pace (min/km), rides as speed (km/h).
    let pace = if is_run { super::format::format_pace(a.distance_m, a.duration_s) } else { None };
    let speed = if !is_run && a.duration_s > 0 {
        Some(round(km / (a.duration_s as f64 / 3600.0), 1))
    } else {
        None
    };
    json!({
        "id": a.id,
        "sport": if is_run { "run" } else { "ride" },
        "sport_type": a.sport_type,
        "indoor": a.sport_type == "VirtualRide",
        "power_source": power_source(a),
        "has_hr": a.avg_hr.is_some(),
        "started_at": a.started_at,
        "title": a.title,
        "distance_km": round(km, 2),
        "duration_s": a.duration_s,
        "elevation_gain_m": a.elevation_gain_m.round(),
        "avg_pace_min_per_km": pace,
        "avg_speed_kmh": speed,
        "avg_power_w": a.avg_power_w.map(|v| v.round()),
        "normalized_power_w": a.normalized_power_w.map(|v| v.round()),
        "avg_hr": a.avg_hr.map(|v| v.round()),
        "max_hr": a.max_hr.map(|v| v.round()),
        "tss": a.tss.map(|v| round(v, 0)),
        "tss_source": a.tss_source,
        "tss_load": a.tss.map(|v| round(if a.tss_source.as_deref() == Some("hr") { v * coef } else { v }, 0)),
        "intensity_factor": a.intensity_factor.map(|v| round(v, 2)),
        "power_zone_seconds": [a.zone1_s, a.zone2_s, a.zone3_s, a.zone4_s, a.zone5_s, a.zone6_s, a.zone7_s],
    })
}

#[derive(Deserialize)]
pub(super) struct ActivitiesQuery {
    limit: Option<i64>,
    days: Option<i64>,
    /// `ride` or `run`; anything else (or absent) means both.
    sport: Option<String>,
}

/// `GET /api/v1/activities?limit=10&days=28&sport=run` — latest activities,
/// newest first.
pub(super) async fn activities(State(state): State<AppState>, Query(q): Query<ActivitiesQuery>) -> ApiResult {
    let limit = q.limit.unwrap_or(10).clamp(1, 200);
    // No `days` means no lower bound: every started_at sorts after "".
    let since = q.days.map(|d| days_ago(d.clamp(1, 3650))).unwrap_or_default();
    let sport_filter = match q.sport.as_deref() {
        Some("ride") => "AND sport_type IN ('Ride', 'VirtualRide')",
        Some("run") => "AND sport_type IN ('Run', 'TrailRun')",
        _ => "",
    };
    let rows: Vec<ActivityRecord> = sqlx::query_as(&format!(
        "SELECT {ACTIVITY_COLUMNS} FROM activities WHERE started_at >= ? {sport_filter} \
         ORDER BY started_at DESC LIMIT ?"
    ))
    .bind(since)
    .bind(limit)
    .fetch_all(&state.pool)
    .await?;
    let coef = crate::metrics::get_hr_config(&state.pool).await?.tss_coef;
    let items: Vec<Value> = rows.iter().map(|r| activity_json(r, coef)).collect();
    Ok(Json(json!({ "count": items.len(), "activities": items })))
}

/// `GET /api/v1/activities/{id}` — one activity with its laps, local segment
/// efforts and the generated bilan (if any).
pub(super) async fn activity(State(state): State<AppState>, Path(id): Path<i64>) -> ApiResult {
    let row: Option<ActivityRecord> =
        sqlx::query_as(&format!("SELECT {ACTIVITY_COLUMNS} FROM activities WHERE id = ?"))
            .bind(id)
            .fetch_optional(&state.pool)
            .await?;
    let Some(row) = row else {
        return Err(ApiError(StatusCode::NOT_FOUND, format!("activity {id} not found")));
    };
    let coef = crate::metrics::get_hr_config(&state.pool).await?.tss_coef;
    let mut out = activity_json(&row, coef);
    let is_run = crate::metrics::is_run(&row.sport_type);

    let laps: Vec<(i64, i64, f64, Option<f64>, Option<f64>)> = sqlx::query_as(
        "SELECT lap_index, elapsed_time_s, distance_m, avg_power_w, avg_hr FROM laps \
         WHERE activity_id = ? ORDER BY lap_index",
    )
    .bind(id)
    .fetch_all(&state.pool)
    .await?;

    // Raw streams give per-lap and whole-ride detail beyond what a source's
    // lap summary carries; without them (rare) laps fall back to the stored
    // summary values and `performance` is null.
    let ath = current_athlete(&state.pool).await?;
    let series = load_series(&state.pool, id, is_run).await?;

    let mut cursor_s = 0usize;
    let mut lap_items = Vec::with_capacity(laps.len());
    for (index, elapsed_s, distance_m, power, hr) in laps {
        let start = cursor_s;
        cursor_s += elapsed_s.max(0) as usize;
        let stats = series.as_ref().map(|s| crate::performance::window_stats(s, start, cursor_s, &ath));
        lap_items.push(lap_json(index, start, elapsed_s, distance_m, power, hr, stats.as_ref(), is_run, ath.ftp_watts));
    }
    out["laps"] = Value::Array(lap_items);
    out["performance"] = match &series {
        Some(s) => performance_json(s, is_run, &ath),
        None => Value::Null,
    };

    let efforts: Vec<(String, f64, Option<f64>, Option<f64>, Option<f64>)> = sqlx::query_as(
        "SELECT rs.name, se.avg_power_w, se.delta_vs_pr_pct, se.delta_vs_recent_avg_pct, se.delta_vs_last_pct \
         FROM segment_efforts se JOIN reference_segments rs ON rs.id = se.reference_segment_id \
         WHERE se.activity_id = ?",
    )
    .bind(id)
    .fetch_all(&state.pool)
    .await?;
    out["segment_efforts"] = Value::Array(
        efforts
            .into_iter()
            .map(|(name, power, vs_pr, vs_recent, vs_last)| {
                json!({
                    "segment": name,
                    "avg_power_w": round(power, 0),
                    "delta_vs_pr_pct": vs_pr.map(|v| round(v, 1)),
                    "delta_vs_recent_avg_pct": vs_recent.map(|v| round(v, 1)),
                    "delta_vs_last_pct": vs_last.map(|v| round(v, 1)),
                })
            })
            .collect(),
    );

    let bilan: Option<String> = sqlx::query_scalar("SELECT text FROM activity_bilans WHERE activity_id = ?")
        .bind(id)
        .fetch_optional(&state.pool)
        .await?;
    out["bilan"] = json!(bilan);
    Ok(Json(out))
}

#[derive(Deserialize)]
pub(super) struct SummaryQuery {
    days: Option<i64>,
}

/// `GET /api/v1/summary?days=28` — synthesis of recent training: totals,
/// per-sport totals and a Monday-based weekly breakdown over the window.
pub(super) async fn summary(State(state): State<AppState>, Query(q): Query<SummaryQuery>) -> ApiResult {
    let days = q.days.unwrap_or(28).clamp(1, 3650);
    let since = days_ago(days - 1);
    // hrTSS counts towards `tss` weighted by the configured coefficient, the
    // same way it does in the training load.
    let coef = crate::metrics::get_hr_config(&state.pool).await?.tss_coef;

    let totals = aggregate(&state.pool, &since, None, coef).await?;

    let sports: Vec<(String,)> = sqlx::query_as(
        "SELECT DISTINCT sport_type FROM activities WHERE started_at >= ? ORDER BY sport_type",
    )
    .bind(&since)
    .fetch_all(&state.pool)
    .await?;
    let mut by_sport = serde_json::Map::new();
    for (sport,) in sports {
        let agg = aggregate(&state.pool, &since, Some(&sport), coef).await?;
        by_sport.insert(sport, agg);
    }

    // 'weekday 0' jumps forward to the next Sunday (or stays on one), so
    // minus 6 days is always that week's Monday.
    let weeks: Vec<(String, i64, f64, i64, f64, f64)> = sqlx::query_as(&format!(
        "SELECT date(started_at, 'weekday 0', '-6 days') AS week_start, COUNT(*), SUM(distance_m), \
                SUM(duration_s), SUM(elevation_gain_m), COALESCE(SUM({}), 0.0) \
         FROM activities WHERE started_at >= ?2 GROUP BY week_start ORDER BY week_start",
        crate::metrics::load_tss_sql(1)
    ))
    .bind(coef)
    .bind(&since)
    .fetch_all(&state.pool)
    .await?;

    let latest: Option<ActivityRecord> = sqlx::query_as(&format!(
        "SELECT {ACTIVITY_COLUMNS} FROM activities ORDER BY started_at DESC LIMIT 1"
    ))
    .fetch_optional(&state.pool)
    .await?;

    Ok(Json(json!({
        "days": days,
        "since": since,
        "totals": totals,
        "by_sport": by_sport,
        "by_week": weeks
            .into_iter()
            .map(|(week_start, n, dist, dur, elev, tss)| json!({
                "week_start": week_start,
                "activities": n,
                "distance_km": round(dist / 1000.0, 1),
                "duration_s": dur,
                "elevation_gain_m": elev.round(),
                "tss": round(tss, 0),
            }))
            .collect::<Vec<_>>(),
        "latest_activity": latest.as_ref().map(|a| activity_json(a, coef)),
    })))
}

/// Totals over `[since, now]`, optionally for one `sport_type` only.
async fn aggregate(pool: &SqlitePool, since: &str, sport: Option<&str>, coef: f64) -> Result<Value, ApiError> {
    let (n, dist, dur, elev, tss, avg_if): (i64, f64, i64, f64, f64, Option<f64>) = sqlx::query_as(&format!(
        "SELECT COUNT(*), COALESCE(SUM(distance_m), 0.0), COALESCE(SUM(duration_s), 0), \
                COALESCE(SUM(elevation_gain_m), 0.0), COALESCE(SUM({}), 0.0), AVG(intensity_factor) \
         FROM activities WHERE started_at >= ?1 AND (?2 IS NULL OR sport_type = ?2)",
        crate::metrics::load_tss_sql(3)
    ))
    .bind(since)
    .bind(sport)
    .bind(coef)
    .fetch_one(pool)
    .await?;
    Ok(json!({
        "activities": n,
        "distance_km": round(dist / 1000.0, 1),
        "duration_s": dur,
        "elevation_gain_m": elev.round(),
        "tss": round(tss, 0),
        "avg_intensity_factor": avg_if.map(|v| round(v, 2)),
    }))
}

// ---------------------------------------------------------------------------
// Workout templates and planned (calendar) sessions
// ---------------------------------------------------------------------------

fn planned_json(p: &crate::planned::Planned, ath: &crate::workout::Athlete) -> Value {
    let workout: Value = serde_json::from_str(&p.steps_json).unwrap_or(Value::Null);
    let (minutes, tss) = p.load_estimate(ath);
    let free_ride_budget = p.is_free_ride().then(|| {
        json!({
            "tss_range": [p.budget_tss_min, p.budget_tss_max],
            "duration_max_min": p.budget_duration_max_min,
            "intent": p.intent,
        })
    });
    json!({
        "id": p.id,
        "date": p.planned_date,
        "name": p.name,
        "kind": p.kind,
        "free_ride_budget": free_ride_budget,
        "activity_id": p.activity_id,
        "notes": p.notes,
        "workout_id": p.workout_id,
        "source": p.source,
        "sport": p.sport,
        "status": p.status,
        "rationale": p.rationale,
        "alternatives": p.alternatives(),
        "duration_min": round(minutes, 1),
        "estimated_tss": round(tss, 0),
        "estimated_if": crate::rules::intensity_factor(minutes, tss).map(|f| round(f, 2)),
        "estimated_distance_km": workout.get("estimated_distance_km"),
        "structure_summary": serde_json::from_value::<crate::workout::Workout>(workout.clone()).ok().map(|w| w.structure_summary()),
        "workout": workout,
    })
}

fn rules_json(r: &crate::rules::Rules) -> Value {
    json!({
        "max_ctl_ramp_per_week": r.max_ctl_ramp_per_week, "min_tsb": r.min_tsb,
        "intense_if": r.intense_if, "intense_tss": r.intense_tss,
        "max_consecutive_intense": r.max_consecutive_intense,
        "min_rest_days_per_week": r.min_rest_days_per_week,
    })
}

/// `GET /api/v1/rules` — the hard constraints the server enforces (editable in /settings).
pub(super) async fn rules(State(state): State<AppState>) -> ApiResult {
    Ok(Json(rules_json(&crate::rules::Rules::load(&state.pool).await?)))
}

#[derive(Deserialize)]
pub(super) struct ProjectionSession {
    date: String,
    workout_id: Option<i64>,
    tss: Option<f64>,
    /// Optional with `tss`: lets the intense-day rule apply to a bare TSS.
    intensity_factor: Option<f64>,
}

#[derive(Deserialize)]
pub(super) struct ProjectionRequest {
    #[serde(default)]
    sessions: Vec<ProjectionSession>,
    /// Also count the sessions already on the calendar (`planned` and
    /// `confirmed`; not proposed/skipped/rejected). Default true.
    include_planned: Option<bool>,
}

/// First day whose planned sessions still count towards the load: today,
/// unless a real ride is already logged today (the planned session of the
/// day is then assumed to be that ride, so it isn't counted twice).
fn projection_start(past: &[crate::rules::PastDay]) -> chrono::NaiveDate {
    match past.last() {
        Some(d) if d.tss >= crate::rules::REST_TSS_MAX => d.date + chrono::Duration::days(1),
        Some(d) => d.date,
        None => chrono::Utc::now().date_naive(),
    }
}

/// Sessions from `from` to `horizon` that count towards the load: `planned`
/// and `confirmed` ones (a proposal, skipped or rejected session doesn't).
async fn calendar_sessions(
    pool: &SqlitePool,
    ath: &crate::workout::Athlete,
    from: chrono::NaiveDate,
    horizon: chrono::NaiveDate,
) -> Result<Vec<crate::rules::FutureSession>, ApiError> {
    let mut out = Vec::new();
    for p in crate::planned::list_between(pool, from, horizon).await? {
        if !matches!(p.status.as_str(), "planned" | "confirmed") {
            continue;
        }
        let Some(date) = crate::planned::parse_date(&p.planned_date) else { continue };
        let (minutes, tss) = p.load_estimate(ath);
        out.push(crate::rules::FutureSession { date, tss, intensity_factor: crate::rules::intensity_factor(minutes, tss) });
    }
    Ok(out)
}

#[derive(Deserialize)]
pub(super) struct ProposalRequest {
    /// `YYYY-MM-DD`, today or later.
    date: String,
    /// A library session… (give this or `free_ride_budget`)
    workout_id: Option<i64>,
    /// …or a free ride bounded by a budget.
    free_ride_budget: Option<FreeRideBudgetRequest>,
    /// Why this session on this day (shown to the user when they validate it).
    rationale: String,
    /// Retrying with the same key returns the first proposal instead of a duplicate.
    idempotency_key: Option<String>,
    /// Up to 3 other library sessions the athlete may prefer (only with `workout_id`).
    /// Each is checked against the hard rules like the main one.
    #[serde(default)]
    alternatives: Vec<AlternativeRequest>,
}

#[derive(Deserialize)]
pub(super) struct AlternativeRequest {
    workout_id: i64,
    /// Why it is an alternative (what it trades off). Max 200 characters.
    note: Option<String>,
}

const MAX_ALTERNATIVES: usize = 3;

/// A library session, ready to be stored.
async fn load_library(
    pool: &SqlitePool,
    ath: &crate::workout::Athlete,
    workout_id: i64,
) -> Result<(String, Value, crate::planned::Prepared, crate::planned::Sport), ApiError> {
    let row: Option<(String, String)> = sqlx::query_as("SELECT steps_json, sport FROM workouts WHERE id = ?")
        .bind(workout_id)
        .fetch_optional(pool)
        .await?;
    let (steps, sport) = row.ok_or_else(|| ApiError(StatusCode::NOT_FOUND, format!("workout template {workout_id} not found")))?;
    let workout: Value = serde_json::from_str(&steps).unwrap_or(Value::Null);
    let sport = crate::planned::Sport::from_db(&sport);
    let prepared = crate::planned::prepare(&workout, ath, sport).map_err(|m| ApiError(StatusCode::BAD_REQUEST, m))?;
    Ok((steps, workout, prepared, sport))
}

/// Refuses only what a session adds: a violation already present in the
/// calendar isn't the proposal's fault.
fn refuse_if_it_breaks_rules(
    past: &[crate::rules::PastDay],
    base: &[crate::rules::FutureSession],
    rules: &crate::rules::Rules,
    date: chrono::NaiveDate,
    minutes: f64,
    tss: f64,
    what: &str,
) -> Result<(), ApiError> {
    let mut with = base.to_vec();
    with.push(crate::rules::FutureSession { date, tss, intensity_factor: crate::rules::intensity_factor(minutes, tss) });
    let before = crate::rules::simulate(past, base, rules).violations;
    let added: Vec<_> = crate::rules::simulate(past, &with, rules)
        .violations
        .into_iter()
        .filter(|v| !before.iter().any(|b| b.rule == v.rule && b.date == v.date))
        .collect();
    if added.is_empty() {
        return Ok(());
    }
    Err(ApiError(
        StatusCode::UNPROCESSABLE_ENTITY,
        format!(
            "{what} refused, it would break: {}",
            added.iter().map(|v| format!("{} ({}: {})", v.rule, v.date, v.message)).collect::<Vec<_>>().join("; ")
        ),
    ))
}

#[derive(Deserialize)]
pub(super) struct FreeRideBudgetRequest {
    /// `[min, max]` TSS; filled in by the engine when omitted.
    tss_range: Option<[f64; 2]>,
    duration_max_min: i64,
    /// `endurance` | `denivele` | `recuperation` | `tempo` | `intense` | `course`
    intent: String,
    /// Real intensity of the ride (0.3-1.2), for a race or a fast group ride.
    /// Replaces the intent's ceiling; exclusive with `tss_range`.
    expected_if: Option<f64>,
}

/// What a proposal adds to the calendar, validated but not yet stored.
enum Candidate {
    Library { workout: Value, prepared: crate::planned::Prepared, sport: crate::planned::Sport, workout_id: i64 },
    FreeRide(crate::planned::FreeRideBudget),
}

/// `POST /api/v1/proposals` — suggests a session for a day. It is stored as
/// `proposed` (the user confirms or rejects it in the dashboard) and refused
/// with 422 when it would introduce a hard-rule violation.
pub(super) async fn proposal_create(
    State(state): State<AppState>,
    Json(req): Json<ProposalRequest>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    let bad = |msg: &str| ApiError(StatusCode::BAD_REQUEST, msg.to_string());
    let ath = current_athlete(&state.pool).await?;

    let key = req.idempotency_key.as_deref().map(str::trim).filter(|k| !k.is_empty());
    if key.is_some_and(|k| k.chars().count() > 100) {
        return Err(bad("`idempotency_key` is limited to 100 characters"));
    }
    if let Some(existing) = match key {
        Some(k) => crate::planned::find_by_idempotency_key(&state.pool, k).await?,
        None => None,
    } {
        return Ok((StatusCode::OK, Json(planned_json(&existing, &ath))));
    }

    let rationale = req.rationale.trim();
    if rationale.is_empty() || rationale.chars().count() > 500 {
        return Err(bad("`rationale` is required and limited to 500 characters"));
    }
    let rules = crate::rules::Rules::load(&state.pool).await?;
    let past = crate::rules::load_past(&state.pool, &rules, 35).await?;
    let today = past.last().map(|d| d.date).unwrap_or_else(|| chrono::Utc::now().date_naive());
    let horizon = today + chrono::Duration::days(120);
    let date = crate::planned::parse_date(&req.date).ok_or_else(|| bad("`date` must be a YYYY-MM-DD date"))?;
    if date < today || date > horizon {
        return Err(ApiError(StatusCode::BAD_REQUEST, format!("`date` must be today ({today}) or later, within 120 days")));
    }

    let candidate = match (req.workout_id, req.free_ride_budget) {
        (Some(workout_id), None) => {
            let (_, workout, prepared, sport) = load_library(&state.pool, &ath, workout_id).await?;
            Candidate::Library { workout, prepared, sport, workout_id }
        }
        (None, Some(b)) => {
            if !b.tss_range.is_none_or(|[lo, hi]| lo.is_finite() && hi.is_finite()) || b.duration_max_min > 720 {
                return Err(bad("`free_ride_budget` has an invalid `tss_range` or `duration_max_min`"));
            }
            let available = crate::availability::range(&state.pool, date, date).await?[0].available_min;
            let budget = crate::planned::free_ride_budget(
                b.tss_range.map(|[lo, hi]| (lo, hi)),
                b.duration_max_min,
                &b.intent,
                available,
                b.expected_if,
            )
            .map_err(|m| ApiError(StatusCode::UNPROCESSABLE_ENTITY, m))?;
            Candidate::FreeRide(budget)
        }
        _ => return Err(bad("give either `workout_id` or `free_ride_budget`")),
    };
    if !req.alternatives.is_empty() {
        let Candidate::Library { workout_id: main_id, .. } = &candidate else {
            return Err(bad("`alternatives` only go with a `workout_id` proposal"));
        };
        let ids: std::collections::HashSet<i64> = req.alternatives.iter().map(|a| a.workout_id).collect();
        if req.alternatives.len() > MAX_ALTERNATIVES || ids.len() != req.alternatives.len() || ids.contains(main_id) {
            return Err(bad(&format!("`alternatives`: at most {MAX_ALTERNATIVES}, distinct, and different from the main `workout_id`")));
        }
        if req.alternatives.iter().any(|a| a.note.as_deref().is_some_and(|n| n.chars().count() > 200)) {
            return Err(bad("an alternative `note` is limited to 200 characters"));
        }
    }

    // Refuse only what this session adds: a violation already present in the
    // calendar isn't the proposal's fault. A free ride is checked at the top
    // of its budget (worst case); the calendar counts it at the middle.
    let (minutes, tss) = match &candidate {
        Candidate::Library { workout, .. } => serde_json::from_value::<crate::workout::Workout>(workout.clone())
            .map(|w| w.load_estimate(&ath))
            .unwrap_or((0.0, 0.0)),
        Candidate::FreeRide(b) => (b.duration_max_min as f64, b.tss_max),
    };
    let base = calendar_sessions(&state.pool, &ath, projection_start(&past), horizon).await?;
    refuse_if_it_breaks_rules(&past, &base, &rules, date, minutes, tss, "proposal")?;

    // Every alternative must be one the engine would accept too.
    let mut alternatives: Vec<Value> = Vec::new();
    for alt in &req.alternatives {
        let (steps, workout, ..) = load_library(&state.pool, &ath, alt.workout_id).await?;
        let (m, t) = serde_json::from_value::<crate::workout::Workout>(workout.clone())
            .map(|w| w.load_estimate(&ath))
            .unwrap_or((0.0, 0.0));
        refuse_if_it_breaks_rules(&past, &base, &rules, date, m, t, &format!("alternative {}", alt.workout_id))?;
        let name = workout.get("name").and_then(Value::as_str).unwrap_or("");
        let note = alt.note.as_deref().map(str::trim).filter(|n| !n.is_empty());
        alternatives.push(crate::planned::alternative_snapshot(alt.workout_id, name, &steps, &ath, note));
    }

    let id = match &candidate {
        Candidate::Library { prepared, sport, workout_id, .. } => {
            crate::planned::insert(&state.pool, date, prepared, None, Some(*workout_id), "api", *sport).await?
        }
        Candidate::FreeRide(budget) => crate::planned::insert_free_ride(&state.pool, date, budget, "api").await?,
    };
    crate::planned::mark_proposal(&state.pool, id, rationale, key).await?;
    crate::planned::set_alternatives(&state.pool, id, &alternatives).await?;
    let created = crate::planned::get(&state.pool, id)
        .await?
        .ok_or_else(|| ApiError(StatusCode::INTERNAL_SERVER_ERROR, "planned session vanished".into()))?;
    Ok((StatusCode::CREATED, Json(planned_json(&created, &ath))))
}

#[derive(Deserialize)]
pub(super) struct SkipRequest {
    reason: Option<String>,
}

/// `POST /api/v1/planned/{id}/skip` — marks an upcoming session as skipped
/// (kept in the history, no longer counted in the load projection).
pub(super) async fn planned_skip(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Json(req): Json<SkipRequest>,
) -> ApiResult {
    let ath = current_athlete(&state.pool).await?;
    let existing = crate::planned::get(&state.pool, id)
        .await?
        .ok_or_else(|| ApiError(StatusCode::NOT_FOUND, format!("planned session {id} not found")))?;
    if !crate::planned::set_status(&state.pool, id, "skipped").await? {
        return Err(ApiError(
            StatusCode::CONFLICT,
            format!("a session with status `{}` can't be skipped", existing.status),
        ));
    }
    if let Some(reason) = req.reason.as_deref().map(str::trim).filter(|r| !r.is_empty()) {
        let reason: String = reason.chars().take(300).collect();
        sqlx::query("UPDATE planned_workouts SET notes = TRIM(COALESCE(notes || char(10), '') || ?) WHERE id = ?")
            .bind(format!("Passée : {reason}"))
            .bind(id)
            .execute(&state.pool)
            .await?;
    }
    let updated = crate::planned::get(&state.pool, id)
        .await?
        .ok_or_else(|| ApiError(StatusCode::NOT_FOUND, format!("planned session {id} not found")))?;
    Ok(Json(planned_json(&updated, &ath)))
}

/// `POST /api/v1/projection` — simulation only, nothing is written. Projects
/// CTL/ATL/TSB day by day for the given extra sessions (plus the calendar) and
/// lists the hard-rule violations. Callable with a read-only key.
pub(super) async fn projection(State(state): State<AppState>, Json(req): Json<ProjectionRequest>) -> ApiResult {
    let bad = |msg: String| ApiError(StatusCode::BAD_REQUEST, msg);
    let ath = current_athlete(&state.pool).await?;
    let rules = crate::rules::Rules::load(&state.pool).await?;
    let past = crate::rules::load_past(&state.pool, &rules, 35).await?;
    let today = past.last().map(|d| d.date).unwrap_or_else(|| chrono::Utc::now().date_naive());
    let horizon = today + chrono::Duration::days(120);

    let mut future = Vec::new();
    if req.include_planned.unwrap_or(true) {
        future = calendar_sessions(&state.pool, &ath, projection_start(&past), horizon).await?;
    }
    for (i, s) in req.sessions.iter().enumerate() {
        let date = crate::planned::parse_date(&s.date).ok_or_else(|| bad(format!("sessions[{i}].date must be YYYY-MM-DD")))?;
        if date < today || date > horizon {
            return Err(bad(format!("sessions[{i}].date must be today ({today}) or later, within 120 days")));
        }
        let (tss, f) = match (s.workout_id, s.tss) {
            (Some(_), Some(_)) | (None, None) => return Err(bad(format!("sessions[{i}]: give either `workout_id` or `tss`"))),
            (None, Some(tss)) if tss.is_finite() && (0.0..=1000.0).contains(&tss) => (tss, s.intensity_factor),
            (None, Some(_)) => return Err(bad(format!("sessions[{i}].tss must be between 0 and 1000"))),
            (Some(id), None) => {
                let steps: Option<String> = sqlx::query_scalar("SELECT steps_json FROM workouts WHERE id = ?")
                    .bind(id)
                    .fetch_optional(&state.pool)
                    .await?;
                let steps = steps.ok_or_else(|| ApiError(StatusCode::NOT_FOUND, format!("workout template {id} not found")))?;
                let (minutes, tss) = serde_json::from_str::<crate::workout::Workout>(&steps)
                    .map(|w| w.load_estimate(&ath))
                    .unwrap_or((0.0, 0.0));
                (tss, crate::rules::intensity_factor(minutes, tss))
            }
        };
        future.push(crate::rules::FutureSession { date, tss, intensity_factor: f });
    }

    let sim = crate::rules::simulate(&past, &future, &rules);
    let days: Vec<Value> = sim
        .days
        .iter()
        .map(|d| json!({ "date": d.date, "tss": round(d.tss, 0), "ctl": round(d.ctl, 1), "atl": round(d.atl, 1), "tsb": round(d.tsb, 1), "intense": d.intense }))
        .collect();
    Ok(Json(json!({
        "from": today,
        "days": days,
        "violations": sim.violations,
        "ok": sim.violations.is_empty(),
        "rules": rules_json(&rules),
    })))
}

#[derive(Deserialize)]
pub(super) struct CheckinRequest {
    legs: i64,
    motivation: i64,
    rhr_bpm: Option<i64>,
    note: Option<String>,
    weather: Option<String>,
}

fn checkin_json(row: (String, i64, i64, Option<i64>, Option<String>, Option<String>)) -> Value {
    let (date, legs, motivation, rhr_bpm, note, weather) = row;
    json!({ "date": date, "legs": legs, "motivation": motivation, "rhr_bpm": rhr_bpm, "note": note, "weather": weather })
}

/// `PUT /api/v1/checkins/{date}` — the day's subjective feeling (legs and
/// motivation 1 = bad … 5 = great). Replaces the day's previous check-in.
pub(super) async fn checkin_put(
    State(state): State<AppState>,
    Path(date): Path<String>,
    Json(req): Json<CheckinRequest>,
) -> ApiResult {
    let bad = |msg: &str| ApiError(StatusCode::BAD_REQUEST, msg.to_string());
    let day = crate::planned::parse_date(&date).ok_or_else(|| bad("date must be YYYY-MM-DD"))?;
    if !(1..=5).contains(&req.legs) || !(1..=5).contains(&req.motivation) {
        return Err(bad("`legs` and `motivation` must be between 1 and 5"));
    }
    if req.rhr_bpm.is_some_and(|v| !(25..=150).contains(&v)) {
        return Err(bad("`rhr_bpm` must be between 25 and 150"));
    }
    let clean = |s: Option<String>, max: usize| s.map(|s| s.trim().chars().take(max).collect::<String>()).filter(|s| !s.is_empty());
    let (note, weather) = (clean(req.note, 1000), clean(req.weather, 100));
    sqlx::query(
        "INSERT INTO checkins (date, legs, motivation, rhr_bpm, note, weather) VALUES (?, ?, ?, ?, ?, ?) \
         ON CONFLICT(date) DO UPDATE SET legs = excluded.legs, motivation = excluded.motivation, \
         rhr_bpm = excluded.rhr_bpm, note = excluded.note, weather = excluded.weather, updated_at = datetime('now')",
    )
    .bind(day.to_string())
    .bind(req.legs)
    .bind(req.motivation)
    .bind(req.rhr_bpm)
    .bind(&note)
    .bind(&weather)
    .execute(&state.pool)
    .await?;
    Ok(Json(checkin_json((day.to_string(), req.legs, req.motivation, req.rhr_bpm, note, weather))))
}

#[derive(Deserialize)]
pub(super) struct CheckinsQuery {
    from: Option<String>,
    to: Option<String>,
}

/// `GET /api/v1/checkins?from=&to=` — check-ins with `from <= date <= to`
/// (default: the last 30 days), newest first.
pub(super) async fn checkins(State(state): State<AppState>, Query(q): Query<CheckinsQuery>) -> ApiResult {
    let from = q.from.unwrap_or_else(|| days_ago(30));
    let to = q.to.unwrap_or_else(|| days_ago(0));
    let rows: Vec<(String, i64, i64, Option<i64>, Option<String>, Option<String>)> = sqlx::query_as(
        "SELECT date, legs, motivation, rhr_bpm, note, weather FROM checkins WHERE date >= ? AND date <= ? ORDER BY date DESC",
    )
    .bind(from)
    .bind(to)
    .fetch_all(&state.pool)
    .await?;
    let items: Vec<Value> = rows.into_iter().map(checkin_json).collect();
    Ok(Json(json!({ "count": items.len(), "checkins": items })))
}

#[derive(Deserialize)]
pub(super) struct RideEnvelopeQuery {
    /// `YYYY-MM-DD`, today or later.
    date: String,
    /// Time to ride; default: what the availability of that day says.
    available_min: Option<i64>,
}

/// `GET /api/v1/ride-envelope?date=&available_min=` — what a ride on that day
/// could look like: for each scenario that fits the time available, the typical
/// TSS (from the ride history), the TSB after it and whether the hard rules
/// allow it. Nothing is written. Sessions already planned on that day are
/// ignored (the envelope is what could replace them).
pub(super) async fn ride_envelope(State(state): State<AppState>, Query(q): Query<RideEnvelopeQuery>) -> ApiResult {
    let bad = |msg: String| ApiError(StatusCode::BAD_REQUEST, msg);
    let ath = current_athlete(&state.pool).await?;
    let rules = crate::rules::Rules::load(&state.pool).await?;
    let past = crate::rules::load_past(&state.pool, &rules, 35).await?;
    let today = past.last().map(|d| d.date).unwrap_or_else(|| chrono::Utc::now().date_naive());
    let horizon = today + chrono::Duration::days(120);
    let date = crate::planned::parse_date(&q.date).ok_or_else(|| bad("`date` must be a YYYY-MM-DD date".into()))?;
    if date < today || date > horizon {
        return Err(bad(format!("`date` must be today ({today}) or later, within 120 days")));
    }
    if q.available_min.is_some_and(|m| !(0..=crate::availability::MAX_MINUTES).contains(&m)) {
        return Err(bad(format!("`available_min` must be between 0 and {}", crate::availability::MAX_MINUTES)));
    }
    let day = crate::availability::range(&state.pool, date, date).await?.remove(0);
    let available_min = q.available_min.unwrap_or(day.available_min);

    // Rides of the last year with a TSS, to learn what each kind of ride costs.
    let rows: Vec<(i64, f64, f64)> = sqlx::query_as(
        "SELECT duration_s, tss, elevation_gain_m FROM activities \
         WHERE sport_type IN ('Ride', 'VirtualRide') AND tss IS NOT NULL AND date(started_at) >= ?",
    )
    .bind(days_ago(365))
    .fetch_all(&state.pool)
    .await?;
    let history: Vec<crate::envelope::HistoricRide> = rows
        .into_iter()
        .map(|(s, tss, elev)| crate::envelope::HistoricRide { duration_min: s as f64 / 60.0, tss, elevation_gain_m: elev })
        .collect();

    let base: Vec<_> = calendar_sessions(&state.pool, &ath, projection_start(&past), horizon)
        .await?
        .into_iter()
        .filter(|s| s.date != date)
        .collect();
    let before = crate::rules::simulate(&past, &base, &rules).violations;
    // Violations the scenario adds on top of the calendar, if ridden at `tss`.
    let added_by = |minutes: f64, tss: f64| {
        let mut with = base.clone();
        with.push(crate::rules::FutureSession { date, tss, intensity_factor: crate::rules::intensity_factor(minutes, tss) });
        let sim = crate::rules::simulate(&past, &with, &rules);
        let added: Vec<_> = sim
            .violations
            .iter()
            .filter(|v| !before.iter().any(|b| b.rule == v.rule && b.date == v.date))
            .cloned()
            .collect();
        (sim, added)
    };

    let mut scenarios = Vec::new();
    for s in crate::envelope::scenarios(available_min) {
        let range = crate::envelope::tss_range(&history, &s);
        let minutes = s.duration_min as f64;
        let mid = (range.lo + range.hi) / 2.0;
        let (sim_mid, added_mid) = added_by(minutes, mid);
        let (_, added_max) = added_by(minutes, range.hi);
        // Violations at the typical load make it inadvisable; only at the top of the range, borderline.
        let (feasibility, rule) = match (added_mid.first(), added_max.first()) {
            (Some(v), _) => ("deconseille", Some(v)),
            (None, Some(v)) => ("limite", Some(v)),
            (None, None) => ("ok", None),
        };
        let after = sim_mid.days.iter().find(|d| d.date == date);
        scenarios.push(json!({
            "scenario": s.name,
            "intent": s.intent,
            "duration_min": s.duration_min,
            "tss_range": [range.lo, range.hi],
            "tss_source": range.source,
            "sample_size": range.sample_size,
            "source": if range.source == "history" { "computed" } else { "estimated" },
            "ctl_after": after.map(|d| round(d.ctl, 1)),
            "tsb_after": after.map(|d| round(d.tsb, 1)),
            "feasibility": feasibility,
            "rule": rule,
        }));
    }
    Ok(Json(json!({
        "date": date,
        "available_min": available_min,
        "available_source": if q.available_min.is_some() { "parameter" } else { day.source },
        "time_of_day": day.time_of_day,
        "context": day.context,
        "slot_type": crate::envelope::slot_type(available_min),
        "scenarios": scenarios,
        "rules": rules_json(&rules),
    })))
}

const MAX_AVAILABILITY_DAYS: i64 = 92;

#[derive(Deserialize)]
pub(super) struct AvailabilityQuery {
    from: Option<String>,
    to: Option<String>,
}

/// `GET /api/v1/availability?from=&to=` — time available to ride, day by day
/// (default: the next 14 days, 92 days at most): the exception of the date
/// if there is one, else the weekday default.
pub(super) async fn availability_get(State(state): State<AppState>, Query(q): Query<AvailabilityQuery>) -> ApiResult {
    let bad = |msg: &str| ApiError(StatusCode::BAD_REQUEST, msg.to_string());
    let today = chrono::Utc::now().date_naive();
    let parse = |v: &Option<String>, name: &str, default: chrono::NaiveDate| match v {
        None => Ok(default),
        Some(s) => crate::planned::parse_date(s).ok_or_else(|| bad(&format!("`{name}` must be a YYYY-MM-DD date"))),
    };
    let from = parse(&q.from, "from", today)?;
    let to = parse(&q.to, "to", from + chrono::Duration::days(13))?;
    if to < from {
        return Err(bad("`to` must not be before `from`"));
    }
    if (to - from).num_days() + 1 > MAX_AVAILABILITY_DAYS {
        return Err(bad(&format!("the range is limited to {MAX_AVAILABILITY_DAYS} days")));
    }
    let days = crate::availability::range(&state.pool, from, to).await?;
    Ok(Json(json!({ "from": from, "to": to, "count": days.len(), "days": days })))
}

#[derive(Deserialize)]
pub(super) struct AvailabilityRequest {
    /// One day… (`YYYY-MM-DD`)
    date: Option<String>,
    /// …or a range, both ends included.
    from: Option<String>,
    to: Option<String>,
    available_min: i64,
    /// `soir` | `journee`
    time_of_day: String,
    /// `travail` | `vacances` | `autre`
    context: String,
    note: Option<String>,
}

/// `PUT /api/v1/availability` — creates or replaces the *exception* of one
/// day or of a range of days. The weekday defaults are never touched here.
pub(super) async fn availability_put(State(state): State<AppState>, Json(req): Json<AvailabilityRequest>) -> ApiResult {
    let bad = |msg: &str| ApiError(StatusCode::BAD_REQUEST, msg.to_string());
    let parse = |s: &str, name: &str| crate::planned::parse_date(s).ok_or_else(|| bad(&format!("`{name}` must be a YYYY-MM-DD date")));
    let (from, to) = match (&req.date, &req.from, &req.to) {
        (Some(d), None, None) => {
            let d = parse(d, "date")?;
            (d, d)
        }
        (None, Some(f), Some(t)) => (parse(f, "from")?, parse(t, "to")?),
        _ => return Err(bad("give either `date`, or both `from` and `to`")),
    };
    let today = chrono::Utc::now().date_naive();
    if from < today {
        return Err(bad(&format!("dates must be today ({today}) or later")));
    }
    if to < from {
        return Err(bad("`to` must not be before `from`"));
    }
    if (to - from).num_days() + 1 > MAX_AVAILABILITY_DAYS {
        return Err(bad(&format!("the range is limited to {MAX_AVAILABILITY_DAYS} days")));
    }
    let slot = crate::availability::Slot { available_min: req.available_min, time_of_day: req.time_of_day, context: req.context };
    crate::availability::validate(&slot).map_err(|m| ApiError(StatusCode::BAD_REQUEST, m))?;
    let note: Option<String> = req.note.map(|n| n.trim().chars().take(300).collect::<String>()).filter(|n| !n.is_empty());

    let mut date = from;
    while date <= to {
        crate::availability::set_exception(&state.pool, date, &slot, note.as_deref()).await?;
        date += chrono::Duration::days(1);
    }
    let days = crate::availability::range(&state.pool, from, to).await?;
    Ok(Json(json!({ "updated": days.len(), "days": days })))
}

async fn current_athlete(pool: &SqlitePool) -> Result<crate::workout::Athlete, ApiError> {
    Ok(crate::workout::Athlete::load(pool).await)
}

#[derive(Deserialize)]
pub(super) struct WorkoutsQuery {
    /// Catalogue category, e.g. `Depuis Zwift`.
    category: Option<String>,
    /// Plan/group key within a category, e.g. `build-me-up` (see `/workouts/categories`).
    group: Option<String>,
    /// Origin, exact match: `manual`, `digest`…
    source: Option<String>,
    /// `cycling`, `running` or `other`.
    sport: Option<String>,
    /// Case-insensitive substring of the name.
    q: Option<String>,
    min_duration_min: Option<f64>,
    max_duration_min: Option<f64>,
    min_tss: Option<f64>,
    max_tss: Option<f64>,
    /// Intensity factor bounds (computed at the current FTP).
    min_if: Option<f64>,
    max_if: Option<f64>,
    /// Minimum time per power zone: `zone:minutes[,zone:minutes]`, e.g.
    /// `4:20` (≥ 20 min in Z4) or `4:10,5:5`.
    min_zone_time: Option<String>,
    /// Session-type tag: `recuperation`, `endurance`, `tempo`, `seuil`, `vo2`,
    /// `anaerobie` (comma-separated = any of them).
    tag: Option<String>,
    /// Rank by closeness to this TSS (closest first) instead of by date/plan order.
    target_tss: Option<f64>,
    /// Leave out templates already done (status `done`) in the last N days.
    exclude_done_within_days: Option<i64>,
    /// Include the full step structure (`workout`). Default false: catalogue
    /// listings are large; fetch one template with `/workouts/{id}`.
    #[serde(default)]
    steps: bool,
    limit: Option<i64>,
    #[serde(default)]
    offset: i64,
}

type WorkoutRow = (
    i64,
    String,
    String,
    String,
    String,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
    i64,
    Option<String>,
    Option<String>,
);

const WORKOUT_COLUMNS: &str =
    "id, name, steps_json, source, sport, category, group_key, group_name, subgroup, position, description, source_url";

/// Figures computed on the fly from a template's steps at the *current* FTP.
struct WorkoutMetrics {
    minutes: f64,
    tss: f64,
    intensity_factor: f64,
    /// Minutes in power zones Z1..Z7.
    zones: [f64; 7],
    tags: Vec<&'static str>,
}

/// Session type tags derived from the zone distribution (the catalogue itself
/// carries none): `recuperation`, `endurance`, `tempo`, `seuil`, `vo2`,
/// `anaerobie`. A session can carry several (e.g. `tempo` + `vo2`).
fn workout_tags(zones: &[f64; 7], minutes: f64, intensity_factor: f64) -> Vec<&'static str> {
    let mut tags = Vec::new();
    if minutes <= 0.0 {
        return tags;
    }
    let easy = zones[0] + zones[1];
    if intensity_factor < 0.55 {
        tags.push("recuperation");
    } else if easy / minutes >= 0.7 && intensity_factor < 0.75 {
        tags.push("endurance");
    }
    if zones[2] >= 15.0 {
        tags.push("tempo");
    }
    if zones[3] >= 10.0 {
        tags.push("seuil");
    }
    if zones[4] >= 5.0 {
        tags.push("vo2");
    }
    if zones[5] + zones[6] >= 2.0 {
        tags.push("anaerobie");
    }
    tags
}

fn workout_json(row: WorkoutRow, ath: &crate::workout::Athlete, with_steps: bool) -> (Value, WorkoutMetrics) {
    let (id, name, steps_json, source, sport, category, group_key, group_name, subgroup, position, description, source_url) =
        row;
    let workout: Value = serde_json::from_str(&steps_json).unwrap_or(Value::Null);
    let parsed = serde_json::from_value::<crate::workout::Workout>(workout.clone()).ok();
    let (minutes, tss) = parsed.as_ref().map(|w| w.load_estimate(ath)).unwrap_or((0.0, 0.0));
    let zones = parsed.as_ref().map(|w| w.zone_minutes(ath)).unwrap_or([0.0; 7]);
    let intensity_factor = crate::workout::intensity_factor(minutes, tss);
    let tags = workout_tags(&zones, minutes, intensity_factor);
    let mut item = json!({
        "id": id, "name": name, "source": source, "sport": sport,
        "category": category, "group_key": group_key, "group_name": group_name,
        "subgroup": subgroup, "position": position,
        "plan": group_key.as_ref().map(|k| json!({
            "category": category, "group_key": k, "group_name": group_name, "subgroup": subgroup, "position": position,
        })),
        "description": description, "source_url": source_url,
        "duration_min": round(minutes, 1), "estimated_tss": round(tss, 0),
        "intensity_factor": round(intensity_factor, 2),
        "power_zone_minutes": zones.iter().map(|m| round(*m, 1)).collect::<Vec<_>>(),
        "tags": tags,
        "structure_summary": parsed.as_ref().map(|w| w.structure_summary()),
        "estimated_distance_km": workout.get("estimated_distance_km"),
        "computed_at_ftp_w": ath.ftp_watts,
    });
    if with_steps {
        item["workout"] = workout;
    }
    (item, WorkoutMetrics { minutes, tss, intensity_factor, zones, tags })
}

/// Parses `min_zone_time` = comma-separated `zone:minutes` pairs, e.g. `4:20,5:5`
/// (at least 20 min in Z4 and 5 min in Z5).
fn parse_zone_filters(raw: &str) -> Result<Vec<(usize, f64)>, ApiError> {
    let bad = || ApiError(StatusCode::BAD_REQUEST, format!("min_zone_time: expected `zone:minutes[,zone:minutes]` (zone 1-7), got `{raw}`"));
    raw.split(',')
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .map(|p| {
            let (z, m) = p.split_once(':').ok_or_else(bad)?;
            let zone: usize = z.trim().parse().map_err(|_| bad())?;
            let minutes: f64 = m.trim().replace(',', ".").parse().map_err(|_| bad())?;
            if !(1..=7).contains(&zone) || minutes < 0.0 {
                return Err(bad());
            }
            Ok((zone - 1, minutes))
        })
        .collect()
}

/// `GET /api/v1/workouts` — the reusable session templates (use an `id` as
/// `workout_id` when planning), including the imported catalogue (Zwift
/// plans). Filterable; see `WorkoutsQuery`.
pub(super) async fn workouts(State(state): State<AppState>, Query(q): Query<WorkoutsQuery>) -> ApiResult {
    let ath = current_athlete(&state.pool).await?;
    let mut qb: sqlx::QueryBuilder<sqlx::Sqlite> =
        sqlx::QueryBuilder::new(format!("SELECT {WORKOUT_COLUMNS} FROM workouts WHERE 1=1"));
    if let Some(v) = q.category.as_deref().filter(|v| !v.is_empty()) {
        qb.push(" AND category = ").push_bind(v.to_string());
    }
    if let Some(v) = q.group.as_deref().filter(|v| !v.is_empty()) {
        qb.push(" AND group_key = ").push_bind(v.to_string());
    }
    if let Some(v) = q.source.as_deref().filter(|v| !v.is_empty()) {
        qb.push(" AND source = ").push_bind(v.to_string());
    }
    if let Some(v) = q.sport.as_deref().filter(|v| !v.is_empty()) {
        qb.push(" AND sport = ").push_bind(v.to_string());
    }
    if let Some(v) = q.q.as_deref().map(str::trim).filter(|v| !v.is_empty()) {
        let like = format!("%{}%", v.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_"));
        qb.push(" AND name LIKE ").push_bind(like).push(" ESCAPE '\\'");
    }
    if q.category.is_some() || q.group.is_some() {
        qb.push(" ORDER BY category, group_key, position, id");
    } else {
        qb.push(" ORDER BY created_at DESC, id DESC");
    }
    let rows: Vec<WorkoutRow> = qb.build_query_as().fetch_all(&state.pool).await?;

    let zone_filters = q.min_zone_time.as_deref().map(parse_zone_filters).transpose()?.unwrap_or_default();
    let tags: Vec<String> = q
        .tag
        .as_deref()
        .unwrap_or("")
        .split(',')
        .map(|t| t.trim().to_lowercase())
        .filter(|t| !t.is_empty())
        .collect();

    // Last completed date per template (planned sessions auto-completed by
    // `planned::reconcile` or marked done).
    crate::planned::reconcile(&state.pool, chrono::Utc::now().date_naive()).await?;
    let done: std::collections::HashMap<i64, String> = sqlx::query_as::<_, (i64, String)>(
        "SELECT workout_id, MAX(planned_date) FROM planned_workouts \
         WHERE status = 'done' AND workout_id IS NOT NULL GROUP BY workout_id",
    )
    .fetch_all(&state.pool)
    .await?
    .into_iter()
    .collect();
    let recent_cutoff = q
        .exclude_done_within_days
        .map(|d| (chrono::Utc::now().date_naive() - chrono::Duration::days(d.clamp(0, 3650))).to_string());

    let in_range = |v: f64, lo: Option<f64>, hi: Option<f64>| lo.is_none_or(|lo| v >= lo) && hi.is_none_or(|hi| v <= hi);
    let mut all: Vec<(Value, f64)> = rows
        .into_iter()
        .filter_map(|row| {
            let id = row.0;
            let (mut item, m) = workout_json(row, &ath, q.steps);
            let last_done = done.get(&id);
            let keep = in_range(m.minutes, q.min_duration_min, q.max_duration_min)
                && in_range(m.tss, q.min_tss, q.max_tss)
                && in_range(m.intensity_factor, q.min_if, q.max_if)
                && zone_filters.iter().all(|(z, min)| m.zones[*z] >= *min)
                && (tags.is_empty() || tags.iter().any(|t| m.tags.contains(&t.as_str())))
                && match (&recent_cutoff, last_done) {
                    (Some(cutoff), Some(d)) => d < cutoff,
                    _ => true,
                };
            item["last_done"] = json!(last_done);
            let distance = q.target_tss.map(|t| (m.tss - t).abs()).unwrap_or(0.0);
            keep.then_some((item, distance))
        })
        .collect();
    if q.target_tss.is_some() {
        all.sort_by(|a, b| a.1.total_cmp(&b.1));
    }
    let all: Vec<Value> = all.into_iter().map(|(v, _)| v).collect();
    let total = all.len();
    let limit = q.limit.unwrap_or(200).clamp(1, 500) as usize;
    let items: Vec<Value> = all.into_iter().skip(q.offset.max(0) as usize).take(limit).collect();
    Ok(Json(json!({ "total": total, "count": items.len(), "workouts": items })))
}

/// `GET /api/v1/workouts/{id}` — one template with its full step structure.
pub(super) async fn workout(State(state): State<AppState>, Path(id): Path<i64>) -> ApiResult {
    let ath = current_athlete(&state.pool).await?;
    let row: Option<WorkoutRow> = sqlx::query_as(&format!("SELECT {WORKOUT_COLUMNS} FROM workouts WHERE id = ?"))
        .bind(id)
        .fetch_optional(&state.pool)
        .await?;
    let row = row.ok_or_else(|| ApiError(StatusCode::NOT_FOUND, format!("workout template {id} not found")))?;
    let (mut item, _) = workout_json(row, &ath, true);
    item["last_done"] = json!(sqlx::query_scalar::<_, Option<String>>(
        "SELECT MAX(planned_date) FROM planned_workouts WHERE status = 'done' AND workout_id = ?",
    )
    .bind(id)
    .fetch_one(&state.pool)
    .await?);
    Ok(Json(item))
}

/// `GET /api/v1/workouts/categories` — the catalogue's table of contents:
/// categories, their plans (`group_key`) and how many sessions each holds.
pub(super) async fn workout_categories(State(state): State<AppState>) -> ApiResult {
    let rows: Vec<(String, Option<String>, Option<String>, i64)> = sqlx::query_as(
        "SELECT category, group_key, MIN(group_name), COUNT(*) FROM workouts \
         WHERE category IS NOT NULL GROUP BY category, group_key ORDER BY category, MIN(position)",
    )
    .fetch_all(&state.pool)
    .await?;
    let mut categories: Vec<Value> = Vec::new();
    for (category, group_key, group_name, count) in rows {
        let group = json!({ "group_key": group_key, "group_name": group_name, "count": count });
        match categories.last_mut().filter(|c| c["category"] == json!(category)) {
            Some(c) => c["groups"].as_array_mut().unwrap().push(group),
            None => categories.push(json!({ "category": category, "groups": [group] })),
        }
    }
    Ok(Json(json!({ "categories": categories })))
}

const COACH_NOTE_MAX_CHARS: usize = 1000;

/// `GET /api/v1/coach-note` — the message currently shown on the dashboard.
pub(super) async fn coach_note_get(State(state): State<AppState>) -> ApiResult {
    let row: Option<(String, Option<String>, String)> =
        sqlx::query_as("SELECT message, author, updated_at FROM coach_note WHERE id = 1")
            .fetch_optional(&state.pool)
            .await?;
    Ok(Json(match row {
        Some((message, author, updated_at)) => json!({ "message": message, "author": author, "updated_at": updated_at }),
        None => json!({ "message": null }),
    }))
}

#[derive(Deserialize)]
pub(super) struct CoachNoteRequest {
    message: String,
    author: Option<String>,
}

/// `PUT /api/v1/coach-note` — replace the dashboard message (an empty message
/// clears it). Plain text, at most 1000 characters.
pub(super) async fn coach_note_put(State(state): State<AppState>, Json(req): Json<CoachNoteRequest>) -> ApiResult {
    let message = req.message.trim();
    if message.chars().count() > COACH_NOTE_MAX_CHARS {
        return Err(ApiError(
            StatusCode::BAD_REQUEST,
            format!("`message` is limited to {COACH_NOTE_MAX_CHARS} characters"),
        ));
    }
    if message.is_empty() {
        sqlx::query("DELETE FROM coach_note WHERE id = 1").execute(&state.pool).await?;
        return Ok(Json(json!({ "message": null })));
    }
    let author = req
        .author
        .as_deref()
        .map(str::trim)
        .filter(|a| !a.is_empty())
        .map(|a| a.chars().take(60).collect::<String>());
    sqlx::query(
        "INSERT INTO coach_note (id, message, author, updated_at) VALUES (1, ?, ?, datetime('now')) \
         ON CONFLICT(id) DO UPDATE SET message = excluded.message, author = excluded.author, updated_at = excluded.updated_at",
    )
    .bind(message)
    .bind(&author)
    .execute(&state.pool)
    .await?;
    coach_note_get(State(state)).await
}

#[derive(Deserialize)]
pub(super) struct PlannedQuery {
    from: Option<String>,
    to: Option<String>,
    /// `cycling`, `running` or `other`; absent means all.
    sport: Option<String>,
}

/// `GET /api/v1/planned?from=2026-10-01&to=2026-11-01` — planned sessions with
/// `from <= date < to`. Defaults: a week back to six weeks ahead.
pub(super) async fn planned_list(State(state): State<AppState>, Query(q): Query<PlannedQuery>) -> ApiResult {
    let today = chrono::Utc::now().date_naive();
    let parse = |v: &Option<String>, name: &str, default: chrono::NaiveDate| match v {
        None => Ok(default),
        Some(s) => crate::planned::parse_date(s)
            .ok_or_else(|| ApiError(StatusCode::BAD_REQUEST, format!("`{name}` must be a YYYY-MM-DD date"))),
    };
    let from = parse(&q.from, "from", today - chrono::Duration::days(7))?;
    let to = parse(&q.to, "to", today + chrono::Duration::days(42))?;
    let ath = current_athlete(&state.pool).await?;
    let sport = match q.sport.as_deref() {
        None => None,
        Some(s) => Some(crate::planned::Sport::parse(s).ok_or_else(|| {
            ApiError(StatusCode::BAD_REQUEST, "`sport` must be cycling, running or other".to_string())
        })?),
    };
    crate::planned::reconcile(&state.pool, today).await?;
    let rows = crate::planned::list_between(&state.pool, from, to).await?;
    let items: Vec<Value> = rows
        .iter()
        .filter(|p| sport.is_none_or(|s| crate::planned::Sport::from_db(&p.sport) == s))
        .map(|p| planned_json(p, &ath))
        .collect();
    Ok(Json(json!({ "from": from.to_string(), "to": to.to_string(), "count": items.len(), "planned": items })))
}

#[derive(Deserialize)]
pub(super) struct PlanRequest {
    /// `YYYY-MM-DD`.
    date: String,
    /// Copy the steps of this template (see `GET /api/v1/workouts`)…
    workout_id: Option<i64>,
    /// …or give the session inline: `{"name": "...", "steps": [...]}`, same
    /// shape as a template's `workout`. Exactly one of the two.
    workout: Option<Value>,
    /// Overrides the session name (handy with `workout_id`).
    name: Option<String>,
    notes: Option<String>,
    /// `cycling` (default), `running` or `other`. With `workout_id`, defaults
    /// to the template's sport.
    sport: Option<String>,
}

/// `POST /api/v1/planned` — put a session on a day. The planned session owns
/// a copy of its steps (editing the template later doesn't change it); several
/// sessions on the same day are allowed. Returns 201 with the created session.
pub(super) async fn planned_create(
    State(state): State<AppState>,
    Json(req): Json<PlanRequest>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    let bad = |msg: String| ApiError(StatusCode::BAD_REQUEST, msg);
    let date = crate::planned::parse_date(&req.date).ok_or_else(|| bad("`date` must be a YYYY-MM-DD date".into()))?;
    let ath = current_athlete(&state.pool).await?;

    let requested_sport = match req.sport.as_deref() {
        None => None,
        Some(s) => Some(
            crate::planned::Sport::parse(s).ok_or_else(|| bad("`sport` must be cycling, running or other".into()))?,
        ),
    };

    let (mut workout, template_id, template_sport) = match (req.workout_id, req.workout) {
        (Some(_), Some(_)) => return Err(bad("give either `workout_id` or `workout`, not both".into())),
        (None, None) => return Err(bad("`workout_id` or `workout` is required".into())),
        (Some(id), None) => {
            let row: Option<(String, String)> = sqlx::query_as("SELECT steps_json, sport FROM workouts WHERE id = ?")
                .bind(id)
                .fetch_optional(&state.pool)
                .await?;
            let (steps, sport) =
                row.ok_or_else(|| ApiError(StatusCode::NOT_FOUND, format!("workout template {id} not found")))?;
            (
                serde_json::from_str::<Value>(&steps).unwrap_or(Value::Null),
                Some(id),
                Some(crate::planned::Sport::from_db(&sport)),
            )
        }
        (None, Some(inline)) => (inline, None, None),
    };
    let sport = requested_sport.or(template_sport).unwrap_or_default();
    let name_override = req.name.as_deref().map(str::trim).filter(|n| !n.is_empty());
    if let (Some(name), Some(obj)) = (name_override, workout.as_object_mut()) {
        obj.insert("name".to_string(), json!(name));
    }

    let prepared = crate::planned::prepare(&workout, &ath, sport).map_err(bad)?;
    let id = crate::planned::insert(&state.pool, date, &prepared, req.notes.as_deref(), template_id, "api", sport).await?;
    let created = crate::planned::get(&state.pool, id)
        .await?
        .ok_or_else(|| ApiError(StatusCode::INTERNAL_SERVER_ERROR, "planned session vanished".into()))?;
    Ok((StatusCode::CREATED, Json(planned_json(&created, &ath))))
}

/// `DELETE /api/v1/planned/{id}` — remove a session from the calendar.
pub(super) async fn planned_delete(State(state): State<AppState>, Path(id): Path<i64>) -> Result<StatusCode, ApiError> {
    if crate::planned::delete(&state.pool, id).await? {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(ApiError(StatusCode::NOT_FOUND, format!("planned session {id} not found")))
    }
}

// ---------------------------------------------------------------------------
// Detailed performance (per lap, whole ride) and raw-ish streams
// ---------------------------------------------------------------------------

async fn load_series(pool: &SqlitePool, activity_id: i64, is_run: bool) -> Result<Option<crate::performance::Series>, ApiError> {
    let streams: Option<String> = sqlx::query_scalar("SELECT streams_json FROM activity_streams WHERE activity_id = ?")
        .bind(activity_id)
        .fetch_optional(pool)
        .await?;
    Ok(streams
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
        .and_then(|v| crate::performance::Series::from_streams(&v, !is_run)))
}

fn opt_round(v: Option<f64>, digits: i32) -> Option<f64> {
    v.map(|v| round(v, digits))
}

#[allow(clippy::too_many_arguments)]
fn lap_json(
    index: i64,
    start_s: usize,
    duration_s: i64,
    stored_distance_m: f64,
    stored_power: Option<f64>,
    stored_hr: Option<f64>,
    stats: Option<&crate::performance::WindowStats>,
    is_run: bool,
    ftp: f64,
) -> Value {
    let distance_m = stats.and_then(|s| s.distance_m).unwrap_or(stored_distance_m);
    let avg_power = stats.and_then(|s| s.avg_power_w).or(stored_power);
    let np = stats.and_then(|s| s.normalized_power_w);
    let cadence = stats.and_then(|s| s.avg_cadence);
    json!({
        "index": index,
        "start_s": start_s,
        "duration_s": duration_s,
        "distance_km": round(distance_m / 1000.0, 2),
        "avg_pace_min_per_km": if is_run { super::format::format_pace(distance_m, duration_s) } else { None },
        "avg_speed_kmh": if !is_run && duration_s > 0 { Some(round(distance_m / duration_s as f64 * 3.6, 1)) } else { None },
        "avg_power_w": opt_round(avg_power, 0),
        "max_power_w": opt_round(stats.and_then(|s| s.max_power_w), 0),
        "normalized_power_w": opt_round(np, 0),
        "intensity_factor": opt_round(np.map(|np| np / ftp), 2),
        "avg_hr": opt_round(stats.and_then(|s| s.avg_hr).or(stored_hr), 0),
        "max_hr": opt_round(stats.and_then(|s| s.max_hr), 0),
        // Sources report running cadence per leg: x2 gives steps per minute.
        "avg_cadence_rpm": if is_run { None } else { opt_round(cadence, 0) },
        "avg_cadence_spm": if is_run { opt_round(cadence.map(|c| c * 2.0), 0) } else { None },
        "elevation_gain_m": opt_round(stats.and_then(|s| s.elevation_gain_m), 0),
        "avg_grade_pct": opt_round(stats.and_then(|s| s.avg_grade_pct), 1),
        "power_zone_seconds": stats.and_then(|s| s.power_zone_s),
        "hr_zone_seconds": stats.and_then(|s| s.hr_zone_s),
    })
}

/// Whole-activity metrics computed from the streams: peak/normalized power,
/// variability, best-power curve, HR zones, power:HR decoupling.
fn performance_json(series: &crate::performance::Series, is_run: bool, ath: &crate::workout::Athlete) -> Value {
    let w = crate::performance::window_stats(series, 0, series.len, ath);
    let np = w.normalized_power_w;
    let curve = series
        .power
        .as_ref()
        .map(|p| {
            crate::performance::power_curve(p)
                .into_iter()
                .map(|(label, watts)| (label.to_string(), json!(opt_round(watts, 0))))
                .collect::<serde_json::Map<_, _>>()
        })
        .map(Value::Object)
        .unwrap_or(Value::Null);
    let cadence = w.avg_cadence;
    json!({
        "duration_s": w.duration_s,
        "avg_power_w": opt_round(w.avg_power_w, 0),
        "max_power_w": opt_round(w.max_power_w, 0),
        "normalized_power_w": opt_round(np, 0),
        "variability_index": match (np, w.avg_power_w) {
            (Some(np), Some(avg)) if avg > 0.0 => Some(round(np / avg, 2)),
            _ => None,
        },
        "intensity_factor": opt_round(np.map(|np| np / ath.ftp_watts), 2),
        "power_curve_w": curve,
        "avg_hr": opt_round(w.avg_hr, 0),
        "max_hr": opt_round(w.max_hr, 0),
        "hr_zone_seconds": w.hr_zone_s,
        "avg_cadence_rpm": if is_run { None } else { opt_round(cadence, 0) },
        "avg_cadence_spm": if is_run { opt_round(cadence.map(|c| c * 2.0), 0) } else { None },
        "avg_grade_pct": opt_round(w.avg_grade_pct, 1),
        "power_hr_decoupling_pct": opt_round(crate::performance::decoupling_pct(series), 1),
    })
}

#[derive(Deserialize)]
pub(super) struct StreamsQuery {
    /// Bucket size in seconds (default 10, 1 to 300).
    step: Option<usize>,
    /// Window start/end in seconds since the activity start (e.g. a lap's
    /// `start_s` and `start_s + duration_s`).
    from_s: Option<usize>,
    to_s: Option<usize>,
}

/// `GET /api/v1/activities/{id}/streams?step=10&from_s=0&to_s=3600` — the
/// activity's time series (power, HR, cadence, altitude, distance, speed or
/// pace) averaged over `step` seconds, optionally restricted to a window.
pub(super) async fn activity_streams(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Query(q): Query<StreamsQuery>,
) -> ApiResult {
    let sport: Option<String> = sqlx::query_scalar("SELECT sport_type FROM activities WHERE id = ?")
        .bind(id)
        .fetch_optional(&state.pool)
        .await?;
    let Some(sport) = sport else {
        return Err(ApiError(StatusCode::NOT_FOUND, format!("activity {id} not found")));
    };
    let is_run = crate::metrics::is_run(&sport);
    let Some(series) = load_series(&state.pool, id, is_run).await? else {
        return Err(ApiError(StatusCode::NOT_FOUND, format!("activity {id} has no stream data")));
    };

    let step = q.step.unwrap_or(10).clamp(1, 300);
    let from = q.from_s.unwrap_or(0);
    let to = q.to_s.unwrap_or(series.len);
    if from >= to.min(series.len) {
        return Err(ApiError(StatusCode::BAD_REQUEST, "`from_s` must be before `to_s` and inside the activity".into()));
    }
    let d = crate::performance::downsample(&series, from, to, step);
    let round_all = |v: &Option<Vec<f64>>, digits: i32| v.as_ref().map(|v| v.iter().map(|x| round(*x, digits)).collect::<Vec<_>>());

    // Runs get pace (min/km, null while standing still) instead of speed.
    let (speed, pace) = match (&d.speed_kmh, is_run) {
        (Some(sp), true) => (
            None,
            Some(sp.iter().map(|v| if *v > 1.0 { Some(round(60.0 / v, 2)) } else { None }).collect::<Vec<_>>()),
        ),
        (sp, _) => (round_all(&sp.clone(), 1), None),
    };
    Ok(Json(json!({
        "activity_id": id,
        "step_s": step,
        "from_s": from,
        "count": d.time_s.len(),
        "time_s": d.time_s,
        "distance_km": round_all(&d.distance_km, 3),
        "speed_kmh": speed,
        "pace_min_per_km": pace,
        "power_w": round_all(&d.power_w, 0),
        "hr_bpm": round_all(&d.hr_bpm, 0),
        "cadence": round_all(&d.cadence, 0),
        "altitude_m": round_all(&d.altitude_m, 1),
    })))
}
