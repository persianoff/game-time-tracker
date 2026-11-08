//! Pure aggregation of session records into local-day totals.

use crate::store::SessionRecord;
use crate::win32::{civil_from_days, days_from_civil};
use std::collections::BTreeMap;

/// Local calendar date (year, month, day).
pub type Date = (i32, u32, u32);

/// Active seconds per (local day, game).
pub type Daily = BTreeMap<(Date, String), u64>;

fn day_index(utc: u64, offset: &impl Fn(u64) -> i32) -> i64 {
    (utc as i64 + offset(utc) as i64).div_euclid(86400)
}

fn date_of(day: i64) -> Date {
    let (y, m, d) = civil_from_days(day);
    (y as i32, m, d)
}

/// First UTC instant after `cur` that falls on a later local day than `day`
/// (the local day containing `cur`). The offset is re-evaluated at the
/// candidate so a DST change between `cur` and midnight is respected.
fn next_boundary(cur: u64, day: i64, offset: &impl Fn(u64) -> i32) -> u64 {
    let local_midnight = (day + 1) * 86400;
    let mut b = cur as i64;
    for _ in 0..3 {
        b = local_midnight - offset(b.max(0) as u64) as i64;
    }
    let mut b = b.max(cur as i64 + 1) as u64;
    // Guard against pathological offset functions: never stay on `day`.
    if day_index(b, offset) <= day {
        let limit = cur + 2 * 86400;
        b = cur + 1;
        while day_index(b, offset) <= day && b < limit {
            b += 60;
        }
    }
    b
}

/// Split a session at local midnights. Active seconds are distributed in
/// proportion to wall-clock overlap with cumulative rounding, so the pieces
/// sum to exactly `active_s`. A zero-length session yields one piece on the
/// start day. Pieces are in chronological order, one per local day.
pub fn split_by_local_day(rec: &SessionRecord, offset: impl Fn(u64) -> i32) -> Vec<(Date, u64)> {
    let start = rec.start_utc;
    let end = rec.end_utc.max(start);
    let first_day = day_index(start, &offset);
    if end == start {
        return vec![(date_of(first_day), rec.active_s)];
    }
    let total = (end - start) as u128;
    let active = rec.active_s as u128;

    let mut out: Vec<(Date, u64)> = Vec::new();
    let mut cur = start;
    let mut prev_cum = 0u64;
    while cur < end {
        let day = day_index(cur, &offset);
        let next = next_boundary(cur, day, &offset).min(end);
        let cum = if next >= end {
            rec.active_s
        } else {
            ((active * (next - start) as u128 + total / 2) / total) as u64
        };
        let cum = cum.max(prev_cum);
        let piece = cum - prev_cum;
        prev_cum = cum;
        let date = date_of(day);
        match out.last_mut() {
            Some(last) if last.0 == date => last.1 += piece,
            _ => out.push((date, piece)),
        }
        cur = next;
    }
    out
}

/// Aggregate records into per-(day, game) active seconds, skipping zeros.
pub fn daily<'a>(
    records: impl IntoIterator<Item = &'a SessionRecord>,
    offset: impl Fn(u64) -> i32,
) -> Daily {
    let mut map = Daily::new();
    for rec in records {
        for (date, secs) in split_by_local_day(rec, &offset) {
            if secs > 0 {
                *map.entry((date, rec.game.clone())).or_insert(0) += secs;
            }
        }
    }
    map
}

/// `date` shifted by `delta` days.
pub fn add_days(date: Date, delta: i64) -> Date {
    date_of(days_from_civil(date.0 as i64, date.1, date.2) + delta)
}

/// Total and per-game seconds for one day, games sorted by seconds
/// descending then name.
pub fn day_summary(daily: &Daily, date: Date) -> (u64, Vec<(String, u64)>) {
    let mut games: Vec<(String, u64)> = daily
        .range((date, String::new())..)
        .take_while(|((d, _), _)| *d == date)
        .map(|((_, g), s)| (g.clone(), *s))
        .collect();
    games.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    (games.iter().map(|g| g.1).sum(), games)
}

