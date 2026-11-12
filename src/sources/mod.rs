//! Activity sources. CyclAI works on its own with manual `.fit`/`.gpx` imports
//! (`source = 'file'`). Other sources (Garmin Connect, ...) are **plugins**: small
//! HTTP services, typically their own container, declared in the environment
//! (see `registry`) and spoken to through `cyclai-plugin-sdk`. They are off
//! until enabled in /settings. A synchronisation walks every enabled plugin in
//! priority order; an activity that overlaps one already stored (more than the
//! tolerance) is ignored, so the first source to bring a ride wins.

mod external;
pub mod registry;

use crate::importer::ParsedLap;
use async_trait::async_trait;
use serde_json::Value as Json;
use sqlx::SqlitePool;
use std::collections::HashMap;
use std::sync::RwLock;

pub use external::manifest_of;

/// Display names learned from the plugins' manifests (also kept in the
/// database, so they survive a plugin being down).
static LABELS: RwLock<Option<HashMap<String, String>>> = RwLock::new(None);

pub fn label(source: &str) -> String {
    if source == "file" {
        return "Fichier .fit/.gpx".to_string();
    }
    if let Some(l) = LABELS.read().unwrap().as_ref().and_then(|m| m.get(source)) {
        return l.clone();
    }
    let mut chars = source.chars();
    chars.next().map(|c| c.to_uppercase().collect::<String>() + chars.as_str()).unwrap_or_default()
}

/// One activity as listed by a source, before anything is downloaded.
#[derive(Clone, Debug)]
pub struct Listed {
    pub external_id: String,
    pub title: String,
    /// RFC 3339, UTC.
    pub started_at: String,
    pub start_epoch: i64,
    /// Known from the listing for some sources: lets the sync skip an activity
    /// that overlaps another before downloading it.
    pub duration_s: Option<i64>,
    pub sport_type: String,
}

/// A complete activity, ready to be stored.
pub struct NewActivity {
    pub source: String,
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
    fn id(&self) -> &str;
    /// Activities started after `after_epoch` (0 = the whole history), limited
    /// to the sports CyclAI handles.
    async fn list(&self, after_epoch: i64) -> anyhow::Result<Vec<Listed>>;
    /// Downloads one activity; `None` when the source has nothing to give for it.
    async fn fetch(&self, listed: &Listed) -> anyhow::Result<Option<NewActivity>>;
    /// Called once the run is over.
    async fn finish(&self, _pool: &SqlitePool) -> anyhow::Result<()> {
        Ok(())
    }
}

/// Connects a plugin (must be declared in the registry and configured).
pub async fn connect(id: &str, pool: &SqlitePool) -> anyhow::Result<Box<dyn ActivitySource>> {
    if let Ok(m) = manifest_of(id).await {
        remember_manifest(pool, &m).await;
    }
    external::connect(id).await
}

// ───────────────────────── Plugin information ─────────────────────────

/// Keeps what a plugin says about itself that the rest of the app needs: its
/// display name and the link to one of its activities. A link template must be
/// `https` and contain `{id}`: it ends up in a page, so it is checked here.
pub async fn remember_manifest(pool: &SqlitePool, m: &cyclai_plugin_sdk::Manifest) {
    let label: String = m.label.chars().filter(|c| !c.is_control()).take(40).collect();
    let label = if label.trim().is_empty() { m.id.clone() } else { label };
    let url = m.activity_url.clone().filter(|u| u.starts_with("https://") && u.contains("{id}") && u.len() <= 300);
    let url_label: Option<String> = m.activity_url_label.as_ref().map(|l| l.chars().filter(|c| !c.is_control()).take(60).collect());
    LABELS.write().unwrap().get_or_insert_with(HashMap::new).insert(m.id.clone(), label.clone());
    let saved = sqlx::query(
        "INSERT INTO plugin_info (source, label, activity_url, activity_url_label, updated_at) VALUES (?, ?, ?, ?, datetime('now')) \
         ON CONFLICT (source) DO UPDATE SET label = excluded.label, activity_url = excluded.activity_url, \
         activity_url_label = excluded.activity_url_label, updated_at = excluded.updated_at",
    )
    .bind(&m.id)
    .bind(label)
    .bind(url)
    .bind(url_label)
    .execute(pool)
    .await;
    if let Err(err) = saved {
        tracing::warn!(?err, source = %m.id, "could not store plugin information");
    }
}

