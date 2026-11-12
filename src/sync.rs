use chrono::Utc;
use serde::Serialize;
use sqlx::SqlitePool;
use std::sync::Mutex;

/// What the running sync is doing right now, polled by the dashboard
/// (`GET /sync/status`) so a slow sync (e.g. on a NAS) isn't a blank wait.
#[derive(Clone, Serialize)]
pub struct Progress {
    pub running: bool,
    pub step: String,
    pub done: usize,
    pub total: usize,
    pub error: Option<String>,
    /// Set when some sources failed but others went through.
    pub warning: Option<String>,
}

static PROGRESS: Mutex<Progress> = Mutex::new(Progress {
    running: false,
    step: String::new(),
    done: 0,
    total: 0,
    error: None,
    warning: None,
});

pub fn progress() -> Progress {
    PROGRESS.lock().unwrap().clone()
}

fn set_step(step: impl Into<String>, done: usize, total: usize) {
    let mut p = PROGRESS.lock().unwrap();
    p.step = step.into();
    p.done = done;
    p.total = total;
}

/// Marks a sync as started; false if one is already running.
fn try_start() -> bool {
    let mut p = PROGRESS.lock().unwrap();
    if p.running {
        return false;
    }
    *p = Progress { running: true, step: "Démarrage".into(), done: 0, total: 0, error: None, warning: None };
    true
}

fn finish(result: &anyhow::Result<Option<String>>) {
    let mut p = PROGRESS.lock().unwrap();
    p.running = false;
    p.error = result.as_ref().err().map(|e| format!("{e:#}"));
    p.warning = result.as_ref().ok().cloned().flatten();
    p.step = if p.error.is_some() {
        "Échec".into()
    } else if p.warning.is_some() {
        "Terminé avec des erreurs".into()
    } else {
        "Terminé".into()
    };
}

/// Runs a sync in the background for the web UI; returns immediately.
/// No-op if one is already running.
pub fn spawn(pool: SqlitePool, full: bool) {
    if !try_start() {
        return;
    }
    tokio::spawn(async move {
        let result = run_inner(&pool, full).await;
        match &result {
            Err(err) => tracing::error!(?err, "sync failed"),
            Ok(Some(warning)) => tracing::warn!(warning, "sync finished with errors"),
            Ok(None) => {}
        }
        finish(&result);
    });
}

/// Synchronises every enabled and configured source (see `sources`), in
/// priority order, then runs deterministic metrics and segment matching.
/// An activity overlapping one already stored is ignored, so the first source
/// to bring a ride wins. LLM bilan generation is NOT triggered here: it's
/// on-demand from the activity detail page (`web::detail::regenerate_bilan`),
/// so the user can add optional context before the first analysis.
pub async fn run(pool: &SqlitePool) -> anyhow::Result<()> {
    run_inner(pool, false).await.map(|_| ())
}

/// Like `run`, but ignores each source's last-sync marker and the list of
/// ignored activities, walking the whole history for activities not stored yet
/// (e.g. runs, which were filtered out before running was supported). Rate
/// limits (a source's API quota) can cut a
/// big backfill short: the next call resumes with what's still missing.
pub async fn run_full(pool: &SqlitePool) -> anyhow::Result<()> {
    run_inner(pool, true).await.map(|_| ())
}

