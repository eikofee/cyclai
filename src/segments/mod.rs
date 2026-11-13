use sqlx::SqlitePool;

/// Distance under which a GPS point counts as "at" a segment endpoint.
/// Consumer cycling-computer GPS is accurate to a few meters in the open,
/// but tree cover / urban canyon can push errors past 20m — 15m is tight
/// enough to avoid cross-matching nearby segments.
const MATCH_THRESHOLD_M: f64 = 15.0;

/// Smallest distance between a segment's start and end: the detection looks
/// for a ride passing near both ends, so they must be told apart.
const MIN_SEGMENT_ENDS_M: f64 = 2.0 * MATCH_THRESHOLD_M;
const MIN_SEGMENT_LENGTH_M: f64 = 100.0;

/// The segment's track as stored: streams-shaped JSON (`latlng`, `distance`,
/// `altitude`) with a point about every 5 m, so a long segment stays small.
fn segment_track(streams: &serde_json::Value) -> Option<String> {
    let latlng = latlng_array(streams)?;
    let altitude = f64_array(streams, "altitude").filter(|a| a.len() == latlng.len());
    let (mut pts, mut dist, mut alt): (Vec<[f64; 2]>, Vec<f64>, Vec<f64>) = (Vec::new(), Vec::new(), Vec::new());
    let mut total = 0.0;
    let mut since_kept = f64::MAX;
    for (i, p) in latlng.iter().enumerate() {
        if i > 0 {
            let step = crate::geo::haversine_m(latlng[i - 1][0], latlng[i - 1][1], p[0], p[1]);
            total += step;
            since_kept += step;
        }
        let last = i + 1 == latlng.len();
        if since_kept >= 5.0 || last {
            pts.push(*p);
            dist.push((total * 10.0).round() / 10.0);
            if let Some(a) = &altitude {
                alt.push((a[i] * 10.0).round() / 10.0);
            }
            since_kept = 0.0;
        }
    }
    if pts.len() < 2 {
        return None;
    }
    let mut out = serde_json::json!({ "latlng": { "data": pts }, "distance": { "data": dist } });
    if altitude.is_some() {
        out["altitude"] = serde_json::json!({ "data": alt });
    }
    Some(out.to_string())
}

/// Deletes a reference segment and every effort detected on it. `false` when
/// there is no such segment. Activities themselves are untouched.
pub async fn delete(pool: &SqlitePool, id: i64) -> anyhow::Result<bool> {
    let mut tx = pool.begin().await?;
    sqlx::query("DELETE FROM segment_efforts WHERE reference_segment_id = ?").bind(id).execute(&mut *tx).await?;
    let removed = sqlx::query("DELETE FROM reference_segments WHERE id = ?").bind(id).execute(&mut *tx).await?.rows_affected();
    tx.commit().await?;
    Ok(removed > 0)
}

