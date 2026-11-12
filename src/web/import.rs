use super::AppState;
use axum::extract::{Multipart, State};
use axum::response::Redirect;
use sqlx::SqlitePool;

/// Manual counterpart to `sync::run` for a single ride uploaded as a `.fit`
/// or `.gpx` file, without depending on any external API. Parses the file into
/// the same shape a plugin sync produces (`importer::ParsedActivity`) and
/// writes it through the same tables (`sources::insert`), so everything downstream — metrics, zones, route thumbnail,
/// bilan generation — works identically regardless of the activity's
/// origin.
pub async fn import_activity(State(state): State<AppState>, mut multipart: Multipart) -> Redirect {
    let mut file_name = None;
    let mut bytes = None;

    while let Ok(Some(field)) = multipart.next_field().await {
        if field.name() == Some("file") {
            file_name = field.file_name().map(|s| s.to_string());
            bytes = field.bytes().await.ok();
        }
    }

    let (Some(file_name), Some(bytes)) = (file_name, bytes) else {
        tracing::warn!("activity import: no file in upload");
        return Redirect::to("/");
    };

    let lower = file_name.to_lowercase();
    let parsed = if lower.ends_with(".fit") {
        crate::importer::parse_fit(&bytes)
    } else if lower.ends_with(".gpx") {
        crate::importer::parse_gpx(&bytes)
    } else {
        tracing::warn!(file_name, "activity import: unsupported file extension (expected .fit or .gpx)");
        return Redirect::to("/");
    };

    let parsed = match parsed {
        Ok(p) => p,
        Err(err) => {
            tracing::error!(?err, file_name, "activity import: failed to parse file");
            return Redirect::to("/");
        }
    };

    let title = file_name
        .rsplit_once('.')
        .map(|(stem, _)| stem)
        .unwrap_or(&file_name)
        .to_string();

    match insert_imported_activity(&state.pool, &title, parsed).await {
        Ok(id) => {
            if let Err(err) = crate::metrics::run(&state.pool).await {
                tracing::error!(?err, activity_id = id, "activity import: metrics recompute failed");
            }
            Redirect::to(&format!("/activity/{id}"))
        }
        Err(err) => {
            tracing::error!(?err, file_name, "activity import: failed to store activity");
            Redirect::to("/")
        }
    }
}

/// Manual imports are the `file` source: never ignored for overlapping another
/// activity (the user chose to import it), but later syncs will skip whatever
/// overlaps it.
async fn insert_imported_activity(
    pool: &SqlitePool,
    title: &str,
    parsed: crate::importer::ParsedActivity,
) -> anyhow::Result<i64> {
    crate::sources::insert(
        pool,
        &crate::sources::NewActivity {
            source: "file".to_string(),
            external_id: None,
            started_at: parsed.started_at,
            sport_type: "Ride".to_string(),
            duration_s: parsed.duration_s,
            distance_m: parsed.distance_m,
            elevation_gain_m: parsed.elevation_gain_m,
            avg_power_w: parsed.avg_power_w,
            normalized_power_w: parsed.normalized_power_w,
            avg_hr: parsed.avg_hr,
            max_hr: parsed.max_hr,
            title: title.to_string(),
            description: None,
            streams: parsed.streams_json,
            laps: parsed.laps,
        },
    )
    .await
}
