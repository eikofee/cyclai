//! Catalogue de séances « Depuis Zwift » : les plans d'entraînement et les
//! collections de séances vélo de whatsonzwift.com, récupérés par
//! `tools/scrape_zwift.py` dans `data/zwift_plans.json` et embarqués dans le
//! binaire. Importés (ou mis à jour) au démarrage, rangés en catégorie (plans
//! ou collections) > plan/collection > semaine ou sous-groupe ; ces séances
//! sont en lecture seule dans l'interface et leurs cibles en % de FTP suivent
//! la FTP des réglages.

use crate::planned::{self, Sport};
use crate::workout::Athlete;
use serde::Deserialize;
use serde_json::Value;
use sqlx::SqlitePool;

pub const SOURCE: &str = "zwift";

const PLANS: &str = include_str!("../data/zwift_plans.json");

#[derive(Deserialize)]
struct Entry {
    external_id: String,
    category: String,
    plan_key: String,
    plan: String,
    subgroup: Option<String>,
    url: String,
    description: String,
    workout: Value,
}

/// Inserts the catalogue, or refreshes the rows already there (matched on
/// `external_id`, so ids stay stable for sessions planned from them), and
/// drops the ones the catalogue no longer contains.
pub async fn seed(pool: &SqlitePool) -> anyhow::Result<()> {
    let entries: Vec<Entry> = serde_json::from_str(PLANS)?;
    let athlete = Athlete::load(pool).await;
    let mut tx = pool.begin().await?;
    for (position, e) in entries.iter().enumerate() {
        let prepared = planned::prepare(&e.workout, &athlete, Sport::Cycling)
            .map_err(|err| anyhow::anyhow!("séance Zwift {} invalide : {err}", e.external_id))?;
        sqlx::query(
            "INSERT INTO workouts (name, steps_json, source, sport, category, group_key, group_name, subgroup, position,                                    description, source_url, external_id)              VALUES (?, ?, ?, 'cycling', ?, ?, ?, ?, ?, ?, ?, ?)              ON CONFLICT(external_id) WHERE external_id IS NOT NULL DO UPDATE SET                name = excluded.name, steps_json = excluded.steps_json, category = excluded.category,                group_key = excluded.group_key, group_name = excluded.group_name, subgroup = excluded.subgroup,                position = excluded.position, description = excluded.description,                source_url = excluded.source_url, updated_at = datetime('now')",
        )
        .bind(&prepared.name)
        .bind(&prepared.steps_json)
        .bind(SOURCE)
        .bind(&e.category)
        .bind(&e.plan_key)
        .bind(&e.plan)
        .bind(&e.subgroup)
        .bind(position as i64)
        .bind(&e.description)
        .bind(&e.url)
        .bind(&e.external_id)
        .execute(&mut *tx)
        .await?;
    }
    let known: std::collections::HashSet<&str> = entries.iter().map(|e| e.external_id.as_str()).collect();
    let stored: Vec<(i64, String)> =
        sqlx::query_as("SELECT id, external_id FROM workouts WHERE source = ? AND external_id IS NOT NULL")
            .bind(SOURCE)
            .fetch_all(&mut *tx)
            .await?;
    for (id, external_id) in stored.into_iter().filter(|(_, x)| !known.contains(x.as_str())) {
        tracing::info!(external_id, "removing a Zwift session that left the catalogue");
        sqlx::query("DELETE FROM workouts WHERE id = ?").bind(id).execute(&mut *tx).await?;
    }
    tx.commit().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn catalogue_is_valid_and_seeding_is_idempotent() {
        let pool = sqlx::sqlite::SqlitePoolOptions::new().connect("sqlite::memory:").await.unwrap();
        sqlx::migrate!("./migrations").run(&pool).await.unwrap();
        seed(&pool).await.unwrap();
        seed(&pool).await.unwrap();
        let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM workouts WHERE source = 'zwift'").fetch_one(&pool).await.unwrap();
        assert_eq!(n, 1227);
        let ramps: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM workouts WHERE steps_json LIKE '%ftp_ramp%'")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert!(ramps > 0);
        let plans: i64 = sqlx::query_scalar("SELECT COUNT(DISTINCT group_key) FROM workouts WHERE source = 'zwift'")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(plans, 41);
    }

    #[test]
    fn every_session_exports_to_a_readable_fit_file() {
        let a = Athlete { ftp_watts: 205.0, hr_rest: 53.0, hr_max: 192.0, run_threshold_pace_s: 0.0 };
        let entries: Vec<Entry> = serde_json::from_str(PLANS).unwrap();
        for e in entries {
            let w: crate::workout::Workout = serde_json::from_value(e.workout).unwrap();
            for mode in [crate::fit_workout::Mode::PercentFtp, crate::fit_workout::Mode::Watts] {
                let bytes = crate::fit_workout::to_fit(&w, &a, Sport::Cycling, mode);
                fitparser::from_bytes(&bytes).unwrap_or_else(|err| panic!("{}: {err}", w.name));
            }
        }
    }
}
