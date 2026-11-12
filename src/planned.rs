//! Planned (scheduled) workouts: a session placed on a calendar day.
//!
//! Shared by the web pages (`web::planned`, the calendar, the designer) and
//! the JSON API, so every entry point validates and stores the same way. A
//! planned session owns a snapshot of its steps — it may come from a template
//! (`workouts`, tracked by `workout_id` for information only) or be unique to
//! its day.

use crate::workout::{Athlete, Workout};
use chrono::NaiveDate;
use serde_json::Value;
use sqlx::SqlitePool;

/// Sport of a session. Stored as its `as_str` in `workouts.sport` /
/// `planned_workouts.sport`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Sport {
    #[default]
    Cycling,
    Running,
    Other,
}

impl Sport {
    pub const ALL: [Sport; 3] = [Sport::Cycling, Sport::Running, Sport::Other];

    pub fn as_str(self) -> &'static str {
        match self {
            Sport::Cycling => "cycling",
            Sport::Running => "running",
            Sport::Other => "other",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Sport::Cycling => "Cyclisme",
            Sport::Running => "Course à pied",
            Sport::Other => "Autre",
        }
    }

    pub fn icon(self) -> &'static str {
        match self {
            Sport::Cycling => "🚴",
            Sport::Running => "🏃",
            Sport::Other => "📅",
        }
    }

    pub fn parse(s: &str) -> Option<Sport> {
        Sport::ALL.into_iter().find(|sp| sp.as_str() == s.trim())
    }

    /// Lenient read of a stored value (unknown -> cycling).
    pub fn from_db(s: &str) -> Sport {
        Sport::parse(s).unwrap_or_default()
    }
}

#[derive(sqlx::FromRow, Clone)]
pub struct Planned {
    pub id: i64,
    pub planned_date: String,
    pub name: String,
    pub steps_json: String,
    pub notes: Option<String>,
    pub workout_id: Option<i64>,
    pub source: String,
    pub sport: String,
    pub status: String,
    pub rationale: Option<String>,
    /// `workout` (structured steps) or `free_ride` (budget only).
    pub kind: String,
    pub budget_tss_min: Option<f64>,
    pub budget_tss_max: Option<f64>,
    pub budget_duration_max_min: Option<i64>,
    pub intent: Option<String>,
    /// The ride a `done` session was matched with.
    pub activity_id: Option<i64>,
    /// Other sessions the agent offered with this proposal (JSON snapshot).
    pub alternatives_json: Option<String>,
}

const COLUMNS: &str = "id, planned_date, name, steps_json, notes, workout_id, source, sport, status, rationale, \
                       kind, budget_tss_min, budget_tss_max, budget_duration_max_min, intent, activity_id, alternatives_json";

impl Planned {
    pub fn is_free_ride(&self) -> bool {
        self.kind == "free_ride"
    }

    /// `(minutes, tss)` this session is expected to add to the load: the
    /// steps' estimate for a workout, the middle of the budget for a free ride.
    pub fn load_estimate(&self, athlete: &Athlete) -> (f64, f64) {
        if self.is_free_ride() {
            let minutes = self.budget_duration_max_min.unwrap_or(0) as f64;
            let tss = (self.budget_tss_min.unwrap_or(0.0) + self.budget_tss_max.unwrap_or(0.0)) / 2.0;
            return (minutes, tss);
        }
        serde_json::from_str::<Workout>(&self.steps_json).map(|w| w.load_estimate(athlete)).unwrap_or((0.0, 0.0))
    }
}

pub const INTENTS: [&str; 6] = ["endurance", "denivele", "recuperation", "tempo", "intense", "course"];

pub fn intent_label(intent: &str) -> &'static str {
    match intent {
        "denivele" => "dénivelé",
        "recuperation" => "récupération",
        "tempo" => "tempo",
        "intense" => "intense",
        "course" => "course",
        _ => "endurance",
    }
}

/// Highest IF a ride of that intent is expected to reach; bounds the TSS a
/// free-ride budget may claim for its duration.
fn intent_if_cap(intent: &str) -> f64 {
    match intent {
        "recuperation" => 0.60,
        "denivele" => 0.85,
        "tempo" => 0.88,
        "intense" => 0.95,
        "course" => 1.0,
        _ => 0.75,
    }
}