/// The `n` days ending at `today` (inclusive), most recent first.
pub fn last_days(today: Date, n: u32) -> Vec<Date> {
    (0..n as i64).map(|i| add_days(today, -i)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(game: &str, start: u64, end: u64, active: u64) -> SessionRecord {
        SessionRecord {
            game: game.into(),
            source: "steam".into(),
            exe: "x.exe".into(),
            start_utc: start,
            end_utc: end,
            active_s: active,
        }
    }

    const DAY: u64 = 86400;
    // 2026-01-10 00:00 UTC
    const D0: u64 = 20463 * DAY;

    #[test]
    fn single_day() {
        let r = rec("A", D0 + 3600, D0 + 7200, 3000);
        assert_eq!(split_by_local_day(&r, |_| 0), vec![((2026, 1, 10), 3000)]);
    }

    #[test]
    fn zero_length_one_piece() {
        let r = rec("A", D0 + 5, D0 + 5, 0);
        assert_eq!(split_by_local_day(&r, |_| 7200), vec![((2026, 1, 10), 0)]);
        // end before start is treated as zero length
        let r = rec("A", D0 + 5, D0, 4);
        assert_eq!(split_by_local_day(&r, |_| 0), vec![((2026, 1, 10), 4)]);
    }

    #[test]
    fn crosses_one_midnight_fixed_offset() {
        // Local = UTC+2. Local midnight at D0+DAY-7200.
        let m = D0 + DAY - 7200;
        let r = rec("A", m - 3600, m + 3600, 7200);
        assert_eq!(
            split_by_local_day(&r, |_| 7200),
            vec![((2026, 1, 10), 3600), ((2026, 1, 11), 3600)]
        );
    }

    #[test]
    fn multi_midnight_and_sum_preserved() {
        let start = D0 + 20 * 3600;
        let end = D0 + 3 * DAY + 4 * 3600 + 17;
        let active = 100_001;
        let r = rec("A", start, end, active);
        let p = split_by_local_day(&r, |_| 0);
        let dates: Vec<_> = p.iter().map(|x| x.0).collect();
        assert_eq!(
            dates,
            vec![(2026, 1, 10), (2026, 1, 11), (2026, 1, 12), (2026, 1, 13)]
        );
        assert_eq!(p.iter().map(|x| x.1).sum::<u64>(), active);
        assert!(p[1].1 > p[0].1);
    }

    #[test]
    fn ending_exactly_at_midnight_has_no_next_day_piece() {
        let m = D0 + DAY;
        let r = rec("A", m - 600, m, 600);
        assert_eq!(split_by_local_day(&r, |_| 0), vec![((2026, 1, 10), 600)]);
    }

    #[test]
    fn starting_exactly_at_midnight() {
        let r = rec("A", D0 + DAY, D0 + DAY + 600, 600);
        assert_eq!(split_by_local_day(&r, |_| 0), vec![((2026, 1, 11), 600)]);
    }

    #[test]
    fn negative_offset() {
        // UTC-5: local midnight at D0 + DAY + 5h
        let m = D0 + DAY + 5 * 3600;
        let r = rec("A", m - 100, m + 300, 400);
        assert_eq!(
            split_by_local_day(&r, |_| -5 * 3600),
            vec![((2026, 1, 10), 100), ((2026, 1, 11), 300)]
        );
    }

    #[test]
    fn cumulative_rounding_sums_exactly() {
        let m = D0 + DAY;
        let r = rec("A", m - 1, m + 2, 2);
        let p = split_by_local_day(&r, |_| 0);
        assert_eq!(p.iter().map(|x| x.1).sum::<u64>(), 2);
        assert_eq!(p.len(), 2);
    }

    // Fall back: +2h before T = D0+23h UTC, +1h after. Local time goes from
    // 01:00 on Jan 11 back to 00:00, so local Jan 11 lasts 25 hours.
    fn fall_back(t: u64) -> i32 {
        if t < D0 + 23 * 3600 {
            7200
        } else {
            3600
        }
    }

    #[test]
    fn dst_fall_back_day_is_25_hours() {
        // Jan 11 local starts at D0+22h UTC (+2), ends at D0+DAY+23h UTC (+1).
        let start = D0 + 22 * 3600;
        let end = D0 + DAY + 23 * 3600;
        let wall = end - start; // 25h
        let r = rec("A", start - 3600, end + 3600, wall + 7200);
        let p = split_by_local_day(&r, fall_back);
        assert_eq!(
            p,
            vec![
                ((2026, 1, 10), 3600),
                ((2026, 1, 11), wall),
                ((2026, 1, 12), 3600)
            ]
        );
    }

    #[test]
    fn dst_fall_back_inside_repeated_hour_no_extra_piece() {
        let start = D0 + 22 * 3600 + 1800;
        let end = D0 + 23 * 3600 + 1800;
        let r = rec("A", start, end, 3600);
        assert_eq!(
            split_by_local_day(&r, fall_back),
            vec![((2026, 1, 11), 3600)]
        );
    }

    // Spring forward: +1h before T = D0+23h UTC, +2h after; local time jumps
    // 00:00 -> 01:00 on Jan 11.
    fn spring_forward(t: u64) -> i32 {
        if t < D0 + 23 * 3600 {
            3600
        } else {
            7200
        }
    }

    #[test]
    fn dst_spring_forward_gap_at_midnight() {
        let start = D0 + 22 * 3600; // Jan 10 23:00 local
        let end = D0 + DAY + 23 * 3600; // Jan 12 01:00 local
        let r = rec("A", start, end, end - start);
        let p = split_by_local_day(&r, spring_forward);
        let dates: Vec<_> = p.iter().map(|x| x.0).collect();
        assert_eq!(dates, vec![(2026, 1, 10), (2026, 1, 11), (2026, 1, 12)]);
        assert_eq!(p.iter().map(|x| x.1).sum::<u64>(), end - start);
        assert_eq!(p[0].1, 3600);
        assert_eq!(p[1].1, 23 * 3600);
        assert_eq!(p[2].1, 3600);
    }

    #[test]
    fn daily_aggregates_and_skips_zero() {
        let recs = vec![
            rec("A", D0 + 100, D0 + 200, 100),
            rec("A", D0 + 300, D0 + 400, 50),
            rec("B", D0 + 500, D0 + 600, 0),
            rec("B", D0 + DAY + 1, D0 + DAY + 61, 60),
        ];
        let d = daily(&recs, |_| 0);
        assert_eq!(d.len(), 2);
        assert_eq!(d[&((2026, 1, 10), "A".to_string())], 150);
        assert_eq!(d[&((2026, 1, 11), "B".to_string())], 60);
    }

    #[test]
    fn summary_and_days() {
        let recs = vec![
            rec("A", D0 + 100, D0 + 200, 100),
            rec("B", D0 + 300, D0 + 400, 300),
            rec("C", D0 + DAY, D0 + DAY + 9, 9),
        ];
        let d = daily(&recs, |_| 0);
        let (t, g) = day_summary(&d, (2026, 1, 10));
        assert_eq!(t, 400);
        assert_eq!(g, vec![("B".to_string(), 300), ("A".to_string(), 100)]);
        assert_eq!(day_summary(&d, (2026, 1, 9)), (0, vec![]));
        assert_eq!(
            last_days((2026, 3, 1), 3),
            vec![(2026, 3, 1), (2026, 2, 28), (2026, 2, 27)]
        );
    }
}
