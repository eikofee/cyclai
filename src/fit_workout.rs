//! Export of a `Workout` to a `.fit` workout file (to copy to a watch or
//! bike computer's workouts folder, or to import in its companion app).
//!
//! The FIT workout format has no ramp step: a ramp becomes a power *range* from its lowest to
//! its highest point. Power targets stay in % of FTP when possible (the device
//! then follows the FTP set on the watch / head unit), or are converted to
//! watts with the app's FTP (`Mode::Watts`).

use crate::planned::Sport;
use crate::workout::{Athlete, SimpleStep, Target, Workout, WorkoutStep};
use crate::zwo::fracs;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Custom power ranges in % of the FTP configured on the device.
    PercentFtp,
    /// Custom power ranges in watts, from the FTP set in the app.
    Watts,
}

// FIT enums (profile 2.x).
const TYPE_WORKOUT: u8 = 5;
const DURATION_TIME: u8 = 0;
const DURATION_REPEAT_UNTIL_STEPS_CMPL: u8 = 6;
const TARGET_SPEED: u8 = 0;
const TARGET_HEART_RATE: u8 = 1;
const TARGET_OPEN: u8 = 2;
const TARGET_POWER: u8 = 4;
const INTENSITY_ACTIVE: u8 = 0;
const INTENSITY_WARMUP: u8 = 2;
const INTENSITY_COOLDOWN: u8 = 3;
const INTENSITY_RECOVERY: u8 = 4;
/// Seconds between the Unix epoch and the FIT epoch (1989-12-31 UTC).
const FIT_EPOCH_OFFSET: i64 = 631_065_600;
/// Custom power values above this are watts + 1000, below it % of FTP.
const POWER_WATTS_OFFSET: u32 = 1000;
/// Custom heart-rate values are bpm + 100.
const HR_BPM_OFFSET: u32 = 100;

struct FitStep {
    duration_type: u8,
    duration_value: u32,
    target_type: u8,
    target_value: u32,
    low: u32,
    high: u32,
    intensity: u8,
    notes: String,
}

pub fn to_fit(w: &Workout, a: &Athlete, sport: Sport, mode: Mode) -> Vec<u8> {
    encode(w, a, sport, mode, chrono::Utc::now().timestamp())
}

fn encode(w: &Workout, a: &Athlete, sport: Sport, mode: Mode, now_unix: i64) -> Vec<u8> {
    let mut steps = Vec::new();
    collect(&w.steps, a, mode, &mut steps);

    let mut data = Vec::new();
    // file_id (global 0)
    definition(&mut data, 0, 0, &[(0, 1, 0x00), (1, 2, 0x84), (2, 2, 0x84), (4, 4, 0x86)]);
    data.push(0);
    data.push(TYPE_WORKOUT);
    data.extend(255u16.to_le_bytes()); // manufacturer: development
    data.extend(0u16.to_le_bytes());
    data.extend(((now_unix - FIT_EPOCH_OFFSET).max(0) as u32).to_le_bytes());
    // workout (global 26)
    definition(&mut data, 1, 26, &[(4, 1, 0x00), (6, 2, 0x84), (8, 32, 0x07)]);
    data.push(1);
    data.push(match sport {
        Sport::Cycling => 2,
        Sport::Running => 1,
        Sport::Other => 0,
    });
    data.extend((steps.len() as u16).to_le_bytes());
    string_field(&mut data, &w.name, 32);
    // workout_step (global 27)
    definition(
        &mut data,
        2,
        27,
        &[
            (254, 2, 0x84),
            (1, 1, 0x00),
            (2, 4, 0x86),
            (3, 1, 0x00),
            (4, 4, 0x86),
            (5, 4, 0x86),
            (6, 4, 0x86),
            (7, 1, 0x00),
            (8, 32, 0x07),
        ],
    );
    for (i, s) in steps.iter().enumerate() {
        data.push(2);
        data.extend((i as u16).to_le_bytes());
        data.push(s.duration_type);
        data.extend(s.duration_value.to_le_bytes());
        data.push(s.target_type);
        data.extend(s.target_value.to_le_bytes());
        data.extend(s.low.to_le_bytes());
        data.extend(s.high.to_le_bytes());
        data.push(s.intensity);
        string_field(&mut data, &s.notes, 32);
    }

    let mut file = Vec::with_capacity(data.len() + 16);
    file.push(14); // header size
    file.push(0x20); // protocol 2.0
    file.extend(2132u16.to_le_bytes()); // profile version
    file.extend((data.len() as u32).to_le_bytes());
    file.extend(b".FIT");
    let header_crc = crc16(0, &file);
    file.extend(header_crc.to_le_bytes());
    file.extend(&data);
    // With a header CRC present, the trailer covers the data records only.
    file.extend(crc16(0, &data).to_le_bytes());
    file
}

