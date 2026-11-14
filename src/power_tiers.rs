//! Power profile tiers, in the spirit of Strava's: a best power over a
//! duration, in W/kg, is placed on an eight-step scale (En herbe → Classe
//! mondiale). The thresholds are approximate values in the style of Coggan's
//! power profile table, given at four anchor durations and interpolated on
//! the log of the duration in between.

pub const TIER_NAMES: [&str; 8] = ["En herbe", "Intermédiaire", "Athlétique", "Sport", "Elite", "Semi-pro", "Classe nationale", "Classe mondiale"];

/// Anchor durations (s) and, for each, the W/kg at which tiers 1..=7 begin
/// (tier 0, "En herbe", is everything below the first).
const ANCHORS: [(f64, [f64; 7]); 4] = [
    (5.0, [11.0, 13.0, 15.0, 17.0, 19.5, 22.0, 24.0]),
    (60.0, [5.0, 6.0, 7.0, 8.0, 9.0, 10.0, 11.5]),
    (300.0, [2.6, 3.2, 3.9, 4.6, 5.4, 6.4, 7.4]),
    (3600.0, [2.0, 2.6, 3.2, 3.8, 4.4, 5.2, 6.0]),
];

fn thresholds(secs: f64) -> [f64; 7] {
    let s = secs.clamp(ANCHORS[0].0, ANCHORS[3].0);
    let i = (0..3).find(|i| s <= ANCHORS[i + 1].0).unwrap_or(2);
    let ((s0, lo), (s1, hi)) = (ANCHORS[i], ANCHORS[i + 1]);
    let t = (s.ln() - s0.ln()) / (s1.ln() - s0.ln());
    std::array::from_fn(|k| lo[k] + (hi[k] - lo[k]) * t)
}

/// Position on the 0..8 scale: tier `k` spans `k..k+1`, so the integer part
/// is the tier and the fraction how far through it the value is.
pub fn score(secs: usize, watts: f64, weight_kg: f64) -> f64 {
    if weight_kg <= 0.0 || watts <= 0.0 {
        return 0.0;
    }
    let wkg = watts / weight_kg;
    let b = thresholds(secs as f64);
    if wkg < b[0] {
        return wkg / b[0];
    }
    for k in 1..7 {
        if wkg < b[k] {
            return k as f64 + (wkg - b[k - 1]) / (b[k] - b[k - 1]);
        }
    }
    // World class: up to 15 % above its threshold fills the last step.
    7.0 + ((wkg - b[6]) / (b[6] * 0.15)).clamp(0.0, 1.0)
}

pub fn tier_name(score: f64) -> &'static str {
    TIER_NAMES[(score.floor().max(0.0) as usize).min(7)]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tiers_follow_w_per_kg() {
        // 5 min at 4.6 W/kg (70 kg → 322 W) is exactly where "Elite" begins.
        let s = score(300, 322.0, 70.0);
        assert_eq!(tier_name(s), "Elite");
        assert!((s - 4.0).abs() < 0.01, "{s}");
        assert_eq!(tier_name(score(300, 140.0, 70.0)), "En herbe");
        assert_eq!(tier_name(score(300, 700.0, 70.0)), "Classe mondiale");
        assert!(score(300, 700.0, 70.0) <= 8.0);
    }

    #[test]
    fn in_between_durations_interpolate_and_no_weight_gives_nothing() {
        let (a, b) = (thresholds(5.0)[3], thresholds(60.0)[3]);
        let mid = thresholds(15.0)[3];
        assert!(mid < a && mid > b);
        assert_eq!(score(300, 300.0, 0.0), 0.0);
    }
}
