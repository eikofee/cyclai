//! Bearer-JWT guard for the exposed API router (`web::serve_api`).

use super::AppState;
use crate::apikeys::{authenticate, AuthError};
use axum::extract::{Request, State};
use axum::http::{header, Method, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::json;

fn deny(status: StatusCode, msg: &str) -> Response {
    let mut res = (status, Json(json!({ "error": msg }))).into_response();
    if status == StatusCode::UNAUTHORIZED {
        res.headers_mut().insert(header::WWW_AUTHENTICATE, "Bearer".parse().unwrap());
    }
    res
}

pub(super) async fn require_api_key(State(state): State<AppState>, req: Request, next: Next) -> Response {
    let token = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "));
    let Some(token) = token else {
        return deny(StatusCode::UNAUTHORIZED, "missing bearer token");
    };
    // `/projection` is a POST (it takes a body) but only simulates: read keys may call it.
    let is_write = !matches!(*req.method(), Method::GET | Method::HEAD) && req.uri().path() != "/api/v1/projection";
    match authenticate(&state.pool, token, is_write).await {
        Ok(_) => next.run(req).await,
        Err(AuthError::Forbidden) => deny(StatusCode::FORBIDDEN, "this key is read-only"),
        Err(AuthError::Invalid(reason)) => {
            // The reason stays in the logs; the client only learns "invalid".
            tracing::warn!(reason, "api request rejected");
            deny(StatusCode::UNAUTHORIZED, "invalid token")
        }
    }
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

    async fn app(scope: Scope) -> axum::Router {
        let pool = SqlitePoolOptions::new().connect("sqlite::memory:").await.unwrap();
        crate::db::migrate(&pool).await.unwrap();
        crate::db::seed_athlete_config(&pool, &test_config()).await.unwrap();
        add(&pool, "k", PUBLIC, scope).await.unwrap();
        api_router(AppState { pool })
    }

    fn test_config() -> crate::config::Config {
        crate::config::Config {
            database_url: "sqlite::memory:".into(),
            strava_client_id: String::new(),
            strava_client_secret: String::new(),
            strava_refresh_token: String::new(),
            anthropic_api_key: String::new(),
            ftp_watts: 200,
            hr_max: 190,
            hr_rest: 50,
            athlete_profile: String::new(),
        }
    }

    async fn status(app: &axum::Router, method: &str, uri: &str, auth: Option<String>) -> StatusCode {
        let mut req = Request::builder().method(method).uri(uri);
        if let Some(a) = auth {
            req = req.header("authorization", a);
        }
        app.clone().oneshot(req.body(Body::empty()).unwrap()).await.unwrap().status()
    }

    #[tokio::test]
    async fn api_requires_a_valid_token() {
        let app = app(Scope::Read).await;
        assert_eq!(status(&app, "GET", "/api/v1/profile", None).await, StatusCode::UNAUTHORIZED);
        assert_eq!(status(&app, "GET", "/api/v1/profile", Some("Bearer junk".into())).await, StatusCode::UNAUTHORIZED);
        let token = mint_token(PRIVATE, "k", 60).unwrap();
        assert_eq!(status(&app, "GET", "/api/v1/profile", Some(format!("Bearer {token}"))).await, StatusCode::OK);
    }

    #[tokio::test]
    async fn read_only_key_gets_403_on_writes() {
        let app = app(Scope::Read).await;
        let token = mint_token(PRIVATE, "k", 60).unwrap();
        assert_eq!(
            status(&app, "DELETE", "/api/v1/planned/1", Some(format!("Bearer {token}"))).await,
            StatusCode::FORBIDDEN
        );
    }

    async fn call(app: &axum::Router, method: &str, uri: &str, token: &str, body: Option<&str>) -> (StatusCode, serde_json::Value) {
        let mut req = Request::builder().method(method).uri(uri).header("authorization", format!("Bearer {token}"));
        if body.is_some() {
            req = req.header("content-type", "application/json");
        }
        let res = app.clone().oneshot(req.body(Body::from(body.unwrap_or("").to_string())).unwrap()).await.unwrap();
        let status = res.status();
        let bytes = axum::body::to_bytes(res.into_body(), usize::MAX).await.unwrap();
        (status, serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null))
    }

    #[tokio::test]
    async fn coach_note_roundtrip_needs_a_write_key() {
        let pool = SqlitePoolOptions::new().connect("sqlite::memory:").await.unwrap();
        crate::db::migrate(&pool).await.unwrap();
        add(&pool, "k", PUBLIC, Scope::Write).await.unwrap();
        let app = api_router(AppState { pool });
        let token = mint_token(PRIVATE, "k", 60).unwrap();

        assert_eq!(call(&app, "GET", "/api/v1/coach-note", &token, None).await.1["message"], serde_json::Value::Null);
        let (status, body) =
            call(&app, "PUT", "/api/v1/coach-note", &token, Some(r#"{"message":"  Semaine douce  ","author":"coach"}"#)).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["message"], "Semaine douce");
        assert_eq!(call(&app, "GET", "/api/v1/coach-note", &token, None).await.1["author"], "coach");
        let too_long = format!(r#"{{"message":"{}"}}"#, "x".repeat(1001));
        assert_eq!(call(&app, "PUT", "/api/v1/coach-note", &token, Some(&too_long)).await.0, StatusCode::BAD_REQUEST);
        assert_eq!(call(&app, "PUT", "/api/v1/coach-note", &token, Some(r#"{"message":""}"#)).await.1["message"], serde_json::Value::Null);
    }

    #[tokio::test]
    async fn workouts_are_filterable_and_catalogued() {
        let pool = SqlitePoolOptions::new().connect("sqlite::memory:").await.unwrap();
        crate::db::migrate(&pool).await.unwrap();
        crate::db::seed_athlete_config(&pool, &test_config()).await.unwrap();
        add(&pool, "k", PUBLIC, Scope::Read).await.unwrap();
        let steps = r#"{"name":"w","steps":[]}"#;
        for (name, cat, group, pos) in [("Alpha", Some("Depuis Zwift"), Some("plan-a"), 2), ("Beta", Some("Depuis Zwift"), Some("plan-a"), 1), ("Mine", None, None, 0)] {
            sqlx::query("INSERT INTO workouts (name, steps_json, category, group_key, group_name, position) VALUES (?, ?, ?, ?, 'Plan A', ?)")
                .bind(name).bind(steps).bind(cat).bind(group).bind(pos)
                .execute(&pool).await.unwrap();
        }
        let app = api_router(AppState { pool });
        let token = mint_token(PRIVATE, "k", 60).unwrap();

        let (_, all) = call(&app, "GET", "/api/v1/workouts", &token, None).await;
        assert_eq!(all["total"], 3);
        assert!(all["workouts"][0].get("workout").is_none(), "steps are opt-in");

        let (_, plan) = call(&app, "GET", "/api/v1/workouts?category=Depuis%20Zwift&group=plan-a&steps=true", &token, None).await;
        let names: Vec<_> = plan["workouts"].as_array().unwrap().iter().map(|w| w["name"].as_str().unwrap().to_string()).collect();
        assert_eq!(names, ["Beta", "Alpha"], "ordered by position");
        assert!(plan["workouts"][0].get("workout").is_some());

        assert_eq!(call(&app, "GET", "/api/v1/workouts?q=mi", &token, None).await.1["total"], 1);
        let (_, cats) = call(&app, "GET", "/api/v1/workouts/categories", &token, None).await;
        assert_eq!(cats["categories"][0]["groups"][0]["count"], 2);
        let id = all["workouts"][0]["id"].as_i64().unwrap();
        assert_eq!(call(&app, "GET", &format!("/api/v1/workouts/{id}"), &token, None).await.0, StatusCode::OK);
        assert_eq!(call(&app, "GET", "/api/v1/workouts/9999", &token, None).await.0, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn projection_is_readable_and_flags_rule_violations() {
        let pool = SqlitePoolOptions::new().connect("sqlite::memory:").await.unwrap();
        crate::db::migrate(&pool).await.unwrap();
        crate::db::seed_athlete_config(&pool, &test_config()).await.unwrap();
        add(&pool, "k", PUBLIC, Scope::Read).await.unwrap();
        // A history so daily_load exists: one activity 10 days ago.
        let day = (chrono::Utc::now() - chrono::Duration::days(10)).format("%Y-%m-%dT08:00:00Z").to_string();
        sqlx::query("INSERT INTO activities (id, started_at, duration_s, distance_m, elevation_gain_m, tss, intensity_factor) VALUES (1, ?, 3600, 30000, 100, 60, 0.7)")
            .bind(day).execute(&pool).await.unwrap();
        crate::metrics::recompute_daily_load(&pool).await.unwrap();
        let app = api_router(AppState { pool });
        let token = mint_token(PRIVATE, "k", 60).unwrap();

        let tomorrow = |n: i64| (chrono::Utc::now().date_naive() + chrono::Duration::days(n)).to_string();
        let body = serde_json::json!({ "sessions": [
            {"date": tomorrow(1), "tss": 150, "intensity_factor": 0.9},
            {"date": tomorrow(2), "tss": 150, "intensity_factor": 0.9},
            {"date": tomorrow(3), "tss": 150, "intensity_factor": 0.9},
        ]}).to_string();
        let (status, res) = call(&app, "POST", "/api/v1/projection", &token, Some(&body)).await;
        assert_eq!(status, StatusCode::OK, "{res}");
        assert_eq!(res["ok"], false);
        assert!(res["violations"].as_array().unwrap().iter().any(|v| v["rule"] == "consecutive_intense"));
        assert_eq!(res["days"].as_array().unwrap().len(), 3);

        let bad = serde_json::json!({ "sessions": [{"date": tomorrow(-1), "tss": 50}] }).to_string();
        assert_eq!(call(&app, "POST", "/api/v1/projection", &token, Some(&bad)).await.0, StatusCode::BAD_REQUEST);
        assert_eq!(call(&app, "GET", "/api/v1/rules", &token, None).await.1["min_tsb"], -25.0);
        // Other POSTs still need a write key.
        assert_eq!(call(&app, "POST", "/api/v1/planned", &token, Some("{}")).await.0, StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn proposals_are_idempotent_refused_on_rule_breaks_and_skippable() {
        let pool = SqlitePoolOptions::new().connect("sqlite::memory:").await.unwrap();
        crate::db::migrate(&pool).await.unwrap();
        crate::db::seed_athlete_config(&pool, &test_config()).await.unwrap();
        add(&pool, "k", PUBLIC, Scope::Write).await.unwrap();
        let day = (chrono::Utc::now() - chrono::Duration::days(10)).format("%Y-%m-%dT08:00:00Z").to_string();
        sqlx::query("INSERT INTO activities (id, started_at, duration_s, distance_m, elevation_gain_m, tss, intensity_factor) VALUES (1, ?, 3600, 30000, 100, 60, 0.7)")
            .bind(day).execute(&pool).await.unwrap();
        crate::metrics::recompute_daily_load(&pool).await.unwrap();
        // One hard template (1 h at ~100 % FTP: IF ≈ 1.0, TSS ≈ 100) and one easy one.
        let hard = r#"{"name":"Hard","steps":[{"type":"interval","duration":{"type":"time","value":"60min"},"target":{"type":"power_range","low":195,"high":205}}]}"#;
        let easy = r#"{"name":"Easy","steps":[{"type":"interval","duration":{"type":"time","value":"60min"},"target":{"type":"power_range","low":100,"high":110}}]}"#;
        for (id, steps) in [(1, hard), (2, easy)] {
            sqlx::query("INSERT INTO workouts (id, name, steps_json) VALUES (?, 'w', ?)").bind(id).bind(steps).execute(&pool).await.unwrap();
        }
        let app = api_router(AppState { pool: pool.clone() });
        let token = mint_token(PRIVATE, "k", 60).unwrap();
        let date = |n: i64| (chrono::Utc::now().date_naive() + chrono::Duration::days(n)).to_string();
        let propose = |n: i64, wid: i64, key: &str| {
            serde_json::json!({"date": date(n), "workout_id": wid, "rationale": "test", "idempotency_key": key}).to_string()
        };

        let (s, first) = call(&app, "POST", "/api/v1/proposals", &token, Some(&propose(1, 2, "a"))).await;
        assert_eq!(s, StatusCode::CREATED, "{first}");
        assert_eq!(first["status"], "proposed");
        let (s, again) = call(&app, "POST", "/api/v1/proposals", &token, Some(&propose(5, 1, "a"))).await;
        assert_eq!(s, StatusCode::OK, "same key returns the first proposal");
        assert_eq!(again["id"], first["id"]);

        // Proposals don't count in the load until confirmed: three hard days in a
        // row are only refused once the first two are on the calendar for real.
        for n in [2, 3] {
            let id: i64 = sqlx::query_scalar("INSERT INTO planned_workouts (planned_date, name, steps_json, status) VALUES (?, 'h', ?, 'confirmed') RETURNING id")
                .bind(date(n)).bind(hard).fetch_one(&pool).await.unwrap();
            assert!(id > 0);
        }
        let (s, refused) = call(&app, "POST", "/api/v1/proposals", &token, Some(&propose(4, 1, "b"))).await;
        assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{refused}");
        assert!(refused["error"].as_str().unwrap().contains("consecutive_intense"));
        let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM planned_workouts WHERE planned_date = ?").bind(date(4)).fetch_one(&pool).await.unwrap();
        assert_eq!(n, 0, "a refused proposal stores nothing");

        let skip_uri = format!("/api/v1/planned/{}/skip", first["id"]);
        let (s, skipped) = call(&app, "POST", &skip_uri, &token, Some(r#"{"reason":"fatigue"}"#)).await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(skipped["status"], "skipped");
        assert!(skipped["notes"].as_str().unwrap().contains("fatigue"));
        assert_eq!(call(&app, "POST", &skip_uri, &token, Some("{}")).await.0, StatusCode::CONFLICT, "final status");
        assert_eq!(call(&app, "POST", "/api/v1/planned/9999/skip", &token, Some("{}")).await.0, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn free_ride_proposals_are_bounded_by_availability_and_intent() {
        let pool = SqlitePoolOptions::new().connect("sqlite::memory:").await.unwrap();
        crate::db::migrate(&pool).await.unwrap();
        crate::db::seed_athlete_config(&pool, &test_config()).await.unwrap();
        add(&pool, "k", PUBLIC, Scope::Write).await.unwrap();
        // Some history, so the load projection has a starting point.
        let past = (chrono::Utc::now() - chrono::Duration::days(10)).format("%Y-%m-%dT08:00:00Z").to_string();
        sqlx::query("INSERT INTO activities (id, started_at, duration_s, distance_m, elevation_gain_m, tss, intensity_factor) VALUES (1, ?, 3600, 30000, 100, 60, 0.7)")
            .bind(past).execute(&pool).await.unwrap();
        crate::metrics::recompute_daily_load(&pool).await.unwrap();
        let app = api_router(AppState { pool: pool.clone() });
        let token = mint_token(PRIVATE, "k", 60).unwrap();
        let date = |n: i64| (chrono::Utc::now().date_naive() + chrono::Duration::days(n)).to_string();
        let post = |body: serde_json::Value| {
            let (app, token) = (app.clone(), token.clone());
            async move { call(&app, "POST", "/api/v1/proposals", &token, Some(&body.to_string())).await }
        };

        // 60 min fits every default day; the engine fills in the TSS range.
        let (s, ride) = post(serde_json::json!({
            "date": date(1), "rationale": "souplesse", "idempotency_key": "fr1",
            "free_ride_budget": {"duration_max_min": 60, "intent": "endurance"}
        })).await;
        assert_eq!(s, StatusCode::CREATED, "{ride}");
        assert_eq!((ride["kind"].as_str(), ride["status"].as_str()), (Some("free_ride"), Some("proposed")));
        assert_eq!(ride["free_ride_budget"]["duration_max_min"], 60);
        assert_eq!(ride["free_ride_budget"]["tss_range"][1], 56.0, "60 min at the endurance IF cap");
        assert_eq!(ride["activity_id"], serde_json::Value::Null);

        // Replaying the key returns it instead of creating another.
        let (s, again) = post(serde_json::json!({
            "date": date(3), "rationale": "x", "idempotency_key": "fr1",
            "free_ride_budget": {"duration_max_min": 60, "intent": "endurance"}
        })).await;
        assert_eq!((s, &again["id"]), (StatusCode::OK, &ride["id"]));

        // Longer than the day allows (max 180 on the default week-end days).
        let (s, e) = post(serde_json::json!({"date": date(1), "rationale": "x", "free_ride_budget": {"duration_max_min": 600, "intent": "endurance"}})).await;
        assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{e}");
        assert!(e["error"].as_str().unwrap().contains("available"));

        // An availability exception opens the day up, but the TSS stays bounded by the intent.
        let exc = format!(r#"{{"date":"{}","available_min":400,"time_of_day":"journee","context":"vacances"}}"#, date(2));
        assert_eq!(call(&app, "PUT", "/api/v1/availability", &token, Some(&exc)).await.0, StatusCode::OK);
        let (s, e) = post(serde_json::json!({"date": date(2), "rationale": "x", "free_ride_budget": {"duration_max_min": 300, "intent": "denivele", "tss_range": [100, 500]}})).await;
        assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{e}");
        assert!(e["error"].as_str().unwrap().contains("out of reach"));
        let (s, ok) = post(serde_json::json!({"date": date(2), "rationale": "x", "free_ride_budget": {"duration_max_min": 120, "intent": "denivele", "tss_range": [40, 80]}})).await;
        assert_eq!(s, StatusCode::CREATED, "{ok}");

        // Bad shapes.
        let (s, _) = post(serde_json::json!({"date": date(1), "rationale": "x"})).await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "neither workout nor budget");
        let (s, _) = post(serde_json::json!({"date": date(1), "rationale": "x", "workout_id": 1, "free_ride_budget": {"duration_max_min": 60, "intent": "endurance"}})).await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "both");
        let (s, _) = post(serde_json::json!({"date": date(1), "rationale": "x", "free_ride_budget": {"duration_max_min": 60, "intent": "sprint"}})).await;
        assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "unknown intent");

        // A proposal isn't counted; once confirmed, the free ride loads the projection at mid-budget.
        sqlx::query("UPDATE planned_workouts SET status = 'confirmed' WHERE id = ?").bind(ok["id"].as_i64().unwrap()).execute(&pool).await.unwrap();
        let proj = serde_json::json!({"sessions": []}).to_string();
        let (s, res) = call(&app, "POST", "/api/v1/projection", &token, Some(&proj)).await;
        assert_eq!(s, StatusCode::OK, "{res}");
        let day = res["days"].as_array().unwrap().iter().find(|d| d["date"] == date(2)).expect("day of the free ride is projected");
        assert_eq!(day["tss"], 60.0, "middle of 40..80");
    }

    #[tokio::test]
    async fn ride_envelope_learns_from_history_and_checks_the_rules() {
        let pool = SqlitePoolOptions::new().connect("sqlite::memory:").await.unwrap();
        crate::db::migrate(&pool).await.unwrap();
        crate::db::seed_athlete_config(&pool, &test_config()).await.unwrap();
        add(&pool, "k", PUBLIC, Scope::Read).await.unwrap();
        // Four rides of ~70 min (TSS 40..55) on separate days, a few weeks back.
        for (i, tss) in [40.0, 45.0, 50.0, 55.0].into_iter().enumerate() {
            let started = (chrono::Utc::now() - chrono::Duration::days(20 + 3 * i as i64)).format("%Y-%m-%dT17:00:00Z").to_string();
            sqlx::query("INSERT INTO activities (id, started_at, duration_s, distance_m, elevation_gain_m, tss, intensity_factor, sport_type) VALUES (?, ?, 4200, 30000, 150, ?, 0.7, 'Ride')")
                .bind(i as i64 + 1).bind(started).bind(tss).execute(&pool).await.unwrap();
        }
        crate::metrics::recompute_daily_load(&pool).await.unwrap();
        let app = api_router(AppState { pool: pool.clone() });
        let token = mint_token(PRIVATE, "k", 60).unwrap();
        let date = |n: i64| (chrono::Utc::now().date_naive() + chrono::Duration::days(n)).to_string();
        let get = |uri: String| {
            let (app, token) = (app.clone(), token.clone());
            async move { call(&app, "GET", &uri, &token, None).await }
        };

        // 90 min: only the two shorter scenarios fit; the 70-min one is learned from the 4 rides.
        let (s, env) = get(format!("/api/v1/ride-envelope?date={}&available_min=90", date(1))).await;
        assert_eq!(s, StatusCode::OK, "{env}");
        let names: Vec<_> = env["scenarios"].as_array().unwrap().iter().map(|s| s["scenario"].as_str().unwrap().to_string()).collect();
        assert_eq!(names, ["tranquille", "moyenne"]);
        assert_eq!(env["slot_type"], "soiree_courte");
        assert_eq!(env["available_source"], "parameter");
        let moyenne = &env["scenarios"][1];
        assert_eq!((moyenne["duration_min"].clone(), moyenne["tss_source"].as_str(), moyenne["source"].as_str()), (serde_json::json!(70), Some("history"), Some("computed")));
        assert_eq!(moyenne["sample_size"], 4);
        assert!(moyenne["tsb_after"].is_number());
        assert_eq!(moyenne["feasibility"], "ok");

        // Time defaults to the day's availability; a short slot has no scenario.
        let (_, dflt) = get(format!("/api/v1/ride-envelope?date={}", date(1))).await;
        assert!(["regle_par_defaut", "exception"].contains(&dflt["available_source"].as_str().unwrap()));
        assert_eq!(get(format!("/api/v1/ride-envelope?date={}&available_min=10", date(1))).await.1["scenarios"].as_array().unwrap().len(), 0);

        // Two hard days already confirmed: a big ride the day after is inadvisable.
        let hard = r#"{"name":"Hard","steps":[{"type":"interval","duration":{"type":"time","value":"60min"},"target":{"type":"power_range","low":195,"high":205}}]}"#;
        for n in [1, 2] {
            sqlx::query("INSERT INTO planned_workouts (planned_date, name, steps_json, status) VALUES (?, 'h', ?, 'confirmed')")
                .bind(date(n)).bind(hard).execute(&pool).await.unwrap();
        }
        let (_, env) = get(format!("/api/v1/ride-envelope?date={}&available_min=240", date(3))).await;
        let by_name = |n: &str| env["scenarios"].as_array().unwrap().iter().find(|s| s["scenario"] == n).unwrap().clone();
        assert_eq!(by_name("longue_denivele")["feasibility"], "deconseille");
        assert!(by_name("longue_denivele")["rule"]["rule"].is_string(), "the rule that explains it");
        assert_ne!(by_name("tranquille")["feasibility"], "ok");

        // A session already planned that very day is ignored: the envelope is what could replace it.
        let (_, same) = get(format!("/api/v1/ride-envelope?date={}&available_min=90", date(1))).await;
        assert_ne!(same["scenarios"][1]["feasibility"], serde_json::Value::Null);

        assert_eq!(get(format!("/api/v1/ride-envelope?date={}", date(-1))).await.0, StatusCode::BAD_REQUEST);
        assert_eq!(get("/api/v1/ride-envelope?date=nope".into()).await.0, StatusCode::BAD_REQUEST);
        assert_eq!(get(format!("/api/v1/ride-envelope?date={}&available_min=9999", date(1))).await.0, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn workout_search_filters_on_computed_zone_time_and_ranks_by_tss() {
        let pool = SqlitePoolOptions::new().connect("sqlite::memory:").await.unwrap();
        crate::db::migrate(&pool).await.unwrap();
        crate::db::seed_athlete_config(&pool, &test_config()).await.unwrap();
        add(&pool, "k", PUBLIC, Scope::Read).await.unwrap();
        let step = |min: u32, pct: u32| format!(r#"{{"type":"interval","duration":{{"type":"time","value":"{min}min"}},"target":{{"type":"ftp_pct_range","low":{pct},"high":{pct}}}}}"#);
        // 1: 60 min endurance (65 %). 2: 10 + 2x(20 min @ 100 % / 5 min @ 50 %) + 10 = threshold.
        // 3: 5x(3 min @ 115 % / 3 min @ 50 %) = vo2.
        let w1 = format!(r#"{{"name":"Z2","steps":[{}]}}"#, step(60, 65));
        let w2 = format!(r#"{{"name":"Seuil","steps":[{},{{"type":"repeat","count":2,"steps":[{},{}]}},{}]}}"#, step(10, 55), step(20, 100), step(5, 50), step(10, 55));
        let w3 = format!(r#"{{"name":"VO2","steps":[{{"type":"repeat","count":5,"steps":[{},{}]}}]}}"#, step(3, 115), step(3, 50));
        for (id, name, steps) in [(1, "Z2", &w1), (2, "Seuil", &w2), (3, "VO2", &w3)] {
            sqlx::query("INSERT INTO workouts (id, name, steps_json, category, group_key, group_name) VALUES (?, ?, ?, 'Depuis Zwift', 'plan-a', 'Plan A')")
                .bind(id).bind(name).bind(steps).execute(&pool).await.unwrap();
        }
        let app = api_router(AppState { pool: pool.clone() });
        let token = mint_token(PRIVATE, "k", 60).unwrap();
        let get = |uri: &str| {
            let (app, token, uri) = (app.clone(), token.clone(), uri.to_string());
            async move { call(&app, "GET", &uri, &token, None).await }
        };
        let ids = |v: &serde_json::Value| -> Vec<i64> { v["workouts"].as_array().unwrap().iter().map(|w| w["id"].as_i64().unwrap()).collect() };

        // Computed fields: zone minutes at current FTP, IF, tags, plan of origin.
        let (s, one) = get("/api/v1/workouts/2").await;
        assert_eq!(s, StatusCode::OK, "{one}");
        assert_eq!(one["power_zone_minutes"][3], 40.0, "{one}");
        assert!(one["tags"].as_array().unwrap().iter().any(|t| t == "seuil"));
        assert_eq!(one["plan"]["group_name"], "Plan A");
        assert!(one["structure_summary"].as_str().unwrap().contains("@100%"), "{one}");
        assert!(one["intensity_factor"].as_f64().unwrap() > 0.7);
        assert!(one["workout"]["steps"].is_array());

        let (_, z4) = get("/api/v1/workouts?min_zone_time=4:30").await;
        assert_eq!(ids(&z4), [2]);
        let (_, z5) = get("/api/v1/workouts?min_zone_time=5:10").await;
        assert_eq!(ids(&z5), [3]);
        let (_, endurance) = get("/api/v1/workouts?tag=endurance").await;
        assert_eq!(ids(&endurance), [1]);
        let (_, hard) = get("/api/v1/workouts?tag=seuil,vo2").await;
        assert_eq!(ids(&hard).len(), 2);
        assert_eq!(get("/api/v1/workouts?min_zone_time=9:5").await.0, StatusCode::BAD_REQUEST);
        assert_eq!(get("/api/v1/workouts?min_zone_time=oops").await.0, StatusCode::BAD_REQUEST);

        // Ranking by closeness to a target TSS: 60 min @ 65 % ≈ 42 TSS comes first for 43.
        let (_, ranked) = get("/api/v1/workouts?target_tss=43").await;
        assert_eq!(ids(&ranked)[0], 1);

        // A template done 3 days ago is left out when asking for 7 days of freshness.
        let done = (chrono::Utc::now().date_naive() - chrono::Duration::days(3)).to_string();
        sqlx::query("INSERT INTO planned_workouts (planned_date, name, steps_json, workout_id, status) VALUES (?, 'x', ?, 1, 'done')")
            .bind(&done).bind(&w1).execute(&pool).await.unwrap();
        let (_, fresh) = get("/api/v1/workouts?exclude_done_within_days=7").await;
        assert!(!ids(&fresh).contains(&1));
        let (_, older) = get("/api/v1/workouts?exclude_done_within_days=2").await;
        assert!(ids(&older).contains(&1));
        let (_, all) = get("/api/v1/workouts").await;
        let w1_item = all["workouts"].as_array().unwrap().iter().find(|w| w["id"] == 1).unwrap();
        assert_eq!(w1_item["last_done"], done.as_str());
    }

    #[tokio::test]
    async fn activities_expose_indoor_power_source_and_hr() {
        let pool = SqlitePoolOptions::new().connect("sqlite::memory:").await.unwrap();
        crate::db::migrate(&pool).await.unwrap();
        crate::db::seed_athlete_config(&pool, &test_config()).await.unwrap();
        add(&pool, "k", PUBLIC, Scope::Read).await.unwrap();
        let day = |n: i64| (chrono::Utc::now() - chrono::Duration::days(n)).format("%Y-%m-%dT08:00:00Z").to_string();
        let rows: [(i64, &str, Option<f64>, Option<f64>); 3] = [
            (1, "VirtualRide", Some(180.0), Some(140.0)),
            (2, "Ride", Some(170.0), None),
            (3, "Ride", None, Some(130.0)),
        ];
        for (id, sport, power, hr) in rows {
            sqlx::query("INSERT INTO activities (id, started_at, duration_s, distance_m, elevation_gain_m, sport_type, avg_power_w, avg_hr) VALUES (?, ?, 3600, 30000, 100, ?, ?, ?)")
                .bind(id).bind(day(id)).bind(sport).bind(power).bind(hr).execute(&pool).await.unwrap();
        }
        let app = api_router(AppState { pool });
        let token = mint_token(PRIVATE, "k", 60).unwrap();
        let (_, list) = call(&app, "GET", "/api/v1/activities", &token, None).await;
        let by_id = |id: i64| list["activities"].as_array().unwrap().iter().find(|a| a["id"] == id).unwrap().clone();
        assert_eq!((by_id(1)["indoor"].clone(), by_id(1)["power_source"].clone(), by_id(1)["has_hr"].clone()), (true.into(), "trainer".into(), true.into()));
        assert_eq!((by_id(2)["indoor"].clone(), by_id(2)["power_source"].clone(), by_id(2)["has_hr"].clone()), (false.into(), "pedals".into(), false.into()));
        assert_eq!((by_id(3)["power_source"].clone(), by_id(3)["has_hr"].clone()), ("none".into(), true.into()));
    }

    #[tokio::test]
    async fn proposals_can_offer_checked_alternatives() {
        let pool = SqlitePoolOptions::new().connect("sqlite::memory:").await.unwrap();
        crate::db::migrate(&pool).await.unwrap();
        crate::db::seed_athlete_config(&pool, &test_config()).await.unwrap();
        add(&pool, "k", PUBLIC, Scope::Write).await.unwrap();
        let day = (chrono::Utc::now() - chrono::Duration::days(10)).format("%Y-%m-%dT08:00:00Z").to_string();
        sqlx::query("INSERT INTO activities (id, started_at, duration_s, distance_m, elevation_gain_m, tss, intensity_factor) VALUES (1, ?, 3600, 30000, 100, 60, 0.7)")
            .bind(day).execute(&pool).await.unwrap();
        crate::metrics::recompute_daily_load(&pool).await.unwrap();
        let steps = |pct: u32| format!(r#"{{"name":"S{pct}","steps":[{{"type":"interval","duration":{{"type":"time","value":"60min"}},"target":{{"type":"ftp_pct_range","low":{pct},"high":{pct}}}}}]}}"#);
        // 1 and 2: easy options. 3: a monster (3 h at 110 %) that no rule allows.
        let monster = r#"{"name":"Monster","steps":[{"type":"interval","duration":{"type":"time","value":"180min"},"target":{"type":"ftp_pct_range","low":110,"high":110}}]}"#;
        for (id, s) in [(1, steps(65)), (2, steps(55)), (3, monster.to_string())] {
            sqlx::query("INSERT INTO workouts (id, name, steps_json) VALUES (?, ?, ?)").bind(id).bind(format!("W{id}")).bind(s).execute(&pool).await.unwrap();
        }
        let app = api_router(AppState { pool: pool.clone() });
        let token = mint_token(PRIVATE, "k", 60).unwrap();
        let tomorrow = (chrono::Utc::now().date_naive() + chrono::Duration::days(1)).to_string();
        let propose = |extra: serde_json::Value| {
            let mut body = serde_json::json!({"date": tomorrow, "workout_id": 1, "rationale": "r"});
            body.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
            let (app, token) = (app.clone(), token.clone());
            async move { call(&app, "POST", "/api/v1/proposals", &token, Some(&body.to_string())).await }
        };

        let (s, ok) = propose(serde_json::json!({"alternatives": [{"workout_id": 2, "note": "plus doux"}]})).await;
        assert_eq!(s, StatusCode::CREATED, "{ok}");
        assert_eq!(ok["alternatives"][0]["workout_id"], 2);
        assert_eq!(ok["alternatives"][0]["name"], "S55");
        assert_eq!(ok["alternatives"][0]["note"], "plus doux");
        assert!(ok["alternatives"][0]["estimated_tss"].as_f64().unwrap() > 0.0);

        // Each alternative is checked like the main session; bad shapes are refused.
        let (s, refused) = propose(serde_json::json!({"alternatives": [{"workout_id": 3}]})).await;
        assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{refused}");
        assert!(refused["error"].as_str().unwrap().contains("alternative 3"), "{refused}");
        assert_eq!(propose(serde_json::json!({"alternatives": [{"workout_id": 1}]})).await.0, StatusCode::BAD_REQUEST, "same as main");
        assert_eq!(propose(serde_json::json!({"alternatives": [{"workout_id": 2}, {"workout_id": 2}]})).await.0, StatusCode::BAD_REQUEST, "duplicate");
        assert_eq!(propose(serde_json::json!({"alternatives": [{"workout_id": 2},{"workout_id": 3},{"workout_id": 4},{"workout_id": 5}]})).await.0, StatusCode::BAD_REQUEST, "too many");
        assert_eq!(propose(serde_json::json!({"alternatives": [{"workout_id": 999}]})).await.0, StatusCode::NOT_FOUND);
        let (s, _) = propose(serde_json::json!({"workout_id": null, "free_ride_budget": {"duration_max_min": 60, "intent": "endurance"}, "alternatives": [{"workout_id": 2}]})).await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "alternatives only go with a library session");
        let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM planned_workouts").fetch_one(&pool).await.unwrap();
        assert_eq!(n, 1, "refused proposals leave nothing behind");
    }

    #[tokio::test]
    async fn checkins_are_validated_and_replace_the_day() {
        let pool = SqlitePoolOptions::new().connect("sqlite::memory:").await.unwrap();
        crate::db::migrate(&pool).await.unwrap();
        add(&pool, "k", PUBLIC, Scope::Write).await.unwrap();
        let app = api_router(AppState { pool });
        let token = mint_token(PRIVATE, "k", 60).unwrap();
        let today = chrono::Utc::now().date_naive().to_string();
        let uri = format!("/api/v1/checkins/{today}");

        assert_eq!(call(&app, "PUT", &uri, &token, Some(r#"{"legs":6,"motivation":3}"#)).await.0, StatusCode::BAD_REQUEST);
        assert_eq!(call(&app, "PUT", "/api/v1/checkins/nope", &token, Some(r#"{"legs":3,"motivation":3}"#)).await.0, StatusCode::BAD_REQUEST);
        assert_eq!(call(&app, "PUT", &uri, &token, Some(r#"{"legs":2,"motivation":4,"note":"lourdes"}"#)).await.0, StatusCode::OK);
        assert_eq!(call(&app, "PUT", &uri, &token, Some(r#"{"legs":4,"motivation":4,"rhr_bpm":48}"#)).await.0, StatusCode::OK);
        let (_, list) = call(&app, "GET", "/api/v1/checkins", &token, None).await;
        assert_eq!(list["count"], 1);
        assert_eq!(list["checkins"][0]["legs"], 4);
        assert_eq!(list["checkins"][0]["rhr_bpm"], 48);
    }

    #[tokio::test]
    async fn availability_exceptions_override_the_defaults() {
        let pool = SqlitePoolOptions::new().connect("sqlite::memory:").await.unwrap();
        crate::db::migrate(&pool).await.unwrap();
        add(&pool, "k", PUBLIC, Scope::Write).await.unwrap();
        let app = api_router(AppState { pool });
        let token = mint_token(PRIVATE, "k", 60).unwrap();
        let today = chrono::Utc::now().date_naive();
        let in3 = today + chrono::Duration::days(3);

        let (status, body) = call(&app, "GET", "/api/v1/availability", &token, None).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["count"], 14);
        assert!(body["days"].as_array().unwrap().iter().all(|d| d["source"] == "regle_par_defaut"));

        let put = |json: String| {
            let (app, token) = (app.clone(), token.clone());
            async move { call(&app, "PUT", "/api/v1/availability", &token, Some(&json)).await }
        };
        let ok = format!(r#"{{"date":"{in3}","available_min":240,"time_of_day":"journee","context":"vacances","note":"congés"}}"#);
        assert_eq!(put(ok).await.0, StatusCode::OK);
        let (_, body) = call(&app, "GET", &format!("/api/v1/availability?from={in3}&to={in3}"), &token, None).await;
        assert_eq!(body["days"][0]["available_min"], 240);
        assert_eq!(body["days"][0]["source"], "exception");
        assert_eq!(body["days"][0]["note"], "congés");

        // Range, then validation errors.
        let range = format!(r#"{{"from":"{today}","to":"{in3}","available_min":60,"time_of_day":"soir","context":"travail"}}"#);
        let (status, body) = call(&app, "PUT", "/api/v1/availability", &token, Some(&range)).await;
        assert_eq!((status, body["updated"].clone()), (StatusCode::OK, serde_json::json!(4)));
        let past = format!(r#"{{"date":"{}","available_min":60,"time_of_day":"soir","context":"travail"}}"#, today - chrono::Duration::days(1));
        assert_eq!(put(past).await.0, StatusCode::BAD_REQUEST);
        let bad_min = format!(r#"{{"date":"{in3}","available_min":9999,"time_of_day":"soir","context":"travail"}}"#);
        assert_eq!(put(bad_min).await.0, StatusCode::BAD_REQUEST);
        let bad_ctx = format!(r#"{{"date":"{in3}","available_min":60,"time_of_day":"soir","context":"boulot"}}"#);
        assert_eq!(put(bad_ctx).await.0, StatusCode::BAD_REQUEST);
        assert_eq!(put(r#"{"available_min":60,"time_of_day":"soir","context":"travail"}"#.into()).await.0, StatusCode::BAD_REQUEST, "no date");
    }

    #[tokio::test]
    async fn api_router_does_not_serve_the_ui() {
        let app = app(Scope::Read).await;
        let token = mint_token(PRIVATE, "k", 60).unwrap();
        assert_eq!(status(&app, "GET", "/settings", None).await, StatusCode::UNAUTHORIZED);
        assert_eq!(status(&app, "GET", "/settings", Some(format!("Bearer {token}"))).await, StatusCode::NOT_FOUND);
    }
}
