//! Strava plugin: lists and downloads activities through the Strava API.

use super::{epoch_of, ActivitySource, Listed, NewActivity};
use crate::importer::ParsedLap;
use crate::strava::models::SummaryActivity;
use crate::strava::StravaClient;
use async_trait::async_trait;
use sqlx::SqlitePool;

pub struct StravaSource {
    client: StravaClient,
}

pub async fn connect(pool: &SqlitePool) -> anyhow::Result<Option<Box<dyn ActivitySource>>> {
    if !super::is_configured(pool, "strava").await {
        return Ok(None);
    }
    let settings = crate::settings::fetch(pool).await?;
    let refresh_token: Option<String> = sqlx::query_scalar("SELECT strava_refresh_token FROM sync_state WHERE id = 1")
        .fetch_optional(pool)
        .await?
        .flatten();
    let client =
        StravaClient::authenticate(&settings.strava_client_id, &settings.strava_client_secret, &refresh_token.unwrap_or_default()).await?;
    Ok(Some(Box::new(StravaSource { client })))
}

#[async_trait]
impl ActivitySource for StravaSource {
    fn id(&self) -> &'static str {
        "strava"
    }

    async fn list(&self, after_epoch: i64) -> anyhow::Result<Vec<Listed>> {
        let fetched = self.client.list_activities_after(after_epoch).await?;
        tracing::info!(count = fetched.len(), "activities fetched from Strava");
        // CyclAI covers road and indoor cycling and running: MTB/gravel, weight
        // training etc. from the same account never enter the pipeline. sport_type
        // (not the legacy `type`) is what distinguishes GravelRide from Ride.
        Ok(fetched
            .into_iter()
            .filter(|a| crate::metrics::is_supported_sport(&a.sport_type))
            .filter_map(|a| {
                Some(Listed {
                    external_id: a.id.to_string(),
                    title: a.name.clone(),
                    started_at: a.start_date.clone(),
                    start_epoch: epoch_of(&a.start_date)?,
                    duration_s: Some(a.moving_time),
                    sport_type: a.sport_type.clone(),
                    raw: serde_json::to_value(&a).ok()?,
                })
            })
            .collect())
    }

    async fn fetch(&self, listed: &Listed) -> anyhow::Result<Option<NewActivity>> {
        let a: SummaryActivity = serde_json::from_value(listed.raw.clone())?;
        // Power-based metrics (NP/TSS/IF/zones) are cycling-only and relative to
        // the cycling FTP: a run's power (e.g. a foot pod) is ignored.
        let is_ride = crate::metrics::is_ride(&a.sport_type);
        let device_power = is_ride && a.device_watts.unwrap_or(false);
        // Only the title is free with the list; the description needs its own call.
        let description = self.client.get_activity_description(a.id).await?;
        let streams = self.client.get_streams(a.id).await?;
        let laps = self
            .client
            .get_laps(a.id)
            .await?
            .into_iter()
            .map(|l| ParsedLap {
                lap_index: l.lap_index,
                elapsed_time_s: l.elapsed_time,
                distance_m: l.distance,
                avg_power_w: l.average_watts.filter(|_| is_ride && l.device_watts.unwrap_or(false)),
                avg_hr: l.average_heartrate,
                max_hr: l.max_heartrate,
            })
            .collect();
        Ok(Some(NewActivity {
            source: "strava",
            external_id: Some(a.id.to_string()),
            started_at: a.start_date,
            sport_type: a.sport_type,
            duration_s: a.moving_time,
            distance_m: a.distance,
            elevation_gain_m: a.total_elevation_gain,
            avg_power_w: a.average_watts.filter(|_| device_power),
            normalized_power_w: a.weighted_average_watts.filter(|_| device_power),
            avg_hr: a.average_heartrate,
            max_hr: a.max_heartrate,
            title: a.name,
            description,
            streams,
            laps,
        }))
    }

    /// Strava rotates the refresh token on every use.
    async fn finish(&self, pool: &SqlitePool) -> anyhow::Result<()> {
        sqlx::query(
            "INSERT INTO sync_state (id, last_sync_epoch, strava_refresh_token) VALUES (1, 0, ?) \
             ON CONFLICT (id) DO UPDATE SET strava_refresh_token = excluded.strava_refresh_token",
        )
        .bind(&self.client.refresh_token)
        .execute(pool)
        .await?;
        Ok(())
    }
}