/// `Ok(Some(warning))`: finished, but some sources failed.
async fn run_inner(pool: &SqlitePool, full: bool) -> anyhow::Result<Option<String>> {
    set_step("Chargement de la configuration", 0, 0);
    let cfg = crate::sources::config(pool).await;
    let sources: Vec<String> = cfg.active().into_iter().map(String::from).collect();
    if sources.is_empty() {
        anyhow::bail!(
            "aucune source d'activités n'est activée : active un plugin dans Paramètres > Sources d'activités (l'import de fichiers .fit/.gpx reste disponible sur le tableau de bord)"
        );
    }

    let mut imported_ids: Vec<i64> = Vec::new();
    let mut failures: Vec<String> = Vec::new();
    let mut succeeded = 0;
    for id in &sources {
        match sync_source(pool, id, full, cfg.tolerance_s, &mut imported_ids).await {
            Ok(()) => succeeded += 1,
            Err(err) => {
                tracing::error!(?err, source = %id, "source sync failed");
                failures.push(format!("{} : {err:#}", crate::sources::label(id)));
            }
        }
    }
    if succeeded == 0 {
        anyhow::bail!("{}", failures.join(" ; "));
    }

    let total = imported_ids.len();
    set_step("Calcul des métriques (TSS, CTL/ATL, zones)", total, total);
    // Only the activities imported just now need their streams re-parsed;
    // FTP/HR setting changes trigger the full recompute themselves.
    crate::metrics::run_incremental(pool, &imported_ids).await?;
    // The segment replay re-parses every stored ride, so it only runs when
    // something new was imported (`import-segments` runs it itself too).
    if imported_ids.is_empty() {
        set_step("Détection des segments (rien de nouveau, ignorée)", total, total);
    } else {
        set_step("Détection des segments", total, total);
        crate::segments::run(pool).await?;
    }

    Ok((!failures.is_empty()).then(|| failures.join(" ; ")))
}

/// One source: list, skip what is known or overlaps, download and store the rest.
async fn sync_source(pool: &SqlitePool, id: &str, full: bool, tolerance_s: i64, imported: &mut Vec<i64>) -> anyhow::Result<()> {
    use crate::sources;
    let label = sources::label(id);
    set_step(format!("{label} : authentification"), 0, 0);
    let source = sources::connect(id, pool).await?;
    let result = import_from(pool, source.as_ref(), full, tolerance_s, imported).await;
    source.finish(pool).await?;
    result
}