/// Creates (or updates, by name) a reference segment from a GPX file: its
/// start and end are the first and last points of the track. Returns its id.
/// The detection then runs again over every stored ride.
pub async fn import_gpx(pool: &SqlitePool, name: &str, gpx: &[u8]) -> anyhow::Result<i64> {
    let name: String = name.chars().filter(|c| !c.is_control()).take(100).collect();
    let name = name.trim();
    anyhow::ensure!(!name.is_empty(), "le segment a besoin d'un nom");
    let parsed = crate::importer::parse_gpx(gpx)?;
    let points = parsed.streams_json["latlng"]["data"].as_array().cloned().unwrap_or_default();
    let point = |p: Option<&serde_json::Value>| -> Option<(f64, f64)> {
        let p = p?.as_array()?;
        Some((p.first()?.as_f64()?, p.get(1)?.as_f64()?))
    };
    let (Some((slat, slng)), Some((elat, elng))) = (point(points.first()), point(points.last())) else {
        anyhow::bail!("le GPX ne contient pas de trace exploitable");
    };
    anyhow::ensure!(
        parsed.distance_m >= MIN_SEGMENT_LENGTH_M,
        "segment trop court ({:.0} m) : il fait au moins {MIN_SEGMENT_LENGTH_M:.0} m",
        parsed.distance_m
    );
    anyhow::ensure!(
        crate::geo::haversine_m(slat, slng, elat, elng) >= MIN_SEGMENT_ENDS_M,
        "le départ et l'arrivée sont trop proches (boucle ?) : la détection a besoin de les distinguer, coupe la boucle en deux segments"
    );
    sqlx::query(
        r#"
        INSERT INTO reference_segments (name, start_lat, start_lng, end_lat, end_lng, distance_m, elevation_gain_m, track_json)
        VALUES (?, ?, ?, ?, ?, ?, ?, ?)
        ON CONFLICT (name) DO UPDATE SET
            start_lat = excluded.start_lat, start_lng = excluded.start_lng,
            end_lat = excluded.end_lat, end_lng = excluded.end_lng,
            distance_m = excluded.distance_m, elevation_gain_m = excluded.elevation_gain_m,
            track_json = excluded.track_json
        "#,
    )
    .bind(name)
    .bind(slat)
    .bind(slng)
    .bind(elat)
    .bind(elng)
    .bind(parsed.distance_m)
    .bind(parsed.elevation_gain_m)
    .bind(segment_track(&parsed.streams_json))
    .execute(pool)
    .await?;
    let id: i64 = sqlx::query_scalar("SELECT id FROM reference_segments WHERE name = ?").bind(name).fetch_one(pool).await?;

    // The sync skips matching when nothing new was imported, so match the
    // existing rides against the (possibly new) segment right away.
    run(pool).await?;
    Ok(id)
}

struct RefSegment {
    id: i64,
    start_lat: f64,
    start_lng: f64,
    end_lat: f64,
    end_lng: f64,
}

/// Finds the [start_idx, end_idx] index range of `latlng` that covers this
/// segment: the first point within threshold of the segment start, then the
/// first *subsequent* point within threshold of the segment end. Returns
/// None if the activity never passed through the segment.
fn match_segment(latlng: &[[f64; 2]], seg: &RefSegment) -> Option<(usize, usize)> {
    let start_idx = latlng
        .iter()
        .position(|p| crate::geo::haversine_m(p[0], p[1], seg.start_lat, seg.start_lng) < MATCH_THRESHOLD_M)?;
    let end_idx = latlng[start_idx..]
        .iter()
        .position(|p| crate::geo::haversine_m(p[0], p[1], seg.end_lat, seg.end_lng) < MATCH_THRESHOLD_M)
        .map(|i| start_idx + i)?;
    if end_idx > start_idx {
        Some((start_idx, end_idx))
    } else {
        None
    }
}

fn mean(values: &[f64]) -> Option<f64> {
    if values.is_empty() {
        None
    } else {
        Some(values.iter().sum::<f64>() / values.len() as f64)
    }
}

struct Portion {
    avg_power_w: Option<f64>,
    avg_speed_ms: Option<f64>,
    avg_hr: Option<f64>,
}

fn extract_portion(
    start_idx: usize,
    end_idx: usize,
    distance: &[f64],
    time: &[f64],
    watts: Option<&[f64]>,
    heartrate: Option<&[f64]>,
) -> Portion {
    let slice_avg = |data: Option<&[f64]>| {
        data.and_then(|d| d.get(start_idx..=end_idx)).and_then(mean)
    };
    let dt = time.get(end_idx).zip(time.get(start_idx)).map(|(e, s)| e - s);
    let dd = distance
        .get(end_idx)
        .zip(distance.get(start_idx))
        .map(|(e, s)| e - s);
    let avg_speed_ms = match (dd, dt) {
        (Some(dd), Some(dt)) if dt > 0.0 => Some(dd / dt),
        _ => None,
    };

    Portion {
        avg_power_w: slice_avg(watts),
        avg_speed_ms,
        avg_hr: slice_avg(heartrate),
    }
}

pub(crate) fn f64_array(v: &serde_json::Value, key: &str) -> Option<Vec<f64>> {
    v.get(key)?
        .get("data")?
        .as_array()?
        .iter()
        .map(|x| x.as_f64())
        .collect()
}