/// Most TSS a ride of `minutes` can plausibly give for that intent.
pub fn tss_ceiling(intent: &str, minutes: i64) -> f64 {
    let cap = intent_if_cap(intent);
    minutes as f64 / 60.0 * 100.0 * cap * cap
}

/// Shortest free ride worth planning.
pub const FREE_RIDE_MIN_MINUTES: i64 = 20;

#[derive(Debug, Clone, PartialEq)]
pub struct FreeRideBudget {
    pub tss_min: f64,
    pub tss_max: f64,
    pub duration_max_min: i64,
    pub intent: String,
}

/// Builds the budget of a free ride on a day with `available_min` to ride.
/// Without `tss_range` the engine fills it in from the duration and the
/// intent. The error is a user-facing message (the request is well-formed but
/// the engine refuses it).
///
/// `expected_if` states how hard the ride will really be (a race, a fast group
/// ride...): it replaces the intent's ceiling, and the budget is built around
/// it so the rules see that intensity. It excludes `tss_range`.
pub const EXPECTED_IF_RANGE: std::ops::RangeInclusive<f64> = 0.3..=1.2;

pub fn free_ride_budget(
    tss_range: Option<(f64, f64)>,
    duration_max_min: i64,
    intent: &str,
    available_min: i64,
    expected_if: Option<f64>,
) -> Result<FreeRideBudget, String> {
    if !INTENTS.contains(&intent) {
        return Err(format!("`intent` must be one of: {}", INTENTS.join(", ")));
    }
    if duration_max_min < FREE_RIDE_MIN_MINUTES {
        return Err(format!("`duration_max_min` must be at least {FREE_RIDE_MIN_MINUTES}"));
    }
    if duration_max_min > available_min {
        return Err(format!(
            "`duration_max_min` ({duration_max_min}) exceeds the {available_min} min available that day (see /api/v1/availability)"
        ));
    }
    if let Some(e) = expected_if {
        if !EXPECTED_IF_RANGE.contains(&e) {
            return Err(format!("`expected_if` must be between {} and {}", EXPECTED_IF_RANGE.start(), EXPECTED_IF_RANGE.end()));
        }
        if tss_range.is_some() {
            return Err("give `expected_if` or `tss_range`, not both".to_string());
        }
        let center = duration_max_min as f64 / 60.0 * 100.0 * e * e;
        return Ok(FreeRideBudget {
            tss_min: (center * 0.9).round(),
            tss_max: (center * 1.1).round(),
            duration_max_min,
            intent: intent.to_string(),
        });
    }
    let ceiling = tss_ceiling(intent, duration_max_min);
    let (tss_min, tss_max) = match tss_range {
        Some((lo, hi)) => {
            if !(lo.is_finite() && hi.is_finite()) || lo < 0.0 || hi < lo {
                return Err("`tss_range` must be [min, max] with 0 <= min <= max".to_string());
            }
            if hi > ceiling.ceil() {
                return Err(format!(
                    "`tss_range` max {hi:.0} is out of reach for {duration_max_min} min of {intent} (at most {:.0})",
                    ceiling.floor()
                ));
            }
            (lo, hi)
        }
        None => ((ceiling * 0.6).round(), ceiling.round()),
    };
    Ok(FreeRideBudget { tss_min, tss_max, duration_max_min, intent: intent.to_string() })
}

pub async fn insert_free_ride(
    pool: &SqlitePool,
    date: NaiveDate,
    budget: &FreeRideBudget,
    source: &str,
) -> anyhow::Result<i64> {
    let name = format!("Sortie libre — {}", intent_label(&budget.intent));
    let steps = serde_json::json!({ "name": name, "steps": [] }).to_string();
    let result = sqlx::query(
        "INSERT INTO planned_workouts (planned_date, name, steps_json, source, sport, kind, budget_tss_min, \
         budget_tss_max, budget_duration_max_min, intent) VALUES (?, ?, ?, ?, 'cycling', 'free_ride', ?, ?, ?, ?)",
    )
    .bind(date.to_string())
    .bind(&name)
    .bind(steps)
    .bind(source)
    .bind(budget.tss_min)
    .bind(budget.tss_max)
    .bind(budget.duration_max_min)
    .bind(&budget.intent)
    .execute(pool)
    .await?;
    Ok(result.last_insert_rowid())
}

