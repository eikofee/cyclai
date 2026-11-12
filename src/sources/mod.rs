//! Activity sources. CyclAI works on its own with manual `.fit`/`.gpx` imports
//! (`source = 'file'`); Strava and Garmin are plugins, disabled until the
//! user enables them in /settings. A synchronisation walks every enabled and
//! configured plugin in priority order; an activity that overlaps one already
//! stored (more than the tolerance) is ignored, so the first source to bring
//! a ride wins.

mod garmin_connect;
mod strava_source;

use crate::importer::ParsedLap;
use async_trait::async_trait;
use serde_json::Value as Json;
use sqlx::SqlitePool;

/// Plugin ids, in their default priority order.
pub const PLUGINS: [&str; 2] = ["strava", "garmin"];

pub fn label(source: &str) -> &'static str {
    match source {
        "strava" => "Strava",
        "garmin" => "Garmin Connect",
        "file" => "Fichier .fit/.gpx",
        _ => "Source inconnue",
    }
}

/// One activity as listed by a source, before anything is downloaded.
#[derive(Clone, Debug)]
pub struct Listed {
    pub external_id: String,
    pub title: String,
    /// RFC 3339, UTC.
    pub started_at: String,
    pub start_epoch: i64,
    /// Known from the listing for some sources (Strava), not for others.
    pub duration_s: Option<i64>,
    pub sport_type: String,
    /// Whatever the plugin needs back in `fetch`.
    pub raw: Json,
}

/// A complete activity, ready to be stored.
pub struct NewActivity {
    pub source: &'static str,
    pub external_id: Option<String>,
    pub started_at: String,
    pub sport_type: String,
    pub duration_s: i64,
    pub distance_m: f64,
    pub elevation_gain_m: f64,
    pub avg_power_w: Option<f64>,
    pub normalized_power_w: Option<f64>,
    pub avg_hr: Option<f64>,
    pub max_hr: Option<f64>,
    pub title: String,
    pub description: Option<String>,
    pub streams: Json,
    pub laps: Vec<ParsedLap>,
}

#[async_trait]
pub trait ActivitySource: Send + Sync {
    fn id(&self) -> &'static str;
    /// Activities started after `after_epoch` (0 = the whole history), limited
    /// to the sports CyclAI handles.
    async fn list(&self, after_epoch: i64) -> anyhow::Result<Vec<Listed>>;
    /// Downloads one activity; `None` when the source has nothing to give for it.
    async fn fetch(&self, listed: &Listed) -> anyhow::Result<Option<NewActivity>>;
    /// Called once the run is over (e.g. to keep a rotated token).
    async fn finish(&self, _pool: &SqlitePool) -> anyhow::Result<()> {
        Ok(())
    }
}

/// Connects a plugin; `None` when it isn't configured (missing credentials).
pub async fn connect(id: &str, pool: &SqlitePool) -> anyhow::Result<Option<Box<dyn ActivitySource>>> {
    match id {
        "strava" => strava_source::connect(pool).await,
        "garmin" => garmin_connect::connect(pool).await,
        other => anyhow::bail!("source inconnue : {other}"),
    }
}

// ───────────────────────── Configuration ─────────────────────────

#[derive(Clone, Debug, PartialEq)]
pub struct SourceConfig {
    /// Every plugin once, highest priority first.
    pub order: Vec<String>,
    pub enabled: Vec<String>,
    pub tolerance_s: i64,
    pub garmin_api_key: String,
}

impl SourceConfig {
    /// Enabled plugins, highest priority first.
    pub fn active(&self) -> Vec<&str> {
        self.order.iter().filter(|s| self.enabled.contains(s)).map(String::as_str).collect()
    }
}

/// Keeps only known plugins, once each, and appends the missing ones.
pub fn normalize_order(order: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for s in order {
        if PLUGINS.contains(&s.as_str()) && !out.contains(s) {
            out.push(s.clone());
        }
    }
    for p in PLUGINS {
        if !out.iter().any(|s| s == p) {
            out.push(p.to_string());
        }
    }
    out
}