/// Loads the stored display names (at startup, before any plugin answers).
pub async fn load_labels(pool: &SqlitePool) {
    let rows: Vec<(String, String)> = sqlx::query_as("SELECT source, label FROM plugin_info").fetch_all(pool).await.unwrap_or_default();
    LABELS.write().unwrap().get_or_insert_with(HashMap::new).extend(rows);
}

/// Asks every declared plugin for its manifest (at startup, in the background),
/// so links and names are known without waiting for a first synchronisation.
pub async fn refresh_manifests(pool: &SqlitePool) {
    for id in registry::ids() {
        if let Ok(m) = manifest_of(&id).await {
            remember_manifest(pool, &m).await;
        }
    }
}

/// What every declared plugin answers to a manifest request right now, asked
/// concurrently (a plugin that does not answer in a few seconds is "down").
pub async fn statuses(pool: &SqlitePool) -> HashMap<String, Result<cyclai_plugin_sdk::Manifest, String>> {
    let mut tasks = tokio::task::JoinSet::new();
    for id in registry::ids() {
        tasks.spawn(async move {
            let answer = match tokio::time::timeout(std::time::Duration::from_secs(4), manifest_of(&id)).await {
                Ok(r) => r,
                Err(_) => Err("ne répond pas".to_string()),
            };
            (id, answer)
        });
    }
    let mut out = HashMap::new();
    while let Some(Ok((id, answer))) = tasks.join_next().await {
        if let Ok(m) = &answer {
            remember_manifest(pool, m).await;
        }
        out.insert(id, answer);
    }
    out
}

/// `source -> (link template, link label)`.
pub async fn link_templates(pool: &SqlitePool) -> HashMap<String, (String, String)> {
    let rows: Vec<(String, String, Option<String>)> =
        sqlx::query_as("SELECT source, activity_url, activity_url_label FROM plugin_info WHERE activity_url IS NOT NULL")
            .fetch_all(pool)
            .await
            .unwrap_or_default();
    rows.into_iter()
        .map(|(source, url, label)| {
            let label = label.filter(|l| !l.trim().is_empty()).unwrap_or_else(|| format!("voir sur {}", label_of(&source)));
            (source, (url, label))
        })
        .collect()
}

fn label_of(source: &str) -> String {
    label(source)
}

/// Percent-encodes what is not safe in a URL path.
fn encode_path(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => (b as char).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}

/// Link to an activity on its source's own site, if the plugin gave a template.
pub fn activity_link(templates: &HashMap<String, (String, String)>, source: &str, external_id: Option<&str>) -> Option<(String, String)> {
    let (template, label) = templates.get(source)?;
    Some((template.replace("{id}", &encode_path(external_id?)), label.clone()))
}

// ───────────────────────── Configuration ─────────────────────────

#[derive(Clone, Debug, PartialEq)]
pub struct SourceConfig {
    /// Every declared plugin once, highest priority first.
    pub order: Vec<String>,
    pub enabled: Vec<String>,
    pub tolerance_s: i64,
}

impl SourceConfig {
    /// Enabled and declared plugins, highest priority first.
    pub fn active(&self) -> Vec<&str> {
        self.order.iter().filter(|s| self.enabled.contains(s)).map(String::as_str).collect()
    }
}

/// `order` restricted to `known` ids, once each, with the missing ones appended.
pub fn normalize_order(order: &[String], known: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for s in order {
        if known.contains(s) && !out.contains(s) {
            out.push(s.clone());
        }
    }
    for k in known {
        if !out.contains(k) {
            out.push(k.clone());
        }
    }
    out
}

pub const DEFAULT_TOLERANCE_S: i64 = 10;
pub const MAX_TOLERANCE_S: i64 = 3600;

