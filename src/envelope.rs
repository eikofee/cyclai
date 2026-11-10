//! Ride envelope: for a day and the time available, the kinds of ride that fit
//! (`tranquille`, `moyenne`, `longue_denivele`) and the TSS each typically
//! costs for this athlete, learned from the ride history.

/// A past ride, reduced to what the envelope needs.
#[derive(Debug, Clone, Copy)]
pub struct HistoricRide {
    pub duration_min: f64,
    pub tss: f64,
    pub elevation_gain_m: f64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Scenario {
    pub name: &'static str,
    /// Intent bounding the TSS ceiling (see `planned::tss_ceiling`).
    pub intent: &'static str,
    pub duration_min: i64,
    /// Rides of the history count towards this scenario only if they climb this much per hour.
    pub min_elevation_m_per_h: f64,
}

/// Rides closer than this to a scenario's duration (in proportion) are its sample.
const DURATION_TOLERANCE: f64 = 0.25;
/// Fewer rides than this and the range comes from the engine's defaults instead.
pub const MIN_SAMPLE: usize = 3;

/// Slot kind from the time available, for display: `soiree_courte` (up to
/// 90 min), `soiree_longue` (up to 150 min), `journee` beyond.
pub fn slot_type(available_min: i64) -> &'static str {
    match available_min {
        ..=90 => "soiree_courte",
        91..=150 => "soiree_longue",
        _ => "journee",
    }
}

/// Scenarios compatible with `available_min`: an easy ride half the time, a
/// regular one at 80 %, and a long hilly one that needs a long slot (2 h 30).
pub fn scenarios(available_min: i64) -> Vec<Scenario> {
    let round5 = |v: f64| ((v / 5.0).round() as i64) * 5;
    let mut out = Vec::new();
    if available_min >= 30 {
        out.push(Scenario { name: "tranquille", intent: "endurance", duration_min: round5(available_min as f64 * 0.5).max(20), min_elevation_m_per_h: 0.0 });
    }
    if available_min >= 60 {
        out.push(Scenario { name: "moyenne", intent: "endurance", duration_min: round5(available_min as f64 * 0.8), min_elevation_m_per_h: 0.0 });
    }
    if available_min >= 150 {
        out.push(Scenario { name: "longue_denivele", intent: "denivele", duration_min: available_min, min_elevation_m_per_h: 250.0 });
    }
    out
}

fn quantile(sorted: &[f64], q: f64) -> f64 {
    let pos = q * (sorted.len() - 1) as f64;
    let (lo, hi) = (pos.floor() as usize, pos.ceil() as usize);
    sorted[lo] + (sorted[hi] - sorted[lo]) * (pos - lo as f64)
}

#[derive(Debug, Clone, PartialEq)]
pub struct TssRange {
    pub lo: f64,
    pub hi: f64,
    /// `history` (computed from rides) or `engine_default` (estimated).
    pub source: &'static str,
    pub sample_size: usize,
}

/// Typical TSS of the scenario: the middle half (25th–75th percentile) of the
/// history rides of about the same duration (and climbing, for hilly
/// scenarios), clipped to what the intent allows. Without enough rides, 60 %
/// to 100 % of that ceiling.
pub fn tss_range(history: &[HistoricRide], s: &Scenario) -> TssRange {
    let ceiling = crate::planned::tss_ceiling(s.intent, s.duration_min);
    let target = s.duration_min as f64;
    let mut sample: Vec<f64> = history
        .iter()
        .filter(|r| (r.duration_min - target).abs() <= target * DURATION_TOLERANCE)
        .filter(|r| r.duration_min > 0.0 && r.elevation_gain_m / (r.duration_min / 60.0) >= s.min_elevation_m_per_h)
        .map(|r| r.tss)
        .collect();
    if sample.len() < MIN_SAMPLE {
        return TssRange { lo: (ceiling * 0.6).round(), hi: ceiling.round(), source: "engine_default", sample_size: sample.len() };
    }
    sample.sort_by(|a, b| a.total_cmp(b));
    let hi = quantile(&sample, 0.75).min(ceiling.ceil()).round();
    let lo = quantile(&sample, 0.25).min(hi).round();
    TssRange { lo, hi, source: "history", sample_size: sample.len() }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ride(duration_min: f64, tss: f64, elevation_gain_m: f64) -> HistoricRide {
        HistoricRide { duration_min, tss, elevation_gain_m }
    }

    #[test]
    fn scenarios_depend_on_the_time_available() {
        assert!(scenarios(20).is_empty());
        assert_eq!(scenarios(45).iter().map(|s| s.name).collect::<Vec<_>>(), ["tranquille"]);
        let evening = scenarios(90);
        assert_eq!(evening.iter().map(|s| (s.name, s.duration_min)).collect::<Vec<_>>(), [("tranquille", 45), ("moyenne", 70)]);
        let day = scenarios(240);
        assert_eq!(day.iter().map(|s| s.name).collect::<Vec<_>>(), ["tranquille", "moyenne", "longue_denivele"]);
        assert_eq!(day[2].duration_min, 240);
        assert_eq!(slot_type(60), "soiree_courte");
        assert_eq!(slot_type(120), "soiree_longue");
        assert_eq!(slot_type(180), "journee");
    }

    #[test]
    fn range_comes_from_similar_rides_when_there_are_enough() {
        let s = Scenario { name: "moyenne", intent: "endurance", duration_min: 100, min_elevation_m_per_h: 0.0 };
        // Four rides of about 100 min (TSS 50..80) and one far-off ride that must be ignored.
        let history = [ride(95.0, 50.0, 0.0), ride(100.0, 60.0, 0.0), ride(105.0, 70.0, 0.0), ride(110.0, 80.0, 0.0), ride(300.0, 250.0, 0.0)];
        let r = tss_range(&history, &s);
        assert_eq!((r.source, r.sample_size), ("history", 4));
        assert_eq!((r.lo, r.hi), (58.0, 73.0)); // quartiles of 50, 60, 70, 80
    }

    #[test]
    fn range_falls_back_to_the_engine_and_never_exceeds_the_intent_ceiling() {
        let s = Scenario { name: "tranquille", intent: "endurance", duration_min: 60, min_elevation_m_per_h: 0.0 };
        let few = tss_range(&[ride(60.0, 40.0, 0.0)], &s);
        assert_eq!((few.source, few.sample_size, few.lo, few.hi), ("engine_default", 1, 34.0, 56.0));
        // Past rides that were much harder than the intent allows are clipped.
        let hard = [ride(60.0, 90.0, 0.0), ride(60.0, 95.0, 0.0), ride(60.0, 100.0, 0.0)];
        let r = tss_range(&hard, &s);
        assert_eq!(r.source, "history");
        assert!(r.hi <= 57.0 && r.lo <= r.hi, "{r:?}");
    }

    #[test]
    fn hilly_scenarios_only_learn_from_climbing_rides() {
        let s = Scenario { name: "longue_denivele", intent: "denivele", duration_min: 180, min_elevation_m_per_h: 250.0 };
        let flat = [ride(180.0, 100.0, 100.0), ride(180.0, 110.0, 120.0), ride(180.0, 105.0, 90.0)];
        assert_eq!(tss_range(&flat, &s).source, "engine_default", "flat rides don't count");
        let hilly = [ride(180.0, 120.0, 900.0), ride(175.0, 125.0, 1000.0), ride(185.0, 130.0, 850.0)];
        let r = tss_range(&hilly, &s);
        assert_eq!((r.source, r.sample_size), ("history", 3));
    }
}