pub const DEFAULT_TOLERANCE_S: i64 = 10;
pub const MAX_TOLERANCE_S: i64 = 3600;

pub async fn config(pool: &SqlitePool) -> SourceConfig {
    let row: Option<(String, String, i64, String)> =
        sqlx::query_as("SELECT source_order, enabled_sources, overlap_tolerance_s, garmin_api_key FROM settings WHERE id = 1")
            .fetch_optional(pool)
            .await
            .ok()
            .flatten();
    let (order, enabled, tolerance_s, garmin_api_key) =
        row.unwrap_or_else(|| ("[]".into(), "[]".into(), DEFAULT_TOLERANCE_S, String::new()));
    let order: Vec<String> = serde_json::from_str(&order).unwrap_or_default();
    let enabled: Vec<String> = serde_json::from_str(&enabled).unwrap_or_default();
    SourceConfig {
        order: normalize_order(&order),
        enabled: enabled.into_iter().filter(|s| PLUGINS.contains(&s.as_str())).collect(),
        tolerance_s: tolerance_s.clamp(0, MAX_TOLERANCE_S),
        garmin_api_key,
    }
}

pub async fn save_order_and_enabled(pool: &SqlitePool, order: &[String], enabled: &[String]) -> anyhow::Result<()> {
    let order = normalize_order(order);
    let enabled: Vec<&String> = order.iter().filter(|s| enabled.contains(s)).collect();
    sqlx::query("UPDATE settings SET source_order = ?, enabled_sources = ? WHERE id = 1")
        .bind(serde_json::to_string(&order)?)
        .bind(serde_json::to_string(&enabled)?)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn save_tolerance(pool: &SqlitePool, seconds: i64) -> anyhow::Result<()> {
    sqlx::query("UPDATE settings SET overlap_tolerance_s = ? WHERE id = 1")
        .bind(seconds.clamp(0, MAX_TOLERANCE_S))
        .execute(pool)
        .await?;
    Ok(())
}

/// Whether the plugin has the credentials it needs.
pub async fn is_configured(pool: &SqlitePool, id: &str) -> bool {
    match id {
        "strava" => {
            let id_secret: Option<(String, String)> =
                sqlx::query_as("SELECT strava_client_id, strava_client_secret FROM settings WHERE id = 1").fetch_optional(pool).await.ok().flatten();
            let token: Option<Option<String>> = sqlx::query_scalar("SELECT strava_refresh_token FROM sync_state WHERE id = 1")
                .fetch_optional(pool)
                .await
                .ok()
                .flatten();
            id_secret.is_some_and(|(i, s)| !i.trim().is_empty() && !s.trim().is_empty())
                && token.flatten().is_some_and(|t| !t.trim().is_empty())
        }
        "garmin" => !config(pool).await.garmin_api_key.trim().is_empty(),
        _ => false,
    }
}

/// Plugins a synchronisation will query, highest priority first.
pub async fn active_and_configured(pool: &SqlitePool) -> Vec<String> {
    let cfg = config(pool).await;
    let mut out = Vec::new();
    for id in cfg.active() {
        if is_configured(pool, id).await {
            out.push(id.to_string());
        }
    }
    out
}

pub async fn last_sync_epoch(pool: &SqlitePool, source: &str) -> i64 {
    sqlx::query_scalar("SELECT last_sync_epoch FROM source_sync_state WHERE source = ?")
        .bind(source)
        .fetch_optional(pool)
        .await
        .ok()
        .flatten()
        .unwrap_or(0)
}

pub async fn set_last_sync_epoch(pool: &SqlitePool, source: &str, epoch: i64) -> anyhow::Result<()> {
    sqlx::query(
        "INSERT INTO source_sync_state (source, last_sync_epoch) VALUES (?, ?) \
         ON CONFLICT (source) DO UPDATE SET last_sync_epoch = excluded.last_sync_epoch",
    )
    .bind(source)
    .bind(epoch)
    .execute(pool)
    .await?;
    Ok(())
}

// ───────────────────────── Storage ─────────────────────────

/// Id for an activity that does not come from Strava (Strava's are positive and
/// kept as the primary key, so the two spaces never collide).
pub async fn next_negative_id(pool: &SqlitePool) -> anyhow::Result<i64> {
    let (min_id,): (i64,) = sqlx::query_as("SELECT COALESCE(MIN(id), 0) FROM activities").fetch_one(pool).await?;
    Ok(min_id.min(0) - 1)
}

/// Already stored with its streams (an interrupted import is redone).
pub async fn is_known(pool: &SqlitePool, source: &str, external_id: &str) -> bool {
    sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM activities a JOIN activity_streams s ON s.activity_id = a.id WHERE a.source = ? AND a.external_id = ?",
    )
    .bind(source)
    .bind(external_id)
    .fetch_one(pool)
    .await
    .map(|n| n > 0)
    .unwrap_or(false)
}

