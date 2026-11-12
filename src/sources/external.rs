//! Adapter that makes a plugin (a separate HTTP service, see
//! `cyclai-plugin-sdk`) look like any other [`ActivitySource`]. What a plugin
//! sends is treated as untrusted: sports, sizes and numbers are checked before
//! anything reaches the database.

use super::{registry, ActivitySource, Listed, NewActivity};
use crate::importer::ParsedLap;
use async_trait::async_trait;
use cyclai_plugin_sdk::{Activity, Client, ClientError, Fetched, ListedActivity, Manifest};

/// An activity longer than this is not credible.
const MAX_DURATION_S: i64 = 30 * 24 * 3600;
const MAX_LAPS: usize = 10_000;
const MAX_LISTED: usize = 200_000;
const MAX_TITLE: usize = 300;
const MAX_DESCRIPTION: usize = 5_000;
const MAX_EXTERNAL_ID: usize = 200;

pub struct ExternalSource {
    id: String,
    client: Client,
}

pub fn client_for(id: &str) -> Result<Client, String> {
    let endpoint = registry::get(id).ok_or_else(|| format!("plugin « {id} » inconnu"))?;
    if endpoint.secret.is_empty() {
        return Err(format!("secret manquant : définis CYCLAI_PLUGIN_{}_SECRET", id.to_ascii_uppercase()));
    }
    Client::new(&endpoint.url, &endpoint.secret).map_err(|e| e.to_string())
}

/// The plugin's manifest, or why it cannot be read.
pub async fn manifest_of(id: &str) -> Result<Manifest, String> {
    let client = client_for(id)?;
    let m = client.manifest().await.map_err(|e| e.to_string())?;
    if m.id != id {
        return Err(format!("le plugin répond comme « {} » alors qu'il est déclaré « {id} »", m.id));
    }
    Ok(m)
}

pub async fn connect(id: &str) -> anyhow::Result<Box<dyn ActivitySource>> {
    let manifest = manifest_of(id).await.map_err(anyhow::Error::msg)?;
    if !manifest.configured {
        anyhow::bail!("pas configuré{}", manifest.message.as_deref().map(|m| format!(" : {m}")).unwrap_or_default());
    }
    let client = client_for(id).map_err(anyhow::Error::msg)?;
    Ok(Box::new(ExternalSource { id: id.to_string(), client }))
}

fn clean(s: &str, max: usize) -> String {
    s.chars().filter(|c| !c.is_control() || *c == '\n').take(max).collect()
}

fn finite(v: Option<f64>) -> Option<f64> {
    v.filter(|x| x.is_finite() && *x >= 0.0)
}

fn listed_from(a: ListedActivity) -> Option<Listed> {
    if a.external_id.is_empty() || a.external_id.len() > MAX_EXTERNAL_ID || !crate::metrics::is_supported_sport(&a.sport_type) {
        return None;
    }
    // The epoch is recomputed from the date: the two cannot disagree.
    let start_epoch = super::epoch_of(&a.started_at)?;
    Some(Listed {
        external_id: a.external_id,
        title: clean(&a.title, MAX_TITLE),
        started_at: a.started_at,
        start_epoch,
        duration_s: a.duration_s.filter(|d| (1..=MAX_DURATION_S).contains(d)),
        sport_type: a.sport_type,
    })
}

fn activity_from(source: &str, listed: &Listed, a: Activity) -> anyhow::Result<NewActivity> {
    if !crate::metrics::is_supported_sport(&a.sport_type) {
        anyhow::bail!("sport non géré : {}", a.sport_type);
    }
    super::epoch_of(&a.started_at).ok_or_else(|| anyhow::anyhow!("date de début illisible"))?;
    if !(0..=MAX_DURATION_S).contains(&a.duration_s) || !a.distance_m.is_finite() || !a.elevation_gain_m.is_finite() {
        anyhow::bail!("valeurs incohérentes (durée, distance ou dénivelé)");
    }
    if !a.streams.is_object() {
        anyhow::bail!("streams invalides");
    }
    // Power metrics are cycling-only, relative to the cycling FTP.
    let is_ride = crate::metrics::is_ride(&a.sport_type);
    Ok(NewActivity {
        source: source.to_string(),
        external_id: Some(listed.external_id.clone()),
        started_at: a.started_at,
        sport_type: a.sport_type,
        duration_s: a.duration_s,
        distance_m: a.distance_m.max(0.0),
        elevation_gain_m: a.elevation_gain_m.max(0.0),
        avg_power_w: finite(a.avg_power_w).filter(|_| is_ride),
        normalized_power_w: finite(a.normalized_power_w).filter(|_| is_ride),
        avg_hr: finite(a.avg_hr),
        max_hr: finite(a.max_hr),
        title: clean(&a.title, MAX_TITLE),
        description: a.description.map(|d| clean(&d, MAX_DESCRIPTION)).filter(|d| !d.trim().is_empty()),
        streams: a.streams,
        laps: a
            .laps
            .into_iter()
            .take(MAX_LAPS)
            .map(|l| ParsedLap {
                lap_index: l.lap_index,
                elapsed_time_s: l.elapsed_time_s.max(0),
                distance_m: l.distance_m.max(0.0),
                avg_power_w: finite(l.avg_power_w).filter(|_| is_ride),
                avg_hr: finite(l.avg_hr),
                max_hr: finite(l.max_hr),
            })
            .collect(),
    })
}