/// Definition message: local type, global message number, (field, size, base type).
fn definition(out: &mut Vec<u8>, local: u8, global: u16, fields: &[(u8, u8, u8)]) {
    out.push(0x40 | local);
    out.push(0); // reserved
    out.push(0); // little endian
    out.extend(global.to_le_bytes());
    out.push(fields.len() as u8);
    for (num, size, base) in fields {
        out.extend([*num, *size, *base]);
    }
}

/// UTF-8 string cut on a char boundary to leave room for the NUL, zero-padded to `size`.
fn string_field(out: &mut Vec<u8>, s: &str, size: usize) {
    let mut end = s.len().min(size - 1);
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    out.extend(&s.as_bytes()[..end]);
    out.extend(std::iter::repeat(0).take(size - end));
}

fn crc16(mut crc: u16, bytes: &[u8]) -> u16 {
    const T: [u16; 16] = [
        0x0000, 0xCC01, 0xD801, 0x1400, 0xF001, 0x3C00, 0x2800, 0xE401, 0xA001, 0x6C00, 0x7800, 0xB401, 0x5000,
        0x9C01, 0x8801, 0x4400,
    ];
    for b in bytes {
        let tmp = T[(crc & 0xF) as usize];
        crc = ((crc >> 4) & 0x0FFF) ^ tmp ^ T[(b & 0xF) as usize];
        let tmp = T[(crc & 0xF) as usize];
        crc = ((crc >> 4) & 0x0FFF) ^ tmp ^ T[((b >> 4) & 0xF) as usize];
    }
    crc
}

fn collect(steps: &[WorkoutStep], a: &Athlete, mode: Mode, out: &mut Vec<FitStep>) {
    for step in steps {
        match step {
            WorkoutStep::Warmup(s) => out.push(leaf(s, INTENSITY_WARMUP, a, mode)),
            WorkoutStep::Interval(s) => out.push(leaf(s, INTENSITY_ACTIVE, a, mode)),
            WorkoutStep::Recovery(s) => out.push(leaf(s, INTENSITY_RECOVERY, a, mode)),
            WorkoutStep::Cooldown(s) => out.push(leaf(s, INTENSITY_COOLDOWN, a, mode)),
            WorkoutStep::Repeat { count, steps } => {
                let first = out.len() as u32;
                collect(steps, a, mode, out);
                out.push(FitStep {
                    duration_type: DURATION_REPEAT_UNTIL_STEPS_CMPL,
                    duration_value: first,
                    target_type: TARGET_OPEN,
                    target_value: *count,
                    low: 0,
                    high: 0,
                    intensity: INTENSITY_ACTIVE,
                    notes: String::new(),
                });
            }
        }
    }
}