pub async fn is_ignored(pool: &SqlitePool, source: &str, external_id: &str) -> bool {
    sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM ignored_activities WHERE source = ? AND external_id = ?")
        .bind(source)
        .bind(external_id)
        .fetch_one(pool)
        .await
        .map(|n| n > 0)
        .unwrap_or(false)
}

pub async fn forget_ignored(pool: &SqlitePool, source: &str) -> anyhow::Result<()> {
    sqlx::query("DELETE FROM ignored_activities WHERE source = ?").bind(source).execute(pool).await?;
    Ok(())
}

pub async fn record_ignored(pool: &SqlitePool, source: &str, listed: &Listed, overlaps: i64) -> anyhow::Result<()> {
    sqlx::query(
        "INSERT OR REPLACE INTO ignored_activities (source, external_id, overlaps_activity_id, title, started_at) VALUES (?, ?, ?, ?, ?)",
    )
    .bind(source)
    .bind(&listed.external_id)
    .bind(overlaps)
    .bind(&listed.title)
    .bind(&listed.started_at)
    .execute(pool)
    .await?;
    Ok(())
}

/// An already stored activity that shares more than `tolerance_s` seconds with
/// `[start_epoch, start_epoch + duration_s]`.
pub async fn find_overlap(pool: &SqlitePool, start_epoch: i64, duration_s: i64, tolerance_s: i64) -> Option<i64> {
    let end = start_epoch + duration_s.max(0);
    sqlx::query_scalar(
        "SELECT id FROM activities \
         WHERE MIN(CAST(strftime('%s', started_at) AS INTEGER) + duration_s, ?1) - MAX(CAST(strftime('%s', started_at) AS INTEGER), ?2) > ?3 \
         ORDER BY id LIMIT 1",
    )
    .bind(end)
    .bind(start_epoch)
    .bind(tolerance_s)
    .fetch_optional(pool)
    .await
    .ok()
    .flatten()
}

pub fn epoch_of(rfc3339: &str) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(rfc3339).ok().map(|d| d.timestamp())
}

