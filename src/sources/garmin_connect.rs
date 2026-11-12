//! Garmin Connect plugin, through Garmin's Personal API: the athlete creates a
//! personal API key in Garmin Connect (it expires after 30 to 365 days), which
//! is exchanged for a 24 h bearer token. Activities are listed with the search
//! endpoint, and each one is downloaded as its original `.fit` file, parsed
//! with the same importer as a manual upload.
//!
//! Garmin has no "after this date" filter that is cheap on big accounts, so the
//! search runs over date windows that are split in two whenever a page comes
//! back full or Garmin answers "too large".

use super::{ActivitySource, Listed, NewActivity};
use async_trait::async_trait;
use chrono::{Duration, NaiveDate, NaiveDateTime, Utc};
use serde::Deserialize;
use sqlx::SqlitePool;
use std::io::Read;

const API_BASE: &str = "https://personal.connectapi.garmin.com";
const TOKEN_URL: &str = "https://personal.connectapi.garmin.com/personalapitoken-service/personal-api/personal-api-key/token";
const PAGE_LIMIT: usize = 20;
/// A full-history walk stops after about two years without any activity.
const EMPTY_WINDOWS_BEFORE_STOP: u32 = 24;
const WINDOW_DAYS: i64 = 30;
const MAX_SPLIT_STEPS: u32 = 500;
const MAX_DAY_OFFSET: usize = 400;

pub struct GarminSource {
    http: reqwest::Client,
    token: String,
}

pub async fn connect(pool: &SqlitePool) -> anyhow::Result<Option<Box<dyn ActivitySource>>> {
    let key = super::config(pool).await.garmin_api_key;
    if key.trim().is_empty() {
        return Ok(None);
    }
    Ok(Some(Box::new(GarminSource::authenticate(key.trim()).await?)))
}

#[derive(Deserialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
struct RawActivity {
    activity_id: i64,
    activity_name: Option<String>,
    #[serde(rename = "startTimeGMT")]
    start_time_gmt: Option<String>,
    activity_type: Option<RawType>,
    #[serde(default)]
    manual_activity: bool,
    duration: Option<f64>,
    elapsed_duration: Option<f64>,
}

#[derive(Deserialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
struct RawType {
    type_key: String,
}

enum PageError {
    TooLarge,
    Other(anyhow::Error),
}

/// CyclAI's sports from Garmin's activity type keys; everything else
/// (gravel, mountain biking, swimming...) is left out.
pub fn map_sport(type_key: &str) -> Option<&'static str> {
    match type_key {
        "cycling" | "road_biking" => Some("Ride"),
        "indoor_cycling" | "virtual_ride" => Some("VirtualRide"),
        "running" | "treadmill_running" | "track_running" => Some("Run"),
        "trail_running" | "ultra_run" => Some("TrailRun"),
        _ => None,
    }
}

/// Garmin's `startTimeGMT` ("2026-11-10 08:00:00" or ISO, optionally with
/// fractional seconds), as UTC epoch seconds.
fn parse_gmt(s: &str) -> Option<i64> {
    ["%Y-%m-%d %H:%M:%S", "%Y-%m-%dT%H:%M:%S", "%Y-%m-%d %H:%M:%S%.f", "%Y-%m-%dT%H:%M:%S%.f"]
        .iter()
        .find_map(|f| NaiveDateTime::parse_from_str(s.trim().trim_end_matches('Z'), f).ok())
        .map(|d| d.and_utc().timestamp())
}