async fn saved(pool: &SqlitePool) -> (Vec<String>, Vec<String>, i64) {
    let row: Option<(String, String, i64)> = sqlx::query_as("SELECT source_order, enabled_sources, overlap_tolerance_s FROM settings WHERE id = 1")
        .fetch_optional(pool)
        .await
        .ok()
        .flatten();
    let (order, enabled, tolerance_s) = row.unwrap_or_else(|| ("[]".into(), "[]".into(), DEFAULT_TOLERANCE_S));
    (serde_json::from_str(&order).unwrap_or_default(), serde_json::from_str(&enabled).unwrap_or_default(), tolerance_s.clamp(0, MAX_TOLERANCE_S))
}

pub async fn config(pool: &SqlitePool) -> SourceConfig {
    let (order, enabled, tolerance_s) = saved(pool).await;
    let known = registry::ids();
    SourceConfig {
        order: normalize_order(&order, &known),
        enabled: enabled.into_iter().filter(|s| known.contains(s)).collect(),
        tolerance_s,
    }
}

/// Saves the priority order and the enabled plugins. A plugin that is no longer
/// declared keeps its enabled state (and is listed last), so removing it from the
/// compose for a while does not lose the choice.
pub async fn save_order_and_enabled(pool: &SqlitePool, order: &[String], enabled: &[String]) -> anyhow::Result<()> {
    let (old_order, old_enabled, _) = saved(pool).await;
    let known = registry::ids();
    let mut order = normalize_order(order, &known);
    let extra: Vec<String> = old_order.into_iter().filter(|s| !known.contains(s) && !order.contains(s)).collect();
    order.extend(extra);
    let mut enabled: Vec<String> = order.iter().filter(|s| known.contains(s) && enabled.contains(s)).cloned().collect();
    enabled.extend(old_enabled.into_iter().filter(|s| !known.contains(s)));
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

/// Id of a new activity. Ids are negative from now on; the positive ones are
/// activities imported before sources became plugins (their ids), kept as is.
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

/// Writes an activity with its streams, route, bests and laps. An earlier
/// interrupted import of the same activity is replaced.
pub async fn insert(pool: &SqlitePool, a: &NewActivity) -> anyhow::Result<i64> {
    if let Some(ext) = &a.external_id {
        let old: Option<i64> = sqlx::query_scalar("SELECT id FROM activities WHERE source = ? AND external_id = ?")
            .bind(&a.source)
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
    let id = next_negative_id(pool).await?;

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
    .bind(&a.source)
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

    fn declare(ids: &[&str]) {
        registry::set(ids.iter().map(|id| registry::PluginEndpoint { id: id.to_string(), url: format!("http://{id}:8080"), secret: "x".repeat(20) }).collect());
    }

    async fn pool() -> SqlitePool {
        let pool = SqlitePoolOptions::new().connect("sqlite::memory:").await.unwrap();
        crate::db::migrate(&pool).await.unwrap();
        sqlx::query("INSERT INTO settings (id, anthropic_api_key, athlete_profile, bilan_system_prompt, digest_system_prompt) VALUES (1, '', '', '', '')")
            .execute(&pool)
            .await
            .unwrap();
        pool
    }

    #[test]
    fn order_keeps_known_plugins_once_and_adds_missing_ones() {
        let o = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let known = o(&["alpha", "garmin"]);
        assert_eq!(normalize_order(&o(&["garmin"]), &known), o(&["garmin", "alpha"]));
        assert_eq!(normalize_order(&o(&["x", "alpha", "alpha", "garmin"]), &known), o(&["alpha", "garmin"]));
        assert_eq!(normalize_order(&[], &known), o(&["alpha", "garmin"]));
        assert!(normalize_order(&o(&["alpha"]), &[]).is_empty(), "no plugin declared, none listed");
    }

    #[tokio::test]
    async fn plugins_are_off_until_enabled_and_follow_the_priority_order() {
        let _lock = registry::TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        declare(&["alpha", "garmin"]);
        let pool = pool().await;
        let cfg = config(&pool).await;
        assert!(cfg.active().is_empty(), "no plugin by default");
        assert_eq!(cfg.tolerance_s, DEFAULT_TOLERANCE_S);
        let both = ["alpha".to_string(), "garmin".to_string()];
        save_order_and_enabled(&pool, &["garmin".into(), "alpha".into()], &both).await.unwrap();
        assert_eq!(config(&pool).await.active(), ["garmin", "alpha"]);
        save_order_and_enabled(&pool, &["garmin".into(), "alpha".into()], &["alpha".to_string()]).await.unwrap();
        assert_eq!(config(&pool).await.active(), ["alpha"]);
    }

    #[tokio::test]
    async fn a_plugin_removed_from_the_compose_keeps_its_choice() {
        let _lock = registry::TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let pool = pool().await;
        declare(&["alpha", "garmin"]);
        save_order_and_enabled(&pool, &["alpha".into(), "garmin".into()], &["alpha".to_string(), "garmin".to_string()]).await.unwrap();
        // Alpha is taken out of the compose; the user saves the settings meanwhile.
        declare(&["garmin"]);
        assert_eq!(config(&pool).await.active(), ["garmin"]);
        save_order_and_enabled(&pool, &["garmin".into()], &["garmin".to_string()]).await.unwrap();
        // Back in the compose: still enabled (it now ranks after the plugins saved meanwhile).
        declare(&["alpha", "garmin"]);
        assert_eq!(config(&pool).await.active(), ["garmin", "alpha"]);
    }

    #[test]
    fn links_come_from_the_plugin_template_and_escape_the_id() {
        let mut t = HashMap::new();
        t.insert("alpha".to_string(), ("https://example.com/activities/{id}".to_string(), "voir sur Alpha".to_string()));
        assert_eq!(
            activity_link(&t, "alpha", Some("12 3")),
            Some(("https://example.com/activities/12%203".to_string(), "voir sur Alpha".to_string()))
        );
        assert_eq!(activity_link(&t, "garmin", Some("1")), None, "no template for this source");
        assert_eq!(activity_link(&t, "alpha", None), None, "no external id (a file import)");
    }

    #[tokio::test]
    async fn only_https_link_templates_with_an_id_are_kept() {
        let pool = pool().await;
        let manifest = |id: &str, url: Option<&str>| cyclai_plugin_sdk::Manifest {
            protocol: cyclai_plugin_sdk::PROTOCOL_VERSION,
            id: id.to_string(),
            label: format!("{id}\u{7}"),
            version: "1".into(),
            sports: vec![],
            configured: true,
            message: None,
            activity_url: url.map(str::to_string),
            activity_url_label: None,
        };
        remember_manifest(&pool, &manifest("good", Some("https://x.example/a/{id}"))).await;
        remember_manifest(&pool, &manifest("script", Some("javascript:alert({id})"))).await;
        remember_manifest(&pool, &manifest("plain", Some("http://x.example/a/{id}"))).await;
        remember_manifest(&pool, &manifest("noid", Some("https://x.example/a"))).await;
        let t = link_templates(&pool).await;
        assert_eq!(t.keys().collect::<Vec<_>>(), ["good"]);
        assert_eq!(label("good"), "good", "control characters are dropped from names");
    }

    fn activity(source: &str, ext: &str, start: &str, duration_s: i64) -> NewActivity {
        NewActivity {
            source: source.to_string(),
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
        let id = insert(&pool, &activity("alpha", "500", "2026-11-10T08:00:00Z", 3600)).await.unwrap();
        assert!(id < 0);

        // Same ride seen by another source, a few seconds apart: overlap.
        assert_eq!(find_overlap(&pool, start + 4, 3590, 10).await, Some(id));
        // Right after: only 5 s shared, within the tolerance.
        assert_eq!(find_overlap(&pool, start + 3595, 1800, 10).await, None);
        // The same 5 s shared with a tolerance of 2 s counts as an overlap.
        assert_eq!(find_overlap(&pool, start + 3595, 1800, 2).await, Some(id));
        // Next day: nothing.
        assert_eq!(find_overlap(&pool, start + 86_400, 3600, 10).await, None);
    }

    #[tokio::test]
    async fn activities_get_negative_ids_and_are_known_afterwards() {
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
        };
        record_ignored(&pool, "alpha", &listed, g).await.unwrap();
        assert!(is_ignored(&pool, "alpha", "42").await);
        forget_ignored(&pool, "alpha").await.unwrap();
        assert!(!is_ignored(&pool, "alpha", "42").await);
    }
}