/// Writes an activity with its streams, route, bests and laps. A Strava id
/// is kept as the primary key; anything else gets a negative id. An earlier
/// interrupted import of the same activity is replaced.
pub async fn insert(pool: &SqlitePool, a: &NewActivity) -> anyhow::Result<i64> {
    if let Some(ext) = &a.external_id {
        let old: Option<i64> = sqlx::query_scalar("SELECT id FROM activities WHERE source = ? AND external_id = ?")
            .bind(a.source)
            .bind(ext)
            .fetch_optional(pool)
            .await?;
        if let Some(old) = old {
            for t in ["laps", "activity_bests"] {
                sqlx::query(&format!("DELETE FROM {t} WHERE activity_id = ?")).bind(old).execute(pool).await?;
            }
            sqlx::query("DELETE FROM activity_streams WHERE activity_id = ?").bind(old).execute(pool).await?;
            sqlx::query("DELETE FROM activities WHERE id = ?").bind(old).execute(pool).await?;
        }
    }
    let id = match (a.source, a.external_id.as_deref().and_then(|e| e.parse::<i64>().ok())) {
        ("strava", Some(strava_id)) if strava_id > 0 => strava_id,
        _ => next_negative_id(pool).await?,
    };

    sqlx::query(
        r#"
        INSERT INTO activities
            (id, started_at, sport_type, duration_s, distance_m, elevation_gain_m,
             avg_power_w, normalized_power_w, avg_hr, max_hr, title, description, source, external_id)
        VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
        "#,
    )
    .bind(id)
    .bind(&a.started_at)
    .bind(&a.sport_type)
    .bind(a.duration_s)
    .bind(a.distance_m)
    .bind(a.elevation_gain_m)
    .bind(a.avg_power_w)
    .bind(a.normalized_power_w)
    .bind(a.avg_hr)
    .bind(a.max_hr)
    .bind(&a.title)
    .bind(&a.description)
    .bind(a.source)
    .bind(&a.external_id)
    .execute(pool)
    .await?;

    let streams_json = a.streams.to_string();
    // '' (not NULL) when there's no GPS data.
    let route_thumb_svg = crate::route_thumb::compute(&streams_json).unwrap_or_default();
    let route_points = crate::route_thumb::compute_points(&streams_json).unwrap_or_default();
    sqlx::query(
        "INSERT INTO activity_streams (activity_id, streams_json) VALUES (?, ?) \
         ON CONFLICT (activity_id) DO UPDATE SET streams_json = excluded.streams_json",
    )
    .bind(id)
    .bind(&streams_json)
    .execute(pool)
    .await?;
    sqlx::query("UPDATE activities SET route_thumb_svg = ?, route_points_json = ? WHERE id = ?")
        .bind(route_thumb_svg)
        .bind(route_points)
        .bind(id)
        .execute(pool)
        .await?;
    crate::bests::store(pool, id, crate::metrics::is_run(&a.sport_type), &a.streams).await?;

    for lap in &a.laps {
        sqlx::query(
            "INSERT INTO laps (activity_id, lap_index, elapsed_time_s, distance_m, avg_power_w, avg_hr, max_hr) VALUES (?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(id)
        .bind(lap.lap_index)
        .bind(lap.elapsed_time_s)
        .bind(lap.distance_m)
        .bind(lap.avg_power_w)
        .bind(lap.avg_hr)
        .bind(lap.max_hr)
        .execute(pool)
        .await?;
    }
    Ok(id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::sqlite::SqlitePoolOptions;

    async fn pool() -> SqlitePool {
        let pool = SqlitePoolOptions::new().connect("sqlite::memory:").await.unwrap();
        crate::db::migrate(&pool).await.unwrap();
        sqlx::query("INSERT INTO settings (id, strava_client_id, strava_client_secret, anthropic_api_key, athlete_profile, bilan_system_prompt, digest_system_prompt) VALUES (1, '', '', '', '', '', '')")
            .execute(&pool)
            .await
            .unwrap();
        pool
    }

    #[test]
    fn order_keeps_known_plugins_once_and_adds_missing_ones() {
        let o = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(normalize_order(&o(&["garmin"])), o(&["garmin", "strava"]));
        assert_eq!(normalize_order(&o(&["x", "strava", "strava", "garmin"])), o(&["strava", "garmin"]));
        assert_eq!(normalize_order(&[]), o(&["strava", "garmin"]));
    }

    #[tokio::test]
    async fn plugins_are_off_until_enabled_and_follow_the_priority_order() {
        let pool = pool().await;
        let cfg = config(&pool).await;
        assert!(cfg.active().is_empty(), "no plugin by default");
        assert_eq!(cfg.tolerance_s, DEFAULT_TOLERANCE_S);
        let both = ["strava".to_string(), "garmin".to_string()];
        save_order_and_enabled(&pool, &["garmin".into(), "strava".into()], &both).await.unwrap();
        assert_eq!(config(&pool).await.active(), ["garmin", "strava"]);
        save_order_and_enabled(&pool, &["garmin".into(), "strava".into()], &["strava".to_string()]).await.unwrap();
        assert_eq!(config(&pool).await.active(), ["strava"]);
        // Enabled but without credentials: not queried.
        assert!(active_and_configured(&pool).await.is_empty());
        sqlx::query("UPDATE settings SET garmin_api_key = 'k'").execute(&pool).await.unwrap();
        save_order_and_enabled(&pool, &["garmin".into(), "strava".into()], &both).await.unwrap();
        assert_eq!(active_and_configured(&pool).await, ["garmin"]);
    }

    fn activity(source: &'static str, ext: &str, start: &str, duration_s: i64) -> NewActivity {
        NewActivity {
            source,
            external_id: Some(ext.to_string()),
            started_at: start.to_string(),
            sport_type: "Ride".into(),
            duration_s,
            distance_m: 1000.0,
            elevation_gain_m: 0.0,
            avg_power_w: None,
            normalized_power_w: None,
            avg_hr: None,
            max_hr: None,
            title: "t".into(),
            description: None,
            streams: serde_json::json!({"time": {"data": [0, 1]}}),
            laps: vec![],
        }
    }

    #[tokio::test]
    async fn overlap_is_measured_in_seconds_against_the_tolerance() {
        let pool = pool().await;
        // 2026-11-10 08:00:00Z for one hour.
        let start = epoch_of("2026-11-10T08:00:00Z").unwrap();
        let id = insert(&pool, &activity("strava", "500", "2026-11-10T08:00:00Z", 3600)).await.unwrap();
        assert_eq!(id, 500, "a Strava id stays the primary key");

        // Same ride seen by another source, a few seconds apart: overlap.
        assert_eq!(find_overlap(&pool, start + 4, 3590, 10).await, Some(500));
        // Right after: only 5 s shared, within the tolerance.
        assert_eq!(find_overlap(&pool, start + 3595, 1800, 10).await, None);
        // The same 5 s shared with a tolerance of 2 s counts as an overlap.
        assert_eq!(find_overlap(&pool, start + 3595, 1800, 2).await, Some(500));
        // Next day: nothing.
        assert_eq!(find_overlap(&pool, start + 86_400, 3600, 10).await, None);
    }

    #[tokio::test]
    async fn non_strava_activities_get_negative_ids_and_are_known_afterwards() {
        let pool = pool().await;
        let g = insert(&pool, &activity("garmin", "98765", "2026-11-11T08:00:00Z", 1800)).await.unwrap();
        assert!(g < 0);
        assert!(is_known(&pool, "garmin", "98765").await);
        assert!(!is_known(&pool, "garmin", "1").await);
        // Re-importing the same external activity replaces it instead of duplicating.
        let again = insert(&pool, &activity("garmin", "98765", "2026-11-11T08:00:00Z", 1800)).await.unwrap();
        let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM activities WHERE source = 'garmin'").fetch_one(&pool).await.unwrap();
        assert_eq!(n, 1);
        assert!(again < 0);

        let listed = Listed {
            external_id: "42".into(),
            title: "x".into(),
            started_at: "2026-11-11T08:00:00Z".into(),
            start_epoch: 0,
            duration_s: None,
            sport_type: "Ride".into(),
            raw: serde_json::Value::Null,
        };
        record_ignored(&pool, "strava", &listed, g).await.unwrap();
        assert!(is_ignored(&pool, "strava", "42").await);
        forget_ignored(&pool, "strava").await.unwrap();
        assert!(!is_ignored(&pool, "strava", "42").await);
    }
}
