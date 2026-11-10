//! Hard training constraints and load projection. Pure calculation (no LLM):
//! the server decides whether a plan is acceptable, an agent can only read the
//! verdict. Limits live in `athlete_config` and are editable in /settings.

use chrono::NaiveDate;
use serde::Serialize;
use sqlx::SqlitePool;
use std::collections::BTreeMap;

/// A day whose total TSS stays under this counts as a rest day.
pub const REST_TSS_MAX: f64 = 10.0;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rules {
    pub max_ctl_ramp_per_week: f64,
    pub min_tsb: f64,
    pub intense_if: f64,
    pub intense_tss: f64,
    pub max_consecutive_intense: i64,
    pub min_rest_days_per_week: i64,
}

impl Default for Rules {
    fn default() -> Self {
        Rules {
            max_ctl_ramp_per_week: 6.0,
            min_tsb: -25.0,
            intense_if: 0.85,
            intense_tss: 100.0,
            max_consecutive_intense: 2,
            min_rest_days_per_week: 1,
        }
    }
}

impl Rules {
    pub async fn load(pool: &SqlitePool) -> anyhow::Result<Rules> {
        let row: Option<(f64, f64, f64, f64, i64, i64)> = sqlx::query_as(
            "SELECT max_ctl_ramp_per_week, min_tsb, intense_if, intense_tss, max_consecutive_intense, \
             min_rest_days_per_week FROM athlete_config WHERE id = 1",
        )
        .fetch_optional(pool)
        .await?;
        Ok(row
            .map(|(r, t, i, s, c, d)| Rules {
                max_ctl_ramp_per_week: r,
                min_tsb: t,
                intense_if: i,
                intense_tss: s,
                max_consecutive_intense: c,
                min_rest_days_per_week: d,
            })
            .unwrap_or_default())
    }

    pub fn is_intense(&self, tss: f64, intensity_factor: Option<f64>) -> bool {
        tss >= self.intense_tss || intensity_factor.is_some_and(|f| f >= self.intense_if)
    }
}

/// One already-lived day (from `daily_load`), oldest first and contiguous.
#[derive(Debug, Clone)]
pub struct PastDay {
    pub date: NaiveDate,
    pub tss: f64,
    pub ctl: f64,
    pub atl: f64,
    pub intense: bool,
}