fn leaf(s: &SimpleStep, intensity: u8, a: &Athlete, mode: Mode) -> FitStep {
    let seconds = (s.duration.minutes() * 60.0).round().max(1.0) as u32;
    let (target_type, target_value, low, high) = match &s.target {
        None => (TARGET_OPEN, 0, 0, 0),
        Some(Target::HrZone { zone }) => (TARGET_HEART_RATE, (*zone).clamp(1, 5) as u32, 0, 0),
        Some(Target::HrRange { low, high }) => {
            (TARGET_HEART_RATE, 0, low.round() as u32 + HR_BPM_OFFSET, high.round() as u32 + HR_BPM_OFFSET)
        }
        // Speed targets are in mm/s; the slow end of the pace range is the low speed.
        Some(Target::PaceRange { low, high }) => {
            (TARGET_SPEED, 0, (1_000_000.0 / high.max(1.0)).round() as u32, (1_000_000.0 / low.max(1.0)).round() as u32)
        }
        Some(_) => {
            let (lo, hi) = fracs(s, a);
            let conv = |f: f64| match mode {
                Mode::PercentFtp => (f * 100.0).round() as u32,
                Mode::Watts => (f * a.ftp_watts).round() as u32 + POWER_WATTS_OFFSET,
            };
            (TARGET_POWER, 0, conv(lo), conv(hi))
        }
    };
    FitStep {
        duration_type: DURATION_TIME,
        duration_value: seconds * 1000,
        target_type,
        target_value,
        low,
        high,
        intensity,
        notes: s.note.clone().unwrap_or_default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn athlete() -> Athlete {
        Athlete { ftp_watts: 200.0, hr_rest: 50.0, hr_max: 190.0, run_threshold_pace_s: 0.0 }
    }

    fn sample() -> Workout {
        serde_json::from_value(json!({"name": "Rampes & co", "steps": [
            {"type": "warmup", "duration": {"type": "time", "value": "10min"}, "target": {"type": "ftp_ramp", "from": 40, "to": 60}},
            {"type": "repeat", "count": 3, "steps": [
                {"type": "interval", "duration": {"type": "time", "value": "4min"}, "target": {"type": "ftp_pct_range", "low": 95, "high": 95}, "note": "90 rpm"},
                {"type": "recovery", "duration": {"type": "time", "value": "110s"}, "target": {"type": "power_range", "low": 100, "high": 120}}]},
            {"type": "cooldown", "duration": {"type": "time", "value": "5min"}, "target": {"type": "ftp_ramp", "from": 60, "to": 40}}
        ]}))
        .unwrap()
    }

    #[test]
    fn file_is_a_valid_fit_with_matching_crc() {
        let bytes = encode(&sample(), &athlete(), Sport::Cycling, Mode::PercentFtp, 1_800_000_000);
        assert_eq!(&bytes[8..12], b".FIT");
        assert_eq!(crc16(0, &bytes[..12]), u16::from_le_bytes([bytes[12], bytes[13]]));
        assert_eq!(crc16(0, &bytes[14..]), 0, "CRC over the data records, trailer included, is 0");
        let data_size = u32::from_le_bytes(bytes[4..8].try_into().unwrap()) as usize;
        assert_eq!(bytes.len(), 14 + data_size + 2);
    }

    #[test]
    fn parses_back_with_ramps_as_ranges_and_percent_ftp_kept() {
        let bytes = encode(&sample(), &athlete(), Sport::Cycling, Mode::PercentFtp, 1_800_000_000);
        let records = fitparser::from_bytes(&bytes).expect("fitparser must read our file");
        let steps: Vec<_> = records.iter().filter(|r| r.kind() == fitparser::profile::MesgNum::WorkoutStep).collect();
        // warmup + (interval, recovery, repeat) + cooldown
        assert_eq!(steps.len(), 5);
        let field = |r: &fitparser::FitDataRecord, name: &str| {
            r.fields().iter().find(|f| f.name() == name).map(|f| f.value().to_string())
        };
        // Ramp 40 -> 60 % FTP: range 40..60.
        assert_eq!(field(steps[0], "custom_target_power_low").as_deref(), Some("40"));
        assert_eq!(field(steps[0], "custom_target_power_high").as_deref(), Some("60"));
        // Watts (100-120 on a 200 W FTP) become 50-60 % FTP.
        assert_eq!(field(steps[2], "custom_target_power_low").as_deref(), Some("50"));
        // Descending ramp is a range too (low first).
        assert_eq!(field(steps[4], "custom_target_power_low").as_deref(), Some("40"));
        // The repeat step points back to the first step of its block.
        assert_eq!(field(steps[3], "duration_step").as_deref(), Some("1"));
        assert_eq!(field(steps[3], "repeat_steps").as_deref(), Some("3"));
    }

    #[test]
    fn watts_mode_adds_the_1000_offset() {
        let bytes = encode(&sample(), &athlete(), Sport::Cycling, Mode::Watts, 1_800_000_000);
        let records = fitparser::from_bytes(&bytes).unwrap();
        let first = records.iter().find(|r| r.kind() == fitparser::profile::MesgNum::WorkoutStep).unwrap();
        let get = |n: &str| first.fields().iter().find(|f| f.name() == n).unwrap().value().to_string();
        // 40 % and 60 % of 200 W, stored as watts + 1000.
        assert_eq!(get("custom_target_power_low"), "1080");
        assert_eq!(get("custom_target_power_high"), "1120");
    }
}