fn from_fit(source: &str, listed: &Listed, fit: &[u8]) -> anyhow::Result<NewActivity> {
    let parsed = crate::importer::parse_fit(fit)?;
    let is_ride = crate::metrics::is_ride(&listed.sport_type);
    Ok(NewActivity {
        source: source.to_string(),
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
    })
}

fn explain(e: ClientError) -> anyhow::Error {
    match e {
        ClientError::Rejected { status: 429, message, retry_after_s } => anyhow::anyhow!(
            "limite de débit atteinte ({message}){}: réessaie plus tard, la synchronisation reprendra où elle s'est arrêtée",
            retry_after_s.map(|s| format!(", réessayer dans {s} s ")).unwrap_or_default()
        ),
        other => anyhow::anyhow!("{other}"),
    }
}

#[async_trait]
impl ActivitySource for ExternalSource {
    fn id(&self) -> &str {
        &self.id
    }

    async fn list(&self, after_epoch: i64) -> anyhow::Result<Vec<Listed>> {
        let listed = self.client.list(after_epoch).await.map_err(explain)?;
        if listed.len() > MAX_LISTED {
            anyhow::bail!("le plugin annonce trop d'activités ({})", listed.len());
        }
        let mut out: Vec<Listed> = listed.into_iter().filter_map(listed_from).filter(|l| after_epoch <= 0 || l.start_epoch >= after_epoch).collect();
        out.sort_by(|a, b| a.start_epoch.cmp(&b.start_epoch).then_with(|| a.external_id.cmp(&b.external_id)));
        out.dedup_by(|a, b| a.external_id == b.external_id);
        Ok(out)
    }

    async fn fetch(&self, listed: &Listed) -> anyhow::Result<Option<NewActivity>> {
        match self.client.fetch(&listed.external_id).await.map_err(explain)? {
            Fetched::Activity(a) => Ok(Some(activity_from(&self.id, listed, a)?)),
            Fetched::Fit(bytes) => Ok(Some(from_fit(&self.id, listed, &bytes)?)),
            Fetched::NotAvailable => Ok(None),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cyclai_plugin_sdk::Lap;

    fn listed() -> Listed {
        Listed {
            external_id: "7".into(),
            title: "t".into(),
            started_at: "2026-11-10T08:00:00Z".into(),
            start_epoch: 0,
            duration_s: None,
            sport_type: "Ride".into(),
        }
    }

    fn activity() -> Activity {
        Activity {
            started_at: "2026-11-10T08:00:00Z".into(),
            sport_type: "Ride".into(),
            duration_s: 3600,
            distance_m: 30_000.0,
            elevation_gain_m: 100.0,
            avg_power_w: Some(180.0),
            normalized_power_w: Some(190.0),
            avg_hr: Some(140.0),
            max_hr: Some(170.0),
            title: "Sortie\u{7}".into(),
            description: Some("  ".into()),
            streams: serde_json::json!({"time": {"data": [0, 1]}}),
            laps: vec![Lap { lap_index: 1, elapsed_time_s: 60, distance_m: 1.0, avg_power_w: Some(f64::NAN), avg_hr: None, max_hr: None }],
        }
    }

    #[test]
    fn a_plugin_activity_is_cleaned_before_storage() {
        let a = activity_from("alpha", &listed(), activity()).unwrap();
        assert_eq!((a.source.as_str(), a.external_id.as_deref()), ("alpha", Some("7")));
        assert_eq!(a.title, "Sortie", "control characters are dropped");
        assert_eq!(a.description, None, "a blank description is nothing");
        assert_eq!(a.laps[0].avg_power_w, None, "NaN never reaches the database");
        assert_eq!(a.avg_power_w, Some(180.0));
    }

    #[test]
    fn power_is_dropped_for_runs_and_bad_activities_are_refused() {
        let run = Activity { sport_type: "Run".into(), ..activity() };
        let a = activity_from("x", &listed(), run).unwrap();
        assert_eq!((a.avg_power_w, a.normalized_power_w), (None, None));

        for bad in [
            Activity { sport_type: "Swim".into(), ..activity() },
            Activity { started_at: "yesterday".into(), ..activity() },
            Activity { duration_s: -1, ..activity() },
            Activity { duration_s: MAX_DURATION_S + 1, ..activity() },
            Activity { distance_m: f64::INFINITY, ..activity() },
            Activity { streams: serde_json::json!([1, 2]), ..activity() },
        ] {
            assert!(activity_from("x", &listed(), bad).is_err());
        }
    }

    #[test]
    fn the_listing_drops_what_cannot_be_trusted() {
        let ok = ListedActivity {
            external_id: "1".into(),
            title: "t".into(),
            started_at: "2026-11-10T08:00:00Z".into(),
            start_epoch: 42, // ignored: recomputed from the date
            duration_s: Some(3600),
            sport_type: "Ride".into(),
        };
        let kept = listed_from(ok.clone()).unwrap();
        assert_eq!(kept.start_epoch, super::super::epoch_of("2026-11-10T08:00:00Z").unwrap());
        assert!(listed_from(ListedActivity { sport_type: "Swim".into(), ..ok.clone() }).is_none());
        assert!(listed_from(ListedActivity { started_at: "nope".into(), ..ok.clone() }).is_none());
        assert!(listed_from(ListedActivity { external_id: String::new(), ..ok.clone() }).is_none());
        assert_eq!(listed_from(ListedActivity { duration_s: Some(-5), ..ok }).unwrap().duration_s, None);
    }
}