fn rfc3339(epoch: i64) -> String {
    chrono::DateTime::from_timestamp(epoch, 0).unwrap_or_default().format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

impl GarminSource {
    async fn authenticate(api_key: &str) -> anyhow::Result<Self> {
        let http = reqwest::Client::builder().timeout(std::time::Duration::from_secs(60)).build()?;
        let res = http
            .post(TOKEN_URL)
            .header("Authorization", format!("ApiKey {api_key}"))
            .header("Accept", "*/*")
            .send()
            .await?;
        let status = res.status();
        if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
            anyhow::bail!("clé API Garmin refusée (expirée ou révoquée)");
        }
        if !status.is_success() {
            anyhow::bail!("authentification Garmin impossible (HTTP {status})");
        }
        // The body is a raw JWT, not JSON.
        let body = res.text().await?;
        let token = body.trim().trim_matches('"').to_string();
        if token.split('.').count() != 3 {
            anyhow::bail!("réponse d'authentification Garmin inattendue");
        }
        Ok(Self { http, token })
    }

    /// GET with a bearer token, waiting out Garmin's rate limit (HTTP 429).
    async fn send_get(&self, url: &str, query: &[(&str, String)], accept: &str) -> anyhow::Result<reqwest::Response> {
        let mut wait = 2u64;
        for attempt in 0..6 {
            let res = self.http.get(url).bearer_auth(&self.token).header("Accept", accept).query(query).send().await?;
            if res.status() != reqwest::StatusCode::TOO_MANY_REQUESTS {
                return Ok(res);
            }
            if attempt == 5 {
                break;
            }
            let retry_after = res.headers().get("retry-after").and_then(|v| v.to_str().ok()).and_then(|v| v.parse::<u64>().ok());
            tokio::time::sleep(std::time::Duration::from_secs(retry_after.unwrap_or(wait).min(60))).await;
            wait = (wait * 2).min(60);
        }
        anyhow::bail!("Garmin limite le nombre de requêtes : réessaie plus tard, la synchronisation reprendra où elle s'est arrêtée")
    }

    async fn search_page(&self, from: NaiveDate, to: NaiveDate, offset: usize) -> Result<Vec<RawActivity>, PageError> {
        let query = [
            ("start", offset.to_string()),
            ("limit", PAGE_LIMIT.to_string()),
            ("startDate", from.to_string()),
            ("endDate", to.to_string()),
        ];
        let res = self
            .send_get(&format!("{API_BASE}/activitylist-service/activities/search/activities"), &query, "application/json")
            .await
            .map_err(PageError::Other)?;
        let status = res.status();
        if status == reqwest::StatusCode::NO_CONTENT {
            return Ok(Vec::new());
        }
        if status == reqwest::StatusCode::INTERNAL_SERVER_ERROR {
            return Err(PageError::TooLarge);
        }
        let body = res.text().await.map_err(|e| PageError::Other(e.into()))?;
        if !status.is_success() {
            let excerpt: String = body.chars().take(180).collect();
            return Err(PageError::Other(anyhow::anyhow!("Garmin a répondu HTTP {status} : {excerpt}")));
        }
        if body.trim().is_empty() {
            return Ok(Vec::new());
        }
        serde_json::from_str(&body).map_err(|e| PageError::Other(anyhow::anyhow!("réponse Garmin illisible : {e}")))
    }

    /// Every activity between two dates (inclusive): a window is split whenever
    /// it holds more than a page.
    async fn search_range(&self, from: NaiveDate, to: NaiveDate) -> anyhow::Result<Vec<RawActivity>> {
        let mut out = Vec::new();
        let mut stack = vec![(from, to)];
        let mut steps = 0;
        while let Some((f, t)) = stack.pop() {
            steps += 1;
            if steps > MAX_SPLIT_STEPS {
                anyhow::bail!("trop de requêtes Garmin pour cette période");
            }
            match self.search_page(f, t, 0).await {
                Ok(page) if page.len() < PAGE_LIMIT => out.extend(page),
                Ok(page) if f == t => {
                    out.extend(page);
                    out.extend(self.search_day_paged(f).await?);
                }
                Err(PageError::TooLarge) if f == t => anyhow::bail!("Garmin refuse de lister le {f} (réponse trop volumineuse)"),
                Ok(_) | Err(PageError::TooLarge) => {
                    let mid = f + Duration::days((t - f).num_days() / 2);
                    stack.push((mid + Duration::days(1), t));
                    stack.push((f, mid));
                }
                Err(PageError::Other(e)) => return Err(e),
            }
        }
        Ok(out)
    }

    /// A single day holding more than a page: the rest, page by page.
    async fn search_day_paged(&self, day: NaiveDate) -> anyhow::Result<Vec<RawActivity>> {
        let mut out = Vec::new();
        let mut offset = PAGE_LIMIT;
        while offset <= MAX_DAY_OFFSET {
            let page = match self.search_page(day, day, offset).await {
                Ok(p) => p,
                Err(PageError::TooLarge) => break,
                Err(PageError::Other(e)) => return Err(e),
            };
            let full = page.len() == PAGE_LIMIT;
            out.extend(page);
            if !full {
                break;
            }
            offset += PAGE_LIMIT;
        }
        Ok(out)
    }
}