/// Marks as `done` the `planned` / `confirmed` sessions of `today` and earlier
/// for which a ride exists on their day (a ride of the session's sport, each
/// ride matching at most one session), and links them. Returns how many
/// sessions were completed. Idempotent; called whenever the plan is read.
pub async fn reconcile(pool: &SqlitePool, today: NaiveDate) -> anyhow::Result<u64> {
    let pending: Vec<(i64, String, String)> = sqlx::query_as(
        "SELECT id, planned_date, sport FROM planned_workouts \
         WHERE status IN ('planned', 'confirmed') AND planned_date <= ? ORDER BY planned_date, id",
    )
    .bind(today.to_string())
    .fetch_all(pool)
    .await?;
    let mut done = 0;
    for (id, date, sport) in pending {
        let types = match Sport::from_db(&sport) {
            Sport::Cycling => "('Ride', 'VirtualRide')",
            Sport::Running => "('Run', 'TrailRun')",
            Sport::Other => continue,
        };
        let activity: Option<i64> = sqlx::query_scalar(&format!(
            "SELECT a.id FROM activities a WHERE date(a.started_at) = ? AND a.sport_type IN {types} \
             AND NOT EXISTS (SELECT 1 FROM planned_workouts p WHERE p.activity_id = a.id) \
             ORDER BY a.duration_s DESC LIMIT 1"
        ))
        .bind(&date)
        .fetch_optional(pool)
        .await?;
        if let Some(activity) = activity {
            let r = sqlx::query(
                "UPDATE planned_workouts SET status = 'done', activity_id = ?, updated_at = datetime('now') \
                 WHERE id = ? AND status IN ('planned', 'confirmed')",
            )
            .bind(activity)
            .bind(id)
            .execute(pool)
            .await?;
            done += r.rows_affected();
        }
    }
    Ok(done)
}

/// A validated workout ready to store: name + the full Workout JSON with the
/// estimated distance filled in (same shape as `workouts.steps_json`).
#[derive(Debug)]
pub struct Prepared {
    pub name: String,
    pub steps_json: String,
}

/// `YYYY-MM-DD` only — the one format stored and compared as text.
pub fn parse_date(s: &str) -> Option<NaiveDate> {
    NaiveDate::parse_from_str(s.trim(), "%Y-%m-%d").ok()
}

/// Parses + validates a Workout JSON value (from the designer or the API)
/// and computes its distance estimate (see below). The error is a user-facing
/// message.
pub fn prepare(workout: &Value, athlete: &Athlete, sport: Sport) -> Result<Prepared, String> {
    let workout: Workout = serde_json::from_value(workout.clone()).map_err(|e| format!("JSON invalide : {e}"))?;
    if workout.name.trim().is_empty() {
        return Err("La séance doit avoir un nom.".to_string());
    }
    if let Some(err) = workout.validate() {
        return Err(err);
    }
    // Cycling: speed model from power/zone targets. Running: from the pace
    // targets. Anything else has no distance.
    let distance_km = match sport {
        Sport::Cycling => Some(workout.estimate_distance_km(athlete)),
        Sport::Running => workout.estimate_run_distance_km(athlete),
        Sport::Other => None,
    };
    let mut value = serde_json::to_value(&workout).map_err(|e| e.to_string())?;
    if let Some(obj) = value.as_object_mut() {
        match distance_km {
            Some(km) => {
                obj.insert("estimated_distance_km".to_string(), serde_json::json!((km * 10.0).round() / 10.0));
            }
            None => {
                obj.remove("estimated_distance_km");
            }
        }
    }
    Ok(Prepared { name: workout.name, steps_json: value.to_string() })
}