async fn import_from(
    pool: &SqlitePool,
    source: &dyn crate::sources::ActivitySource,
    full: bool,
    tolerance_s: i64,
    imported: &mut Vec<i64>,
) -> anyhow::Result<()> {
    use crate::sources;
    let id = source.id();
    let label = sources::label(id);
    // Taken before listing: whatever starts during the run is picked up next time.
    let started = Utc::now().timestamp();
    let after = if full { 0 } else { sources::last_sync_epoch(pool, id).await };
    if full {
        sources::forget_ignored(pool, id).await?;
    }

    set_step(format!("{label} : récupération de la liste des activités"), 0, 0);
    let listed = source.list(after).await?;

    let total = listed.len();
    for (i, l) in listed.iter().enumerate() {
        let step = format!("{label} : activité {}/{} « {} »", i + 1, total, l.title);
        if sources::is_known(pool, id, &l.external_id).await || sources::is_ignored(pool, id, &l.external_id).await {
            continue;
        }
        // Duration known from the listing: no need to download an activity that
        // an earlier (higher-priority) source already brought.
        if let Some(d) = l.duration_s
            && let Some(other) = sources::find_overlap(pool, l.start_epoch, d, tolerance_s).await
        {
            sources::record_ignored(pool, id, l, other).await?;
            continue;
        }
        set_step(format!("{step} : téléchargement"), i, total);
        let Some(activity) = source.fetch(l).await? else { continue };
        if l.duration_s.is_none()
            && let Some(other) = sources::find_overlap(pool, l.start_epoch, activity.duration_s, tolerance_s).await
        {
            sources::record_ignored(pool, id, l, other).await?;
            continue;
        }
        set_step(format!("{step} : enregistrement"), i, total);
        imported.push(sources::insert(pool, &activity).await?);
    }

    set_step("Enregistrement de l'état de synchronisation", total, total);
    sources::set_last_sync_epoch(pool, id, started).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sources::{ActivitySource, Listed, NewActivity};
    use sqlx::sqlite::SqlitePoolOptions;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// A plugin with a fixed listing; counts downloads.
    struct Fake {
        listed: Vec<Listed>,
        downloads: AtomicUsize,
    }

    #[async_trait::async_trait]
    impl ActivitySource for Fake {
        fn id(&self) -> &str {
            "garmin"
        }
        async fn list(&self, _after: i64) -> anyhow::Result<Vec<Listed>> {
            Ok(self.listed.clone())
        }
        async fn fetch(&self, l: &Listed) -> anyhow::Result<Option<NewActivity>> {
            self.downloads.fetch_add(1, Ordering::SeqCst);
            Ok(Some(NewActivity {
                source: "garmin".to_string(),
                external_id: Some(l.external_id.clone()),
                started_at: l.started_at.clone(),
                sport_type: "Ride".into(),
                duration_s: l.duration_s.unwrap_or(3600),
                distance_m: 1000.0,
                elevation_gain_m: 0.0,
                avg_power_w: None,
                normalized_power_w: None,
                avg_hr: None,
                max_hr: None,
                title: l.title.clone(),
                description: None,
                streams: serde_json::json!({"time": {"data": [0, 1]}}),
                laps: vec![],
            }))
        }
    }

    fn listed(id: &str, start: &str, duration_s: Option<i64>) -> Listed {
        Listed {
            external_id: id.into(),
            title: format!("ride {id}"),
            started_at: start.into(),
            start_epoch: crate::sources::epoch_of(start).unwrap(),
            duration_s,
            sport_type: "Ride".into(),
        }
    }

    #[tokio::test]
    async fn an_overlapping_activity_is_ignored_once_and_never_downloaded_again() {
        let pool = SqlitePoolOptions::new().connect("sqlite::memory:").await.unwrap();
        crate::db::migrate(&pool).await.unwrap();
        // A ride already stored (say from an earlier file import), 08:00-09:00.
        sqlx::query("INSERT INTO activities (id, started_at, duration_s, distance_m, elevation_gain_m, source) VALUES (-1, '2026-11-10T08:00:00Z', 3600, 1, 0, 'file')")
            .execute(&pool)
            .await
            .unwrap();
        let fake = Fake {
            listed: vec![
                // Same ride, 4 s later: ignored before any download (duration known).
                listed("dup", "2026-11-10T08:00:04Z", Some(3590)),
                // Same ride, duration unknown from the listing: downloaded, then ignored.
                listed("dup2", "2026-11-10T08:00:02Z", None),
                // Next ride, starting as the first one ends: kept.
                listed("next", "2026-11-10T09:00:00Z", Some(1800)),
            ],
            downloads: AtomicUsize::new(0),
        };
        let mut imported = Vec::new();
        import_from(&pool, &fake, false, 10, &mut imported).await.unwrap();
        assert_eq!(imported.len(), 1, "only the non-overlapping ride");
        assert_eq!(fake.downloads.load(Ordering::SeqCst), 2, "dup was skipped without a download");
        for id in ["dup", "dup2"] {
            assert!(crate::sources::is_ignored(&pool, "garmin", id).await, "{id}");
        }
        assert!(crate::sources::is_known(&pool, "garmin", "next").await);

        // A second run changes nothing and downloads nothing.
        let mut again = Vec::new();
        import_from(&pool, &fake, false, 10, &mut again).await.unwrap();
        assert!(again.is_empty());
        assert_eq!(fake.downloads.load(Ordering::SeqCst), 2);

        // A full sync reconsiders the ignored ones (here still overlapping: ignored again).
        import_from(&pool, &fake, true, 10, &mut again).await.unwrap();
        assert!(again.is_empty());
        assert_eq!(fake.downloads.load(Ordering::SeqCst), 3, "dup2 is downloaded again, dup is not");

        // A bigger tolerance lets the 4 s overlap through on a full sync.
        import_from(&pool, &fake, true, 7200, &mut again).await.unwrap();
        assert_eq!(again.len(), 2, "with a huge tolerance nothing overlaps");
    }

    /// A real plugin service on a local port, driven through the HTTP adapter.
    struct Service;

    #[async_trait::async_trait]
    impl cyclai_plugin_sdk::Plugin for Service {
        async fn manifest(&self) -> cyclai_plugin_sdk::Manifest {
            cyclai_plugin_sdk::Manifest {
                protocol: cyclai_plugin_sdk::PROTOCOL_VERSION,
                id: "fake".into(),
                label: "Fake source".into(),
                version: "1".into(),
                sports: vec!["Ride".into()],
                configured: true,
                message: None,
                activity_url: Some("https://example.com/a/{id}".into()),
                activity_url_label: Some("voir chez Fake".into()),
            }
        }
        async fn list(&self, _after: i64) -> Result<Vec<cyclai_plugin_sdk::ListedActivity>, cyclai_plugin_sdk::PluginError> {
            let item = |id: &str, start: &str, epoch: i64| cyclai_plugin_sdk::ListedActivity {
                external_id: id.into(),
                title: format!("ride {id}"),
                started_at: start.into(),
                start_epoch: epoch,
                duration_s: Some(1800),
                sport_type: "Ride".into(),
            };
            let base = crate::sources::epoch_of("2026-11-10T08:00:00Z").unwrap();
            Ok(vec![item("dup", "2026-11-10T08:00:03Z", base + 3), item("new", "2026-11-11T08:00:00Z", base + 86_400)])
        }
        async fn fetch(&self, id: &str) -> Result<cyclai_plugin_sdk::Fetched, cyclai_plugin_sdk::PluginError> {
            Ok(cyclai_plugin_sdk::Fetched::Activity(cyclai_plugin_sdk::Activity {
                started_at: "2026-11-11T08:00:00Z".into(),
                sport_type: "Ride".into(),
                duration_s: 1800,
                distance_m: 10_000.0,
                elevation_gain_m: 50.0,
                avg_power_w: Some(150.0),
                normalized_power_w: Some(155.0),
                avg_hr: None,
                max_hr: None,
                title: format!("ride {id}"),
                description: None,
                streams: serde_json::json!({"time": {"data": [0, 1, 2]}, "watts": {"data": [150, 150, 150]}}),
                laps: vec![],
            }))
        }
    }

    #[tokio::test]
    async fn a_sync_goes_through_a_real_plugin_over_signed_http() {
        let _lock = crate::sources::registry::TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let secret = "an-integration-test-secret";
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let router = cyclai_plugin_sdk::app(std::sync::Arc::new(Service), secret);
        tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });

        let pool = SqlitePoolOptions::new().connect("sqlite::memory:").await.unwrap();
        crate::db::migrate(&pool).await.unwrap();
        let config = crate::config::Config {
            database_url: "sqlite::memory:".into(),
            anthropic_api_key: String::new(),
            ftp_watts: 200,
            hr_max: 190,
            hr_rest: 50,
            athlete_profile: String::new(),
        };
        crate::db::seed_athlete_config(&pool, &config).await.unwrap();
        crate::db::seed_settings(&pool, &config).await.unwrap();
        crate::db::seed_sync_state(&pool, &config).await.unwrap();
        sqlx::query("INSERT INTO activities (id, started_at, duration_s, distance_m, elevation_gain_m, source) VALUES (-1, '2026-11-10T08:00:00Z', 3600, 1, 0, 'file')")
            .execute(&pool)
            .await
            .unwrap();

        crate::sources::registry::set(vec![crate::sources::registry::PluginEndpoint { id: "fake".into(), url: format!("http://{addr}"), secret: secret.into() }]);
        // Declared but off by default: nothing to sync.
        assert!(run(&pool).await.unwrap_err().to_string().contains("aucune source"));
        crate::sources::save_order_and_enabled(&pool, &["fake".to_string()], &["fake".to_string()]).await.unwrap();

        run(&pool).await.unwrap();
        let rows: Vec<(String, String, Option<String>)> =
            sqlx::query_as("SELECT source, title, external_id FROM activities WHERE source = 'fake'").fetch_all(&pool).await.unwrap();
        assert_eq!(rows, [("fake".to_string(), "ride new".to_string(), Some("new".to_string()))], "the duplicate of the file import was ignored");
        assert!(crate::sources::is_ignored(&pool, "fake", "dup").await);
        let links = crate::sources::link_templates(&pool).await;
        assert_eq!(links["fake"].1, "voir chez Fake", "the manifest's link reached the database");

        // Nothing new the second time.
        run(&pool).await.unwrap();
        let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM activities").fetch_one(&pool).await.unwrap();
        assert_eq!(n, 2);

        // A wrong secret is an error naming the source, not a panic or a silent skip.
        crate::sources::registry::set(vec![crate::sources::registry::PluginEndpoint { id: "fake".into(), url: format!("http://{addr}"), secret: "another-secret-entirely".into() }]);
        let err = run(&pool).await.unwrap_err().to_string();
        assert!(err.contains("Fake source") && err.contains("401"), "{err}");
        crate::sources::registry::set(vec![]);
    }
}
