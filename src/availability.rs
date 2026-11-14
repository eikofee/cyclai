//! How much time the athlete has to ride on a given day: a default per
//! weekday plus per-date exceptions (holidays, long summer evenings…).

use chrono::{Datelike, NaiveDate};
use serde::Serialize;
use sqlx::SqlitePool;
use std::collections::HashMap;

pub const TIMES_OF_DAY: [&str; 3] = ["matin", "soir", "journee"];
pub const CONTEXTS: [&str; 3] = ["travail", "vacances", "autre"];
pub const MAX_MINUTES: i64 = 720;
pub const WEEKDAYS: [&str; 7] = ["Lundi", "Mardi", "Mercredi", "Jeudi", "Vendredi", "Samedi", "Dimanche"];

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Slot {
    pub available_min: i64,
    pub time_of_day: String,
    pub context: String,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Day {
    pub date: NaiveDate,
    pub available_min: i64,
    pub time_of_day: String,
    pub context: String,
    /// `regle_par_defaut` | `exception`
    pub source: &'static str,
    pub note: Option<String>,
}

/// 1 = Monday … 7 = Sunday.
pub fn weekday_number(date: NaiveDate) -> i64 {
    date.weekday().number_from_monday() as i64
}

/// Used when the defaults table has no row for a weekday.
fn fallback(weekday: i64) -> Slot {
    if weekday >= 6 {
        Slot { available_min: 180, time_of_day: "journee".into(), context: "autre".into() }
    } else {
        Slot { available_min: 90, time_of_day: "soir".into(), context: "travail".into() }
    }
}

pub fn validate(slot: &Slot) -> Result<(), String> {
    if !(0..=MAX_MINUTES).contains(&slot.available_min) {
        return Err(format!("`available_min` must be between 0 and {MAX_MINUTES}"));
    }
    if !TIMES_OF_DAY.contains(&slot.time_of_day.as_str()) {
        return Err(format!("`time_of_day` must be one of: {}", TIMES_OF_DAY.join(", ")));
    }
    if !CONTEXTS.contains(&slot.context.as_str()) {
        return Err(format!("`context` must be one of: {}", CONTEXTS.join(", ")));
    }
    Ok(())
}

/// The seven weekday defaults, Monday first.
pub async fn defaults(pool: &SqlitePool) -> sqlx::Result<Vec<Slot>> {
    let rows: Vec<(i64, i64, String, String)> =
        sqlx::query_as("SELECT weekday, available_min, time_of_day, context FROM availability_defaults").fetch_all(pool).await?;
    let mut by_day: HashMap<i64, Slot> =
        rows.into_iter().map(|(w, m, t, c)| (w, Slot { available_min: m, time_of_day: t, context: c })).collect();
    Ok((1..=7).map(|w| by_day.remove(&w).unwrap_or_else(|| fallback(w))).collect())
}

pub async fn save_defaults(pool: &SqlitePool, slots: &[Slot]) -> sqlx::Result<()> {
    let mut tx = pool.begin().await?;
    for (i, s) in slots.iter().enumerate() {
        sqlx::query(
            "INSERT INTO availability_defaults (weekday, available_min, time_of_day, context) VALUES (?, ?, ?, ?) \
             ON CONFLICT(weekday) DO UPDATE SET available_min = excluded.available_min, \
             time_of_day = excluded.time_of_day, context = excluded.context",
        )
        .bind(i as i64 + 1)
        .bind(s.available_min)
        .bind(&s.time_of_day)
        .bind(&s.context)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await
}

/// Availability for every day of `from..=to`: the exception of the date when
/// there is one, the weekday default otherwise.
pub async fn range(pool: &SqlitePool, from: NaiveDate, to: NaiveDate) -> sqlx::Result<Vec<Day>> {
    let defaults = defaults(pool).await?;
    let rows: Vec<(String, i64, String, String, Option<String>)> = sqlx::query_as(
        "SELECT date, available_min, time_of_day, context, note FROM availability_exceptions WHERE date >= ? AND date <= ?",
    )
    .bind(from.to_string())
    .bind(to.to_string())
    .fetch_all(pool)
    .await?;
    let mut exceptions: HashMap<String, (Slot, Option<String>)> = rows
        .into_iter()
        .map(|(d, m, t, c, n)| (d, (Slot { available_min: m, time_of_day: t, context: c }, n)))
        .collect();

    let mut out = Vec::new();
    let mut date = from;
    while date <= to {
        let day = match exceptions.remove(&date.to_string()) {
            Some((s, note)) => Day {
                date,
                available_min: s.available_min,
                time_of_day: s.time_of_day,
                context: s.context,
                source: "exception",
                note,
            },
            None => {
                let s = &defaults[(weekday_number(date) - 1) as usize];
                Day {
                    date,
                    available_min: s.available_min,
                    time_of_day: s.time_of_day.clone(),
                    context: s.context.clone(),
                    source: "regle_par_defaut",
                    note: None,
                }
            }
        };
        out.push(day);
        date += chrono::Duration::days(1);
    }
    Ok(out)
}

/// Creates or replaces the exception of one date.
pub async fn set_exception(pool: &SqlitePool, date: NaiveDate, slot: &Slot, note: Option<&str>) -> sqlx::Result<()> {
    sqlx::query(
        "INSERT INTO availability_exceptions (date, available_min, time_of_day, context, note) VALUES (?, ?, ?, ?, ?) \
         ON CONFLICT(date) DO UPDATE SET available_min = excluded.available_min, time_of_day = excluded.time_of_day, \
         context = excluded.context, note = excluded.note, updated_at = datetime('now')",
    )
    .bind(date.to_string())
    .bind(slot.available_min)
    .bind(&slot.time_of_day)
    .bind(&slot.context)
    .bind(note)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn delete_exception(pool: &SqlitePool, date: &str) -> sqlx::Result<()> {
    sqlx::query("DELETE FROM availability_exceptions WHERE date = ?").bind(date).execute(pool).await?;
    Ok(())
}

/// Exceptions dated `from` or later, soonest first (for the settings page).
pub async fn exceptions_from(pool: &SqlitePool, from: NaiveDate) -> sqlx::Result<Vec<(String, Slot, Option<String>)>> {
    let rows: Vec<(String, i64, String, String, Option<String>)> = sqlx::query_as(
        "SELECT date, available_min, time_of_day, context, note FROM availability_exceptions WHERE date >= ? ORDER BY date",
    )
    .bind(from.to_string())
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(d, m, t, c, n)| (d, Slot { available_min: m, time_of_day: t, context: c }, n))
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn pool() -> SqlitePool {
        let pool = sqlx::sqlite::SqlitePoolOptions::new().connect("sqlite::memory:").await.unwrap();
        crate::db::migrate(&pool).await.unwrap();
        pool
    }

    fn day(s: &str) -> NaiveDate {
        NaiveDate::parse_from_str(s, "%Y-%m-%d").unwrap()
    }

    #[tokio::test]
    async fn default_rules_apply_until_an_exception_replaces_them() {
        let pool = pool().await;
        // 2026-11-09 is a Monday.
        let days = range(&pool, day("2026-11-09"), day("2026-11-15")).await.unwrap();
        assert_eq!(days.len(), 7);
        assert_eq!((days[0].available_min, days[0].time_of_day.as_str(), days[0].source), (90, "soir", "regle_par_defaut"));
        assert_eq!((days[5].available_min, days[5].time_of_day.as_str(), days[5].context.as_str()), (180, "journee", "autre"));

        let holiday = Slot { available_min: 300, time_of_day: "journee".into(), context: "vacances".into() };
        set_exception(&pool, day("2026-11-10"), &holiday, Some("congés")).await.unwrap();
        let days = range(&pool, day("2026-11-09"), day("2026-11-11")).await.unwrap();
        assert_eq!((days[1].available_min, days[1].source), (300, "exception"));
        assert_eq!(days[1].note.as_deref(), Some("congés"));
        assert_eq!(days[2].source, "regle_par_defaut");

        // A second call replaces the first.
        let short = Slot { available_min: 45, ..holiday };
        set_exception(&pool, day("2026-11-10"), &short, None).await.unwrap();
        assert_eq!(range(&pool, day("2026-11-10"), day("2026-11-10")).await.unwrap()[0].available_min, 45);

        delete_exception(&pool, "2026-11-10").await.unwrap();
        assert_eq!(range(&pool, day("2026-11-10"), day("2026-11-10")).await.unwrap()[0].source, "regle_par_defaut");
    }

    #[tokio::test]
    async fn defaults_can_be_edited() {
        let pool = pool().await;
        let mut slots = defaults(&pool).await.unwrap();
        assert_eq!(slots.len(), 7);
        slots[2].available_min = 60;
        save_defaults(&pool, &slots).await.unwrap();
        assert_eq!(defaults(&pool).await.unwrap()[2].available_min, 60);
    }

    #[test]
    fn slots_are_validated() {
        let ok = Slot { available_min: 90, time_of_day: "soir".into(), context: "travail".into() };
        assert!(validate(&ok).is_ok());
        assert!(validate(&Slot { available_min: -1, ..ok.clone() }).is_err());
        assert!(validate(&Slot { available_min: 721, ..ok.clone() }).is_err());
        assert!(validate(&Slot { time_of_day: "nuit".into(), ..ok.clone() }).is_err());
        assert!(validate(&Slot { context: "boulot".into(), ..ok }).is_err());
    }
}