/// A session to add to the future (planned, or hypothetical).
#[derive(Debug, Clone)]
pub struct FutureSession {
    pub date: NaiveDate,
    pub tss: f64,
    pub intensity_factor: Option<f64>,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct ProjectedDay {
    pub date: NaiveDate,
    pub tss: f64,
    pub ctl: f64,
    pub atl: f64,
    pub tsb: f64,
    pub intense: bool,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Violation {
    /// `consecutive_intense` | `no_rest_day` | `ctl_ramp` | `tsb_floor`
    pub rule: &'static str,
    /// First day of the streak breaking the rule.
    pub date: NaiveDate,
    pub message: String,
}

#[derive(Debug, Serialize)]
pub struct Simulation {
    pub days: Vec<ProjectedDay>,
    pub violations: Vec<Violation>,
}

/// Day-by-day Banister projection (same constants as `metrics::recompute_daily_load`)
/// from the end of `past`, through the last future session, plus the rule
/// check over past + projected days. Violations are only reported for
/// projected days; the past is context (a 2-day intense streak already
/// behind you counts).
pub fn simulate(past: &[PastDay], future: &[FutureSession], rules: &Rules) -> Simulation {
    let Some(last) = past.last() else {
        return Simulation { days: vec![], violations: vec![] };
    };
    // Sessions dated on the last past day ("today") are added on top of what
    // was already ridden that day (Strava is assumed synced). The day is then
    // recomputed from the day before and counts as projected.
    let same_day = past.len() >= 2 && future.iter().any(|s| s.date == last.date);
    let (base, start, carry) = if same_day {
        (&past[..past.len() - 1], last.date, Some((last.tss, last.intense)))
    } else {
        (past, last.date + chrono::Duration::days(1), None)
    };
    let from = base.last().expect("base is never empty");
    let Some(end) = future.iter().map(|s| s.date).max().filter(|e| *e >= start) else {
        return Simulation { days: vec![], violations: vec![] };
    };

    let mut by_day: BTreeMap<NaiveDate, (f64, Option<f64>)> = BTreeMap::new();
    for s in future.iter().filter(|s| s.date >= start) {
        let e = by_day.entry(s.date).or_insert((0.0, None));
        e.0 += s.tss;
        e.1 = match (e.1, s.intensity_factor) {
            (Some(a), Some(b)) => Some(a.max(b)),
            (a, b) => a.or(b),
        };
    }

    let (mut ctl, mut atl) = (from.ctl, from.atl);
    let mut days = Vec::new();
    let mut day = start;
    while day <= end {
        let (mut tss, f) = by_day.get(&day).copied().unwrap_or((0.0, None));
        let mut intense_before = false;
        if let (Some((done, was_intense)), true) = (carry, day == start) {
            tss += done;
            intense_before = was_intense;
        }
        ctl += (tss - ctl) / 42.0;
        atl += (tss - atl) / 7.0;
        days.push(ProjectedDay {
            date: day,
            tss,
            ctl,
            atl,
            tsb: ctl - atl,
            intense: intense_before || rules.is_intense(tss, f),
        });
        day += chrono::Duration::days(1);
    }

    let violations = check(base, &days, rules);
    Simulation { days, violations }
}

/// Keeps the first index of each run of consecutive `true`s, so a rule that
/// stays broken for a week reports once, not seven times.
fn run_starts(flags: &[bool]) -> Vec<usize> {
    (0..flags.len()).filter(|&i| flags[i] && (i == 0 || !flags[i - 1])).collect()
}

fn check(past: &[PastDay], future: &[ProjectedDay], rules: &Rules) -> Vec<Violation> {
    let mut out = Vec::new();
    let first_future = past.len();

    // Combined series, as (date, tss, ctl, tsb, intense).
    let all: Vec<(NaiveDate, f64, f64, f64, bool)> = past
        .iter()
        .map(|d| (d.date, d.tss, d.ctl, d.ctl - d.atl, d.intense))
        .chain(future.iter().map(|d| (d.date, d.tss, d.ctl, d.tsb, d.intense)))
        .collect();

    // Consecutive intense days: flag the day that exceeds the limit, once per streak.
    let mut streak = 0;
    let mut flagged = false;
    for (i, d) in all.iter().enumerate() {
        streak = if d.4 { streak + 1 } else { 0 };
        if !d.4 {
            flagged = false;
        }
        if i >= first_future && streak > rules.max_consecutive_intense && !flagged {
            flagged = true;
            out.push(Violation {
                rule: "consecutive_intense",
                date: d.0,
                message: format!(
                    "{streak} jours intenses de suite (max {})",
                    rules.max_consecutive_intense
                ),
            });
        }
    }

    // At least `min_rest_days_per_week` rest days in every 7-day window.
    let broken: Vec<bool> = (0..all.len())
        .map(|i| {
            if i < first_future || i < 6 {
                return false;
            }
            let rest = all[i - 6..=i].iter().filter(|d| d.1 < REST_TSS_MAX).count() as i64;
            rest < rules.min_rest_days_per_week
        })
        .collect();
    for i in run_starts(&broken) {
        out.push(Violation {
            rule: "no_rest_day",
            date: all[i].0,
            message: format!("moins de {} jour(s) de repos sur 7 jours glissants", rules.min_rest_days_per_week),
        });
    }

    // CTL gain over 7 days.
    let ramp: Vec<bool> = (0..all.len())
        .map(|i| i >= first_future && i >= 7 && all[i].2 - all[i - 7].2 > rules.max_ctl_ramp_per_week)
        .collect();
    for i in run_starts(&ramp) {
        out.push(Violation {
            rule: "ctl_ramp",
            date: all[i].0,
            message: format!(
                "CTL +{:.1} en 7 jours (max +{:.1})",
                all[i].2 - all[i - 7].2,
                rules.max_ctl_ramp_per_week
            ),
        });
    }

    // TSB floor.
    let low: Vec<bool> = (0..all.len()).map(|i| i >= first_future && all[i].3 < rules.min_tsb).collect();
    for i in run_starts(&low) {
        out.push(Violation {
            rule: "tsb_floor",
            date: all[i].0,
            message: format!("TSB {:.1} sous le plancher de {:.1}", all[i].3, rules.min_tsb),
        });
    }

    out.sort_by_key(|v| v.date);
    out
}

/// Last `days` days of `daily_load` (topped up to today) with the intensity
/// flag derived from the activities of each day.
pub async fn load_past(pool: &SqlitePool, rules: &Rules, days: i64) -> anyhow::Result<Vec<PastDay>> {
    crate::metrics::extend_daily_load_to_today(pool).await?;
    // Sessions whose ride is now in the history are done, not upcoming.
    crate::planned::reconcile(pool, chrono::Utc::now().date_naive()).await?;
    let mut rows: Vec<(String, f64, f64, f64)> =
        sqlx::query_as("SELECT date, tss, ctl, atl FROM daily_load ORDER BY date DESC LIMIT ?")
            .bind(days)
            .fetch_all(pool)
            .await?;
    rows.reverse();
    let ifs: Vec<(String, Option<f64>)> =
        sqlx::query_as("SELECT date(started_at), MAX(intensity_factor) FROM activities GROUP BY date(started_at)")
            .fetch_all(pool)
            .await?;
    let ifs: BTreeMap<String, Option<f64>> = ifs.into_iter().collect();
    Ok(rows
        .into_iter()
        .filter_map(|(date, tss, ctl, atl)| {
            let f = ifs.get(&date).copied().flatten();
            Some(PastDay { date: NaiveDate::parse_from_str(&date, "%Y-%m-%d").ok()?, tss, ctl, atl, intense: rules.is_intense(tss, f) })
        })
        .collect())
}

/// IF of a session from its duration and TSS (TSS = hours × IF² × 100).
pub fn intensity_factor(minutes: f64, tss: f64) -> Option<f64> {
    (minutes > 0.0 && tss > 0.0).then(|| (tss / (minutes / 60.0) / 100.0).sqrt())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(n: i64) -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, 11, 1).unwrap() + chrono::Duration::days(n)
    }