pub async fn insert(
    pool: &SqlitePool,
    date: NaiveDate,
    prepared: &Prepared,
    notes: Option<&str>,
    workout_id: Option<i64>,
    source: &str,
    sport: Sport,
) -> anyhow::Result<i64> {
    let result = sqlx::query(
        "INSERT INTO planned_workouts (planned_date, name, steps_json, notes, workout_id, source, sport) \
         VALUES (?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(date.to_string())
    .bind(&prepared.name)
    .bind(&prepared.steps_json)
    .bind(notes.map(str::trim).filter(|n| !n.is_empty()))
    .bind(workout_id)
    .bind(source)
    .bind(sport.as_str())
    .execute(pool)
    .await?;
    Ok(result.last_insert_rowid())
}

pub async fn update(
    pool: &SqlitePool,
    id: i64,
    date: NaiveDate,
    prepared: &Prepared,
    notes: Option<&str>,
    sport: Sport,
) -> anyhow::Result<bool> {
    let result = sqlx::query(
        "UPDATE planned_workouts SET planned_date = ?, name = ?, steps_json = ?, notes = ?, sport = ?, \
         updated_at = datetime('now') WHERE id = ?",
    )
    .bind(date.to_string())
    .bind(&prepared.name)
    .bind(&prepared.steps_json)
    .bind(notes.map(str::trim).filter(|n| !n.is_empty()))
    .bind(sport.as_str())
    .bind(id)
    .execute(pool)
    .await?;
    Ok(result.rows_affected() > 0)
}

/// Allowed moves of a session's `status`. `proposed` (suggested by an agent)
/// is settled by the user in the UI: `confirmed` or `rejected`. Anything still
/// upcoming can be `skipped`. `done`, `skipped` and `rejected` are final.
pub fn can_transition(from: &str, to: &str) -> bool {
    matches!(
        (from, to),
        ("proposed", "confirmed" | "rejected" | "skipped")
            | ("planned" | "confirmed", "skipped" | "done")
            | ("proposed", "done")
    )
}

#[cfg(test)]
mod status_tests {
    use super::can_transition as ok;

    #[test]
    fn proposals_are_settled_and_final_states_stay_final() {
        assert!(ok("proposed", "confirmed") && ok("proposed", "rejected"));
        assert!(ok("planned", "skipped") && ok("confirmed", "done"));
        for from in ["done", "skipped", "rejected"] {
            for to in ["planned", "proposed", "confirmed", "rejected", "skipped", "done"] {
                assert!(!ok(from, to), "{from} -> {to}");
            }
        }
        assert!(!ok("planned", "confirmed"), "only proposals are confirmed");
        assert!(!ok("confirmed", "rejected"));
    }
}

/// Moves a session to `to` if the transition is allowed. Returns `Ok(false)`
/// when the session doesn't exist or the move isn't allowed.
pub async fn set_status(pool: &SqlitePool, id: i64, to: &str) -> anyhow::Result<bool> {
    let Some(current) = get(pool, id).await? else { return Ok(false) };
    if !can_transition(&current.status, to) {
        return Ok(false);
    }
    let result = sqlx::query(
        "UPDATE planned_workouts SET status = ?, updated_at = datetime('now') WHERE id = ? AND status = ?",
    )
    .bind(to)
    .bind(id)
    .bind(&current.status)
    .execute(pool)
    .await?;
    Ok(result.rows_affected() > 0)
}

/// Turns a freshly inserted session into an agent proposal.
pub async fn mark_proposal(pool: &SqlitePool, id: i64, rationale: &str, idempotency_key: Option<&str>) -> anyhow::Result<()> {
    sqlx::query("UPDATE planned_workouts SET status = 'proposed', rationale = ?, idempotency_key = ? WHERE id = ?")
        .bind(rationale)
        .bind(idempotency_key)
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

/// Snapshot of a library session offered as an alternative (what the user needs
/// to choose: name, duration, TSS) - kept as it was at proposal time.
pub fn alternative_snapshot(workout_id: i64, name: &str, steps_json: &str, athlete: &Athlete, note: Option<&str>) -> serde_json::Value {
    let (minutes, tss) = serde_json::from_str::<Workout>(steps_json).map(|w| w.load_estimate(athlete)).unwrap_or((0.0, 0.0));
    serde_json::json!({
        "workout_id": workout_id,
        "name": name,
        "duration_min": (minutes * 10.0).round() / 10.0,
        "estimated_tss": tss.round(),
        "note": note,
    })
}

impl Planned {
    /// The alternatives offered with this proposal, if any.
    pub fn alternatives(&self) -> Vec<serde_json::Value> {
        self.alternatives_json
            .as_deref()
            .and_then(|j| serde_json::from_str(j).ok())
            .unwrap_or_default()
    }
}

pub async fn set_alternatives(pool: &SqlitePool, id: i64, alternatives: &[serde_json::Value]) -> anyhow::Result<()> {
    let json = (!alternatives.is_empty()).then(|| serde_json::Value::Array(alternatives.to_vec()).to_string());
    sqlx::query("UPDATE planned_workouts SET alternatives_json = ? WHERE id = ?").bind(json).bind(id).execute(pool).await?;
    Ok(())
}

/// Replaces a still-`proposed` session by one of its alternatives; the former
/// main session becomes an alternative in turn, so the choice can be undone.
/// `Ok(Err(msg))` is a user-facing refusal.
pub async fn swap_to_alternative(pool: &SqlitePool, id: i64, new_workout_id: i64, athlete: &Athlete) -> anyhow::Result<Result<(), String>> {
    let Some(p) = get(pool, id).await? else { return Ok(Err("Séance introuvable.".into())) };
    if p.status != "proposed" || p.is_free_ride() {
        return Ok(Err("Seule une proposition en attente peut être remplacée.".into()));
    }
    let alts = p.alternatives();
    let Some(chosen) = alts.iter().find(|a| a["workout_id"].as_i64() == Some(new_workout_id)) else {
        return Ok(Err("Cette séance ne fait pas partie des alternatives proposées.".into()));
    };
    let row: Option<(String, String)> = sqlx::query_as("SELECT steps_json, sport FROM workouts WHERE id = ?").bind(new_workout_id).fetch_optional(pool).await?;
    let Some((steps, sport)) = row else { return Ok(Err("Ce modèle n'existe plus.".into())) };
    let sport = Sport::from_db(&sport);
    let workout: serde_json::Value = serde_json::from_str(&steps).unwrap_or(serde_json::Value::Null);
    let prepared = match prepare(&workout, athlete, sport) {
        Ok(p) => p,
        Err(m) => return Ok(Err(m)),
    };
    let mut next: Vec<serde_json::Value> = alts.iter().filter(|a| *a != chosen).cloned().collect();
    if let Some(old) = p.workout_id {
        next.push(alternative_snapshot(old, &p.name, &p.steps_json, athlete, None));
    }
    sqlx::query("UPDATE planned_workouts SET name = ?, steps_json = ?, workout_id = ?, sport = ?, updated_at = datetime('now') WHERE id = ?")
        .bind(&prepared.name)
        .bind(&prepared.steps_json)
        .bind(new_workout_id)
        .bind(sport.as_str())
        .bind(id)
        .execute(pool)
        .await?;
    set_alternatives(pool, id, &next).await?;
    Ok(Ok(()))
}

pub async fn find_by_idempotency_key(pool: &SqlitePool, key: &str) -> anyhow::Result<Option<Planned>> {
    Ok(sqlx::query_as(&format!("SELECT {COLUMNS} FROM planned_workouts WHERE idempotency_key = ?"))
    .bind(key)
    .fetch_optional(pool)
    .await?)
}

pub async fn delete(pool: &SqlitePool, id: i64) -> anyhow::Result<bool> {
    let result = sqlx::query("DELETE FROM planned_workouts WHERE id = ?")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(result.rows_affected() > 0)
}

pub async fn get(pool: &SqlitePool, id: i64) -> anyhow::Result<Option<Planned>> {
    Ok(sqlx::query_as(&format!("SELECT {COLUMNS} FROM planned_workouts WHERE id = ?"))
    .bind(id)
    .fetch_optional(pool)
    .await?)
}

/// Planned sessions with `from <= date < to` (end exclusive), oldest first.
pub async fn list_between(pool: &SqlitePool, from: NaiveDate, to: NaiveDate) -> anyhow::Result<Vec<Planned>> {
    Ok(sqlx::query_as(&format!(
        "SELECT {COLUMNS} FROM planned_workouts WHERE planned_date >= ? AND planned_date < ? ORDER BY planned_date, id"
    ))
    .bind(from.to_string())
    .bind(to.to_string())
    .fetch_all(pool)
    .await?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    async fn test_pool() -> SqlitePool {
        let pool = sqlx::sqlite::SqlitePoolOptions::new().connect("sqlite::memory:").await.unwrap();
        sqlx::migrate!("./migrations").run(&pool).await.unwrap();
        pool
    }

    fn athlete() -> Athlete {
        Athlete { ftp_watts: 205.0, hr_rest: 53.0, hr_max: 192.0, run_threshold_pace_s: 0.0 }
    }

    fn sample() -> Value {
        json!({"name": "Seuil 2x10", "steps": [
            {"type": "warmup", "duration": {"type": "time", "value": "10min"}, "target": {"type": "power_range", "low": 120, "high": 150}},
            {"type": "repeat", "count": 2, "steps": [
                {"type": "interval", "duration": {"type": "time", "value": "10min"}, "target": {"type": "power_range", "low": 195, "high": 205}},
                {"type": "recovery", "duration": {"type": "time", "value": "5min"}, "target": {"type": "power_range", "low": 100, "high": 120}}
            ]}
        ]})
    }

    #[test]
    fn free_ride_budget_is_bounded_by_the_day_and_the_intent() {
        // 120 min of endurance (IF cap 0.75): ceiling 112 TSS. Default range = 60 %..100 %.
        let b = free_ride_budget(None, 120, "endurance", 180, None).unwrap();
        assert_eq!((b.tss_min, b.tss_max), (68.0, 113.0));
        assert!(free_ride_budget(Some((40.0, 100.0)), 120, "endurance", 180, None).is_ok());
        assert!(free_ride_budget(Some((40.0, 200.0)), 120, "endurance", 180, None).unwrap_err().contains("out of reach"));
        assert!(free_ride_budget(Some((90.0, 50.0)), 120, "endurance", 180, None).is_err(), "min > max");
        assert!(free_ride_budget(None, 120, "endurance", 90, None).unwrap_err().contains("available"));
        assert!(free_ride_budget(None, 10, "endurance", 90, None).is_err(), "too short");
        assert!(free_ride_budget(None, 60, "sprint", 90, None).unwrap_err().contains("intent"));
        // A race declares its own intensity: 90 min at IF 0.9 is ~121 TSS, centred in the budget.
        let race = free_ride_budget(None, 90, "course", 120, Some(0.9)).unwrap();
        assert_eq!((race.tss_min, race.tss_max), (109.0, 134.0));
        assert!(crate::rules::intensity_factor(90.0, (race.tss_min + race.tss_max) / 2.0).unwrap() > 0.89);
        assert!(free_ride_budget(Some((40.0, 80.0)), 90, "course", 120, Some(0.9)).unwrap_err().contains("not both"));
        assert!(free_ride_budget(None, 90, "course", 120, Some(1.5)).unwrap_err().contains("expected_if"));
        assert!(free_ride_budget(None, 90, "course", 120, None).is_ok(), "course now has its own ceiling");
        // Recovery is capped lower than hilly rides for the same duration.
        assert!(tss_ceiling("recuperation", 60) < tss_ceiling("denivele", 60));
    }

    #[tokio::test]
    async fn free_rides_count_at_mid_budget_and_complete_with_the_ride() {
        let pool = test_pool().await;
        let day = parse_date("2026-11-10").unwrap();
        let budget = free_ride_budget(Some((40.0, 80.0)), 120, "endurance", 180, None).unwrap();
        let free = insert_free_ride(&pool, day, &budget, "api").await.unwrap();
        let p = get(&pool, free).await.unwrap().unwrap();
        assert!(p.is_free_ride());
        assert_eq!(p.load_estimate(&athlete()), (120.0, 60.0));

        // No ride yet: stays upcoming, and a later day never completes it.
        assert_eq!(reconcile(&pool, day).await.unwrap(), 0);
        sqlx::query("INSERT INTO activities (id, started_at, duration_s, distance_m, elevation_gain_m, sport_type) VALUES (1, '2026-11-10T17:00:00Z', 5400, 40000, 300, 'Ride')")
            .execute(&pool)
            .await
            .unwrap();
        // A run does not complete a cycling session.
        sqlx::query("INSERT INTO activities (id, started_at, duration_s, distance_m, elevation_gain_m, sport_type) VALUES (2, '2026-11-10T07:00:00Z', 3000, 8000, 50, 'Run')")
            .execute(&pool)
            .await
            .unwrap();
        // The day's ride is matched once the day has begun… but not before it.
        assert_eq!(reconcile(&pool, day - chrono::Duration::days(1)).await.unwrap(), 0);
        assert_eq!(reconcile(&pool, day).await.unwrap(), 1);
        let done = get(&pool, free).await.unwrap().unwrap();
        assert_eq!((done.status.as_str(), done.activity_id), ("done", Some(1)));
        // Idempotent, and a second session on the same day can't reuse the ride.
        let second = insert_free_ride(&pool, day, &budget, "api").await.unwrap();
        assert_eq!(reconcile(&pool, day).await.unwrap(), 0);
        assert_eq!(get(&pool, second).await.unwrap().unwrap().status, "planned");
    }

    #[test]
    fn sport_parses_known_values_only_and_distance_is_cycling_only() {
        assert_eq!(Sport::parse("running"), Some(Sport::Running));
        assert_eq!(Sport::parse(" cycling "), Some(Sport::Cycling));
        assert_eq!(Sport::parse("swimming"), None);
        assert_eq!(Sport::from_db("???"), Sport::Cycling);

        let ride = prepare(&sample(), &athlete(), Sport::Cycling).unwrap();
        let run = prepare(&sample(), &athlete(), Sport::Running).unwrap();
        assert!(ride.steps_json.contains("estimated_distance_km"));
        assert!(!run.steps_json.contains("estimated_distance_km"));
    }

    #[test]
    fn prepare_validates_and_adds_distance_estimate() {
        let p = prepare(&sample(), &athlete(), Sport::Cycling).unwrap();
        let v: Value = serde_json::from_str(&p.steps_json).unwrap();
        assert_eq!(p.name, "Seuil 2x10");
        assert!(v["estimated_distance_km"].as_f64().unwrap() > 0.0);

        assert!(prepare(&json!({"name": "", "steps": []}), &athlete(), Sport::Cycling).is_err());
        assert!(prepare(&json!({"name": "x", "steps": []}), &athlete(), Sport::Cycling).unwrap_err().contains("au moins une étape"));
        assert!(prepare(&json!({"nope": 1}), &athlete(), Sport::Cycling).unwrap_err().contains("JSON invalide"));
    }

    #[tokio::test]
    async fn planned_session_is_independent_of_its_template() {
        let pool = test_pool().await;
        let prepared = prepare(&sample(), &athlete(), Sport::Cycling).unwrap();
        let tpl = sqlx::query("INSERT INTO workouts (name, steps_json) VALUES (?, ?)")
            .bind(&prepared.name)
            .bind(&prepared.steps_json)
            .execute(&pool)
            .await
            .unwrap()
            .last_insert_rowid();
        let day = parse_date("2026-10-12").unwrap();
        let id = insert(&pool, day, &prepared, Some("  jambes lourdes ? "), Some(tpl), "manual", Sport::Cycling).await.unwrap();
        // A second, template-less session on the same day is fine.
        insert(&pool, day, &prepared, None, None, "api", Sport::Running).await.unwrap();

        // Editing / deleting the template leaves the planned copy alone.
        sqlx::query("UPDATE workouts SET name = 'renamed', steps_json = '{}' WHERE id = ?").bind(tpl).execute(&pool).await.unwrap();
        sqlx::query("DELETE FROM workouts WHERE id = ?").bind(tpl).execute(&pool).await.unwrap();

        let planned = get(&pool, id).await.unwrap().unwrap();
        assert_eq!(planned.name, "Seuil 2x10");
        assert_eq!(planned.workout_id, None, "link cleared when the template is deleted");
        assert_eq!(planned.notes.as_deref(), Some("jambes lourdes ?"));
        assert!(planned.steps_json.contains("interval"));

        let in_window = list_between(&pool, parse_date("2026-10-12").unwrap(), parse_date("2026-10-13").unwrap()).await.unwrap();
        assert_eq!(in_window.len(), 2);
        let outside = list_between(&pool, parse_date("2026-10-13").unwrap(), parse_date("2026-10-20").unwrap()).await.unwrap();
        assert!(outside.is_empty());

        assert_eq!(in_window.iter().map(|p| p.sport.as_str()).collect::<Vec<_>>(), vec!["cycling", "running"]);

        assert!(delete(&pool, id).await.unwrap());
        assert!(!delete(&pool, id).await.unwrap());
    }
}