pub(crate) fn latlng_array(v: &serde_json::Value) -> Option<Vec<[f64; 2]>> {
    v.get("latlng")?
        .get("data")?
        .as_array()?
        .iter()
        .map(|pair| {
            let arr = pair.as_array()?;
            Some([arr.first()?.as_f64()?, arr.get(1)?.as_f64()?])
        })
        .collect()
}

/// An effort of a segment's history, as far as comparing the next one goes.
struct PastEffort {
    activity_id: i64,
    duration_s: f64,
    power_w: Option<f64>,
}

/// How an effort compares with a reference one: time difference in seconds
/// (negative = faster) and, when both have power, difference in watts.
fn delta(duration_s: f64, power_w: Option<f64>, reference: Option<(f64, Option<f64>)>) -> (Option<f64>, Option<f64>) {
    let Some((ref_s, ref_w)) = reference else { return (None, None) };
    (Some(duration_s - ref_s), power_w.zip(ref_w).map(|(p, r)| p - r))
}

/// "-5s (+13W)", or "-5s" when there is no power to compare; a minute or more
/// reads "-1:05". `None` without a time difference.
pub fn format_delta(time_s: Option<f64>, power_w: Option<f64>) -> Option<String> {
    let t = time_s?.round() as i64;
    let sign = if t < 0 { "-" } else if t > 0 { "+" } else { "" };
    let a = t.abs();
    let time = if a >= 60 { format!("{sign}{}:{:02}", a / 60, a % 60) } else { format!("{sign}{a}s") };
    Some(match power_w {
        Some(w) => format!("{time} ({:+.0}W)", w),
        None => time,
    })
}

/// Rebuilds `segment_efforts` from scratch, processing activities in
/// chronological order so each effort is compared with the history that
/// existed *before* it (a replay, not a batch aggregate). The record is the
/// fastest earlier effort; power is optional throughout.
pub async fn run(pool: &SqlitePool) -> anyhow::Result<()> {
    sqlx::query("DELETE FROM segment_efforts").execute(pool).await?;

    let ref_segments: Vec<(i64, f64, f64, f64, f64)> = sqlx::query_as(
        "SELECT id, start_lat, start_lng, end_lat, end_lng FROM reference_segments",
    )
    .fetch_all(pool)
    .await?;
    let ref_segments: Vec<RefSegment> = ref_segments
        .into_iter()
        .map(|(id, start_lat, start_lng, end_lat, end_lng)| RefSegment {
            id,
            start_lat,
            start_lng,
            end_lat,
            end_lng,
        })
        .collect();

    let activities: Vec<(i64, String)> = sqlx::query_as(
        r#"
        SELECT a.id, s.streams_json
        FROM activities a
        JOIN activity_streams s ON s.activity_id = a.id
        WHERE a.sport_type = 'Ride'
        ORDER BY a.started_at ASC
        "#,
    )
    .fetch_all(pool)
    .await?;

    for (activity_id, streams_json) in activities {
        let parsed: serde_json::Value = serde_json::from_str(&streams_json)?;
        let Some(latlng) = latlng_array(&parsed) else {
            continue;
        };
        let Some(distance) = f64_array(&parsed, "distance") else {
            continue;
        };
        let Some(time) = f64_array(&parsed, "time") else {
            continue;
        };
        let watts = f64_array(&parsed, "watts");
        let heartrate = f64_array(&parsed, "heartrate");

        for seg in &ref_segments {
            let Some((start_idx, end_idx)) = match_segment(&latlng, seg) else {
                continue;
            };
            let portion = extract_portion(
                start_idx,
                end_idx,
                &distance,
                &time,
                watts.as_deref(),
                heartrate.as_deref(),
            );
            let Some(duration_s) = time.get(end_idx).zip(time.get(start_idx)).map(|(e, s)| e - s).filter(|d| *d > 0.0) else {
                continue;
            };
            let power = portion.avg_power_w;

            let history: Vec<(i64, Option<f64>, Option<f64>, Option<f64>)> = sqlx::query_as(
                "SELECT activity_id, avg_power_w, start_time_s, end_time_s FROM segment_efforts \
                 WHERE reference_segment_id = ? ORDER BY id ASC",
            )
            .bind(seg.id)
            .fetch_all(pool)
            .await?;
            let history: Vec<PastEffort> = history
                .into_iter()
                .filter_map(|(activity_id, power_w, start, end)| {
                    let duration_s = end? - start?;
                    (duration_s > 0.0).then_some(PastEffort { activity_id, duration_s, power_w })
                })
                .collect();

            // Record: the fastest earlier effort (the first one on a tie).
            let pr = history.iter().fold(None::<&PastEffort>, |best, e| match best {
                Some(b) if b.duration_s <= e.duration_s => best,
                _ => Some(e),
            });
            let last = history.last();
            let recent = &history[history.len().saturating_sub(5)..];
            let recent_ref = mean(&recent.iter().map(|e| e.duration_s).collect::<Vec<_>>())
                .map(|d| (d, mean(&recent.iter().filter_map(|e| e.power_w).collect::<Vec<_>>())));
            let (pr_s, pr_w) = delta(duration_s, power, pr.map(|e| (e.duration_s, e.power_w)));
            let (recent_s, recent_w) = delta(duration_s, power, recent_ref);
            let (last_s, last_w) = delta(duration_s, power, last.map(|e| (e.duration_s, e.power_w)));

            sqlx::query(
                r#"
                INSERT INTO segment_efforts
                    (activity_id, reference_segment_id, avg_power_w, avg_speed_ms, avg_hr,
                     delta_vs_pr_s, delta_vs_pr_w, delta_vs_recent_s, delta_vs_recent_w,
                     delta_vs_last_s, delta_vs_last_w,
                     start_time_s, end_time_s, pr_activity_id, last_activity_id,
                     start_distance_m, end_distance_m)
                VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
                "#,
            )
            .bind(activity_id)
            .bind(seg.id)
            .bind(power)
            .bind(portion.avg_speed_ms)
            .bind(portion.avg_hr)
            .bind(pr_s)
            .bind(pr_w)
            .bind(recent_s)
            .bind(recent_w)
            .bind(last_s)
            .bind(last_w)
            .bind(time.get(start_idx).copied())
            .bind(time.get(end_idx).copied())
            .bind(pr.map(|e| e.activity_id))
            .bind(last.map(|e| e.activity_id))
            .bind(distance.get(start_idx).copied())
            .bind(distance.get(end_idx).copied())
            .execute(pool)
            .await?;
        }
    }

    Ok(())
}