    fn rules() -> Rules {
        Rules::default()
    }

    /// 14 easy past days at a steady state (CTL=ATL=40, ~40 TSS a day).
    fn steady_past() -> Vec<PastDay> {
        (-13..=0).map(|i| PastDay { date: d(i), tss: 40.0, ctl: 40.0, atl: 40.0, intense: false }).collect()
    }

    fn session(day: i64, tss: f64, f: f64) -> FutureSession {
        FutureSession { date: d(day), tss, intensity_factor: Some(f) }
    }

    #[test]
    fn rest_projection_matches_the_banister_decay() {
        let sim = simulate(&steady_past(), &[session(3, 0.0, 0.0)], &rules());
        assert_eq!(sim.days.len(), 3);
        let first = &sim.days[0];
        assert!((first.ctl - 40.0 * (1.0 - 1.0 / 42.0)).abs() < 1e-9);
        assert!((first.atl - 40.0 * (1.0 - 1.0 / 7.0)).abs() < 1e-9);
    }

    #[test]
    fn three_intense_days_in_a_row_are_flagged_once() {
        let future = [session(1, 110.0, 0.9), session(2, 110.0, 0.9), session(3, 110.0, 0.9), session(4, 120.0, 0.9)];
        let v = simulate(&steady_past(), &future, &rules()).violations;
        let streaks: Vec<_> = v.iter().filter(|v| v.rule == "consecutive_intense").collect();
        assert_eq!(streaks.len(), 1);
        assert_eq!(streaks[0].date, d(3));
    }