/// The ZIP Garmin returns holds the original `.fit`; a bare FIT is accepted too.
fn extract_fit(bytes: Vec<u8>) -> anyhow::Result<Vec<u8>> {
    if !bytes.starts_with(b"PK") {
        return Ok(bytes);
    }
    let mut archive = zip::ZipArchive::new(std::io::Cursor::new(bytes))?;
    for i in 0..archive.len() {
        let mut file = archive.by_index(i)?;
        if file.name().to_lowercase().ends_with(".fit") {
            let mut out = Vec::new();
            file.read_to_end(&mut out)?;
            return Ok(out);
        }
    }
    anyhow::bail!("aucun fichier .fit dans l'archive Garmin")
}

#[async_trait]
impl ActivitySource for GarminSource {
    fn id(&self) -> &'static str {
        "garmin"
    }

    async fn list(&self, after_epoch: i64) -> anyhow::Result<Vec<Listed>> {
        let today = Utc::now().date_naive();
        let mut raw: Vec<RawActivity> = Vec::new();
        if after_epoch > 0 {
            let mut from = chrono::DateTime::from_timestamp(after_epoch, 0).map(|d| d.date_naive()).unwrap_or(today);
            while from <= today {
                let to = (from + Duration::days(WINDOW_DAYS - 1)).min(today);
                raw.extend(self.search_range(from, to).await?);
                from = to + Duration::days(1);
            }
        } else {
            let oldest = NaiveDate::from_ymd_opt(2007, 1, 1).unwrap();
            let (mut to, mut empty) = (today, 0u32);
            while to >= oldest && empty < EMPTY_WINDOWS_BEFORE_STOP {
                let from = (to - Duration::days(WINDOW_DAYS - 1)).max(oldest);
                let found = self.search_range(from, to).await?;
                empty = if found.is_empty() { empty + 1 } else { 0 };
                raw.extend(found);
                to = from - Duration::days(1);
            }
        }

        let mut listed: Vec<Listed> = raw
            .into_iter()
            .filter(|a| !a.manual_activity)
            .filter_map(|a| {
                let sport = map_sport(&a.activity_type.as_ref()?.type_key)?;
                let epoch = a.start_time_gmt.as_deref().and_then(parse_gmt)?;
                let duration_s = a.duration.or(a.elapsed_duration).map(|d| d.round() as i64).filter(|d| *d > 0);
                Some(Listed {
                    external_id: a.activity_id.to_string(),
                    title: a.activity_name.filter(|n| !n.trim().is_empty()).unwrap_or_else(|| "Sortie".to_string()),
                    started_at: rfc3339(epoch),
                    start_epoch: epoch,
                    duration_s,
                    sport_type: sport.to_string(),
                    raw: serde_json::Value::Null,
                })
            })
            .filter(|l| after_epoch <= 0 || l.start_epoch >= after_epoch)
            .collect();
        listed.sort_by(|a, b| a.start_epoch.cmp(&b.start_epoch).then_with(|| a.external_id.cmp(&b.external_id)));
        listed.dedup_by(|a, b| a.external_id == b.external_id);
        tracing::info!(count = listed.len(), "activities fetched from Garmin");
        Ok(listed)
    }

    async fn fetch(&self, listed: &Listed) -> anyhow::Result<Option<NewActivity>> {
        let res = self
            .send_get(
                &format!("{API_BASE}/download-service/files/activity/{}", listed.external_id),
                &[],
                "application/zip, application/octet-stream",
            )
            .await?;
        let status = res.status();
        if status == reqwest::StatusCode::NOT_FOUND || status == reqwest::StatusCode::NO_CONTENT {
            return Ok(None);
        }
        if !status.is_success() {
            anyhow::bail!("téléchargement Garmin refusé (HTTP {status})");
        }
        let fit = extract_fit(res.bytes().await?.to_vec())?;
        let parsed = crate::importer::parse_fit(&fit)?;
        // Power metrics are cycling-only, relative to the cycling FTP.
        let is_ride = crate::metrics::is_ride(&listed.sport_type);
        Ok(Some(NewActivity {
            source: "garmin",
            external_id: Some(listed.external_id.clone()),
            started_at: if parsed.started_at.is_empty() { listed.started_at.clone() } else { parsed.started_at },
            sport_type: listed.sport_type.clone(),
            duration_s: parsed.duration_s,
            distance_m: parsed.distance_m,
            elevation_gain_m: parsed.elevation_gain_m,
            avg_power_w: parsed.avg_power_w.filter(|_| is_ride),
            normalized_power_w: parsed.normalized_power_w.filter(|_| is_ride),
            avg_hr: parsed.avg_hr,
            max_hr: parsed.max_hr,
            title: listed.title.clone(),
            description: None,
            streams: parsed.streams_json,
            laps: parsed.laps,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sources::epoch_of;

    #[test]
    fn sports_are_mapped_and_others_left_out() {
        assert_eq!(map_sport("road_biking"), Some("Ride"));
        assert_eq!(map_sport("indoor_cycling"), Some("VirtualRide"));
        assert_eq!(map_sport("treadmill_running"), Some("Run"));
        assert_eq!(map_sport("trail_running"), Some("TrailRun"));
        assert_eq!(map_sport("gravel_cycling"), None);
        assert_eq!(map_sport("mountain_biking"), None);
        assert_eq!(map_sport("swimming"), None);
    }

    #[test]
    fn garmin_timestamps_are_read_as_utc() {
        let want = epoch_of("2026-11-10T08:00:00Z");
        assert_eq!(parse_gmt("2026-11-10 08:00:00"), want);
        assert_eq!(parse_gmt("2026-11-10T08:00:00"), want);
        assert_eq!(parse_gmt("2026-11-10 08:00:00.0"), want);
        assert_eq!(parse_gmt("nope"), None);
        assert_eq!(rfc3339(want.unwrap()), "2026-11-10T08:00:00Z");
    }

    #[test]
    fn the_fit_is_taken_from_the_archive_or_passed_through() {
        use std::io::Write;
        let mut buf = std::io::Cursor::new(Vec::new());
        {
            let mut zip = zip::ZipWriter::new(&mut buf);
            let opts = zip::write::SimpleFileOptions::default();
            zip.start_file("readme.txt", opts).unwrap();
            zip.write_all(b"hello").unwrap();
            zip.start_file("12345_ACTIVITY.FIT", opts).unwrap();
            zip.write_all(b"FITDATA").unwrap();
            zip.finish().unwrap();
        }
        assert_eq!(extract_fit(buf.into_inner()).unwrap(), b"FITDATA");
        assert_eq!(extract_fit(b"raw".to_vec()).unwrap(), b"raw");
        let mut empty = std::io::Cursor::new(Vec::new());
        zip::ZipWriter::new(&mut empty).finish().unwrap();
        assert!(extract_fit(empty.into_inner()).is_err());
    }
}