/// Efforts stored before the deltas became absolute (time and watts) have none:
/// any effort after the first one of its segment always has a time delta, so a
/// missing one means the table predates that change and is rebuilt.
pub async fn refresh_if_stale(pool: &SqlitePool) -> anyhow::Result<()> {
    let stale: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM segment_efforts WHERE delta_vs_pr_s IS NULL \
         AND id > (SELECT MIN(id) FROM segment_efforts e2 WHERE e2.reference_segment_id = segment_efforts.reference_segment_id)",
    )
    .fetch_one(pool)
    .await?;
    if stale > 0 {
        run(pool).await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn haversine_known_distance() {
        // Roughly 1 degree of longitude at the equator ~= 111.2 km.
        let d = crate::geo::haversine_m(0.0, 0.0, 0.0, 1.0);
        assert!((d - 111_195.0).abs() < 200.0);
    }

    #[test]
    fn matches_segment_when_gps_passes_through() {
        let latlng = vec![
            [49.0, 6.0],   // far away
            [49.1000, 6.2000], // near start
            [49.1005, 6.2005], // mid
            [49.1010, 6.2010], // near end
            [49.2, 6.3],   // far away after
        ];
        let seg = RefSegment {
            id: 1,
            start_lat: 49.1000,
            start_lng: 6.2000,
            end_lat: 49.1010,
            end_lng: 6.2010,
        };
        let m = match_segment(&latlng, &seg);
        assert_eq!(m, Some((1, 3)));
    }

    #[test]
    fn no_match_when_never_near_segment() {
        let latlng = vec![[0.0, 0.0], [0.001, 0.001]];
        let seg = RefSegment {
            id: 1,
            start_lat: 49.0,
            start_lng: 6.0,
            end_lat: 49.01,
            end_lng: 6.01,
        };
        assert_eq!(match_segment(&latlng, &seg), None);
    }

    async fn test_pool() -> SqlitePool {
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .connect("sqlite::memory:")
            .await
            .unwrap();
        sqlx::migrate!("./migrations").run(&pool).await.unwrap();
        pool
    }

    fn segment_streams_json(latlng: [[f64; 2]; 3], end_time_s: f64, watts: Option<f64>) -> String {
        let mut v = serde_json::json!({
            "latlng": {"data": latlng},
            "distance": {"data": [0.0, 50.0, 100.0]},
            "time": {"data": [0.0, end_time_s / 2.0, end_time_s]},
        });
        if let Some(w) = watts {
            v["watts"] = serde_json::json!({"data": [w, w, w]});
        }
        v.to_string()
    }

    #[test]
    fn deltas_read_as_time_then_power() {
        assert_eq!(format_delta(Some(-5.0), Some(13.0)).as_deref(), Some("-5s (+13W)"));
        assert_eq!(format_delta(Some(-5.0), None).as_deref(), Some("-5s"));
        assert_eq!(format_delta(Some(7.0), Some(-4.0)).as_deref(), Some("+7s (-4W)"));
        assert_eq!(format_delta(Some(65.0), None).as_deref(), Some("+1:05"));
        assert_eq!(format_delta(Some(-125.0), Some(0.0)).as_deref(), Some("-2:05 (+0W)"));
        assert_eq!(format_delta(Some(0.2), None).as_deref(), Some("0s"));
        assert_eq!(format_delta(None, Some(5.0)), None);
    }

    #[tokio::test]
    async fn full_pipeline_compares_efforts_chronologically_in_seconds_and_watts() {
        let pool = test_pool().await;

        sqlx::query(
            "INSERT INTO reference_segments (name, start_lat, start_lng, end_lat, end_lng, distance_m, elevation_gain_m) \
             VALUES ('crux', 49.1000, 6.2000, 49.1010, 6.2010, 2420, 131)",
        )
        .execute(&pool)
        .await
        .unwrap();

        // Three rides on the same segment: 20 s at 200 W, then 15 s at 220 W,
        // then 12 s on a ride with no power meter.
        let latlng = [[49.1000, 6.2000], [49.1005, 6.2005], [49.1010, 6.2010]];
        for (id, started_at, secs, watts) in [
            (1i64, "2026-08-01T10:00:00Z", 20.0, Some(200.0)),
            (2i64, "2026-08-08T10:00:00Z", 15.0, Some(220.0)),
            (3i64, "2026-08-15T10:00:00Z", 12.0, None),
        ] {
            sqlx::query(
                "INSERT INTO activities (id, started_at, sport_type, duration_s, distance_m, elevation_gain_m) VALUES (?, ?, 'Ride', 20, 100, 0)",
            )
            .bind(id)
            .bind(started_at)
            .execute(&pool)
            .await
            .unwrap();
            sqlx::query("INSERT INTO activity_streams (activity_id, streams_json) VALUES (?, ?)")
                .bind(id)
                .bind(segment_streams_json(latlng, secs, watts))
                .execute(&pool)
                .await
                .unwrap();
        }

        run(&pool).await.unwrap();

        type Row = (i64, Option<f64>, Option<f64>, Option<f64>, Option<f64>, Option<f64>, Option<f64>, Option<f64>, Option<i64>);
        let rows: Vec<Row> = sqlx::query_as(
            "SELECT activity_id, avg_power_w, delta_vs_pr_s, delta_vs_pr_w, delta_vs_recent_s, delta_vs_last_s, delta_vs_last_w, \
                    delta_vs_recent_w, pr_activity_id \
             FROM segment_efforts ORDER BY activity_id",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(rows.len(), 3, "a ride without power is detected too");

        // First effort: nothing before it.
        assert_eq!(rows[0].2, None);
        assert_eq!(rows[0].8, None);

        // Second: 5 s faster than the first, +20 W.
        assert_eq!((rows[1].2, rows[1].3), (Some(-5.0), Some(20.0)));
        assert_eq!(rows[1].8, Some(1));

        // Third, no power: the record is now the second effort (15 s); the
        // recent average is (20 + 15) / 2 = 17.5 s; the last one is the second.
        // Its power stays empty and so do the watt differences.
        assert_eq!(rows[2].1, None);
        assert_eq!((rows[2].2, rows[2].3), (Some(-3.0), None));
        assert_eq!(rows[2].4, Some(12.0 - 17.5));
        assert_eq!((rows[2].5, rows[2].6, rows[2].7), (Some(-3.0), None, None));
        assert_eq!(rows[2].8, Some(2));
    }

    #[tokio::test]
    async fn efforts_stored_before_the_absolute_deltas_are_rebuilt_at_startup() {
        let pool = test_pool().await;
        sqlx::query(
            "INSERT INTO reference_segments (name, start_lat, start_lng, end_lat, end_lng, distance_m, elevation_gain_m) \
             VALUES ('crux', 49.1000, 6.2000, 49.1010, 6.2010, 100, 0)",
        )
        .execute(&pool)
        .await
        .unwrap();
        let latlng = [[49.1000, 6.2000], [49.1005, 6.2005], [49.1010, 6.2010]];
        for (id, started_at, secs) in [(1i64, "2026-08-01T10:00:00Z", 20.0), (2i64, "2026-08-08T10:00:00Z", 15.0)] {
            sqlx::query("INSERT INTO activities (id, started_at, sport_type, duration_s, distance_m, elevation_gain_m) VALUES (?, ?, 'Ride', 20, 100, 0)")
                .bind(id)
                .bind(started_at)
                .execute(&pool)
                .await
                .unwrap();
            sqlx::query("INSERT INTO activity_streams (activity_id, streams_json) VALUES (?, ?)")
                .bind(id)
                .bind(segment_streams_json(latlng, secs, None))
                .execute(&pool)
                .await
                .unwrap();
        }
        run(&pool).await.unwrap();
        // Simulate rows written by the previous version: efforts without the new deltas.
        sqlx::query("UPDATE segment_efforts SET delta_vs_pr_s = NULL, delta_vs_pr_w = NULL").execute(&pool).await.unwrap();

        refresh_if_stale(&pool).await.unwrap();
        let d: Option<f64> = sqlx::query_scalar("SELECT delta_vs_pr_s FROM segment_efforts WHERE activity_id = 2").fetch_one(&pool).await.unwrap();
        assert_eq!(d, Some(-5.0));
    }

    fn gpx(points: &[(f64, f64)]) -> Vec<u8> {
        let pts: String = points.iter().map(|(la, lo)| format!(r#"<trkpt lat="{la}" lon="{lo}"><ele>200</ele></trkpt>"#)).collect();
        format!(r#"<?xml version="1.0"?><gpx version="1.1" creator="t" xmlns="http://www.topografix.com/GPX/1/1"><trk><trkseg>{pts}</trkseg></trk></gpx>"#).into_bytes()
    }

    #[tokio::test]
    async fn a_segment_is_imported_from_a_gpx_track_and_updated_by_name() {
        let pool = sqlx::sqlite::SqlitePoolOptions::new().connect("sqlite::memory:").await.unwrap();
        crate::db::migrate(&pool).await.unwrap();
        // About 555 m northwards.
        let track: Vec<(f64, f64)> = (0..6).map(|i| (45.0 + i as f64 * 0.001, 5.0)).collect();
        let id = import_gpx(&pool, " Col du test ", &gpx(&track)).await.unwrap();
        let (name, slat, elat, dist): (String, f64, f64, f64) =
            sqlx::query_as("SELECT name, start_lat, end_lat, distance_m FROM reference_segments WHERE id = ?").bind(id).fetch_one(&pool).await.unwrap();
        assert_eq!(name, "Col du test");
        assert!((slat - 45.0).abs() < 1e-9 && (elat - 45.005).abs() < 1e-9);
        assert!((dist - 556.0).abs() < 10.0, "{dist}");
        // The track is kept (about a point every 5 m, with altitude) for the map and profile.
        let stored: String = sqlx::query_scalar("SELECT track_json FROM reference_segments WHERE id = ?").bind(id).fetch_one(&pool).await.unwrap();
        let t: serde_json::Value = serde_json::from_str(&stored).unwrap();
        let n = t["latlng"]["data"].as_array().unwrap().len();
        assert!((6..=120).contains(&n), "{n}");
        assert_eq!(t["altitude"]["data"].as_array().unwrap().len(), n);
        assert!((t["distance"]["data"].as_array().unwrap().last().unwrap().as_f64().unwrap() - 556.0).abs() < 10.0);

        // Same name: updated, not duplicated.
        let again = import_gpx(&pool, "Col du test", &gpx(&track[..4])).await.unwrap();
        assert_eq!(again, id);
        let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM reference_segments").fetch_one(&pool).await.unwrap();
        assert_eq!(n, 1);
    }

    #[tokio::test]
    async fn unusable_segment_tracks_are_refused_with_a_reason() {
        let pool = sqlx::sqlite::SqlitePoolOptions::new().connect("sqlite::memory:").await.unwrap();
        crate::db::migrate(&pool).await.unwrap();
        let short = import_gpx(&pool, "court", &gpx(&[(45.0, 5.0), (45.0003, 5.0)])).await.unwrap_err().to_string();
        assert!(short.contains("trop court"), "{short}");
        // A loop: starts and ends at the same place.
        let ring = [(45.0, 5.0), (45.002, 5.0), (45.002, 5.002), (45.0, 5.002), (45.0, 5.0)];
        let lp = import_gpx(&pool, "boucle", &gpx(&ring)).await.unwrap_err().to_string();
        assert!(lp.contains("trop proches"), "{lp}");
        assert!(import_gpx(&pool, "  ", &gpx(&[(45.0, 5.0), (45.01, 5.0)])).await.is_err(), "a name is required");
        assert!(import_gpx(&pool, "x", b"not a gpx").await.is_err());
        let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM reference_segments").fetch_one(&pool).await.unwrap();
        assert_eq!(n, 0, "nothing stored on refusal");
    }

    #[tokio::test]
    async fn deleting_a_segment_removes_its_efforts_and_nothing_else() {
        let pool = sqlx::sqlite::SqlitePoolOptions::new().connect("sqlite::memory:").await.unwrap();
        crate::db::migrate(&pool).await.unwrap();
        let pts: String = (0..=10)
            .map(|i| format!(r#"<trkpt lat="{}" lon="5.0"><ele>200</ele></trkpt>"#, 45.0 + i as f64 * 0.0009))
            .collect();
        let gpx = format!(r#"<?xml version="1.0"?><gpx version="1.1" creator="t" xmlns="http://www.topografix.com/GPX/1/1"><trk><trkseg>{pts}</trkseg></trk></gpx>"#);
        let a = import_gpx(&pool, "A", gpx.as_bytes()).await.unwrap();
        let b = import_gpx(&pool, "B", gpx.as_bytes()).await.unwrap();
        sqlx::query("INSERT INTO activities (id, started_at, sport_type, duration_s, distance_m, elevation_gain_m, title) VALUES (-1, '2026-11-10T08:00:00Z', 'Ride', 600, 1000, 0, 't')")
            .execute(&pool)
            .await
            .unwrap();
        for seg in [a, b] {
            sqlx::query("INSERT INTO segment_efforts (activity_id, reference_segment_id) VALUES (-1, ?)").bind(seg).execute(&pool).await.unwrap();
        }

        assert!(delete(&pool, a).await.unwrap());
        assert!(!delete(&pool, a).await.unwrap(), "already gone");
        let segments: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM reference_segments").fetch_one(&pool).await.unwrap();
        let efforts: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM segment_efforts WHERE reference_segment_id = ?").bind(b).fetch_one(&pool).await.unwrap();
        let activities: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM activities").fetch_one(&pool).await.unwrap();
        assert_eq!((segments, efforts, activities), (1, 1, 1));
    }
}