    #[test]
    fn intense_days_already_behind_count() {
        let mut past = steady_past();
        let n = past.len();
        past[n - 1].intense = true;
        past[n - 2].intense = true;
        let v = simulate(&past, &[session(1, 110.0, 0.9)], &rules()).violations;
        assert!(v.iter().any(|v| v.rule == "consecutive_intense" && v.date == d(1)));
    }

    #[test]
    fn a_week_without_rest_is_flagged() {
        // The past is all 40-TSS days, so any 7-day window without a rest day breaks the rule.
        let future = [session(1, 40.0, 0.6), session(2, 40.0, 0.6)];
        let v = simulate(&steady_past(), &future, &rules()).violations;
        let rest: Vec<_> = v.iter().filter(|v| v.rule == "no_rest_day").collect();
        assert_eq!(rest.len(), 1, "one violation per streak");
        assert_eq!(rest[0].date, d(1));
    }

    #[test]
    fn a_rest_day_in_the_window_satisfies_the_rule() {
        let mut past = steady_past();
        let n = past.len();
        past[n - 3].tss = 0.0;
        let v = simulate(&past, &[session(1, 40.0, 0.6)], &rules()).violations;
        assert!(!v.iter().any(|v| v.rule == "no_rest_day"));
    }

    #[test]
    fn big_block_breaks_ramp_and_tsb_floor() {
        let future: Vec<_> = (1..=7).map(|i| session(i, 220.0, 0.8)).collect();
        let v = simulate(&steady_past(), &future, &rules()).violations;
        assert!(v.iter().any(|v| v.rule == "ctl_ramp"));
        assert!(v.iter().any(|v| v.rule == "tsb_floor"));
    }

    #[test]
    fn rules_are_configurable() {
        let future = [session(1, 110.0, 0.9), session(2, 110.0, 0.9), session(3, 110.0, 0.9)];
        let lax = Rules { max_consecutive_intense: 3, ..rules() };
        assert!(!simulate(&steady_past(), &future, &lax).violations.iter().any(|v| v.rule == "consecutive_intense"));
    }

    #[test]
    fn nothing_to_project_is_empty() {
        assert!(simulate(&steady_past(), &[], &rules()).days.is_empty());
        assert!(simulate(&steady_past(), &[session(-1, 50.0, 0.7)], &rules()).days.is_empty(), "past days are not projected");
    }

    #[test]
    fn a_session_today_adds_to_what_was_already_ridden() {
        // Today already has 40 TSS; a 60-TSS session brings the day to 100.
        let sim = simulate(&steady_past(), &[session(0, 60.0, 0.7)], &rules());
        assert_eq!(sim.days.len(), 1);
        let today = &sim.days[0];
        assert_eq!(today.date, d(0));
        assert!((today.tss - 100.0).abs() < 1e-9);
        assert!((today.ctl - (40.0 + 60.0 / 42.0)).abs() < 1e-9);
        assert!(today.intense, "100 TSS reaches the intense threshold");
    }

    #[test]
    fn an_intense_ride_already_done_today_stays_intense() {
        let mut past = steady_past();
        let n = past.len();
        past[n - 1].intense = true;
        past[n - 2].intense = true;
        // Two intense days behind (yesterday, today); an easy extra today keeps today intense,
        // so tomorrow's intense session is the 3rd in a row.
        let v = simulate(&past, &[session(0, 10.0, 0.5), session(1, 110.0, 0.9)], &rules()).violations;
        assert!(v.iter().any(|v| v.rule == "consecutive_intense" && v.date == d(1)));
    }

    #[test]
    fn intensity_factor_from_tss() {
        assert!((intensity_factor(60.0, 100.0).unwrap() - 1.0).abs() < 1e-9);
        assert!((intensity_factor(120.0, 100.0).unwrap() - 0.7071).abs() < 1e-3);
        assert!(intensity_factor(0.0, 50.0).is_none());
    }
}
