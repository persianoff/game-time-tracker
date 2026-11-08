//! `gametime scan` and `gametime status`. Render functions take data and
//! return a `String`; the command functions gather data and print.

use crate::config::{self, Config};
use crate::detect::{self, Game};
use crate::stats::{self, Date};
use crate::store::{SessionRecord, Store};
use crate::tracker::MUTEX_NAME;
use crate::win32;
use std::fmt::Write as _;
use std::path::Path;
use std::process::ExitCode;

/// `2h 05m`, `45m`, `<1m`.
pub fn fmt_duration(secs: u64) -> String {
    let mins = secs / 60;
    if mins == 0 {
        "<1m".to_string()
    } else if mins >= 60 {
        format!("{}h {:02}m", mins / 60, mins % 60)
    } else {
        format!("{mins}m")
    }
}

pub fn fmt_date(d: Date) -> String {
    format!("{:04}-{:02}-{:02}", d.0, d.1, d.2)
}

/// Pad `s` with spaces to `width` characters (char count, not bytes).
fn pad(s: &str, width: usize) -> String {
    let n = s.chars().count();
    let mut out = String::with_capacity(s.len() + width.saturating_sub(n));
    out.push_str(s);
    for _ in n..width {
        out.push(' ');
    }
    out
}

fn width_of<'a>(header: &str, cells: impl Iterator<Item = &'a str>) -> usize {
    cells
        .map(|c| c.chars().count())
        .chain(std::iter::once(header.chars().count()))
        .max()
        .unwrap_or(0)
}

fn reject_args(cmd: &str, args: &[String]) -> Option<ExitCode> {
    if args.is_empty() {
        return None;
    }
    eprintln!("gametime {cmd} takes no arguments");
    Some(ExitCode::from(2))
}

// ----------------------------------------------------------------------- scan

pub fn render_scan(games: &[Game], config_path: &str) -> String {
    if games.is_empty() {
        return format!(
            "No games found: no supported launchers (Steam, Epic, GOG, Ubisoft Connect, EA, Xbox) \
             with installed games were detected.\n\
             Add games manually via \"extra_games\" in {config_path}, for example:\n  \
             \"extra_games\": {{ \"D:\\\\Games\\\\MyGame\\\\game.exe\": \"My Game\" }}\n"
        );
    }
    let mut sorted: Vec<&Game> = games.iter().collect();
    sorted.sort_by_cached_key(|g| (g.name.to_lowercase(), g.name.clone()));
    let dirs: Vec<String> = sorted
        .iter()
        .map(|g| g.install_dir.display().to_string())
        .collect();
    let w_name = width_of("NAME", sorted.iter().map(|g| g.name.as_str()));
    let w_src = width_of("SOURCE", sorted.iter().map(|g| g.source.as_str()));

    let mut out = String::new();
    let _ = writeln!(
        out,
        "{}  {}  INSTALL DIR",
        pad("NAME", w_name),
        pad("SOURCE", w_src)
    );
    for (g, dir) in sorted.iter().zip(&dirs) {
        let _ = writeln!(
            out,
            "{}  {}  {}",
            pad(&g.name, w_name),
            pad(g.source.as_str(), w_src),
            dir
        );
    }

    let mut counts: Vec<(&str, usize)> = Vec::new();
    for g in &sorted {
        let s = g.source.as_str();
        match counts.iter_mut().find(|c| c.0 == s) {
            Some(c) => c.1 += 1,
            None => counts.push((s, 1)),
        }
    }
    counts.sort();
    let parts: Vec<String> = counts.iter().map(|(s, n)| format!("{s}: {n}")).collect();
    let n = sorted.len();
    let _ = writeln!(
        out,
        "\n{n} {} ({})",
        if n == 1 { "game" } else { "games" },
        parts.join(", ")
    );
    out
}

pub fn scan(args: &[String]) -> ExitCode {
    if let Some(code) = reject_args("scan", args) {
        return code;
    }
    let dir = match config::data_dir() {
        Ok(d) => d,
        Err(e) => {
            eprintln!("cannot open data directory: {e}");
            return ExitCode::FAILURE;
        }
    };
    let cfg = match Config::load_or_create(&dir) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("cannot read config: {e}");
            return ExitCode::FAILURE;
        }
    };
    let db = detect::scan_system(&cfg);
    print!(
        "{}",
        render_scan(db.games(), &dir.join("config.json").display().to_string())
    );
    ExitCode::SUCCESS
}

// --------------------------------------------------------------------- status

pub struct CurrentSession {
    pub game: String,
    pub source: String,
    /// Local start time, e.g. `2026-11-07T22:31:05+01:00`.
    pub started_local: String,
    pub active_s: u64,
}

pub struct DayRow {
    pub date: Date,
    pub total_s: u64,
    pub top_game: Option<String>,
}

pub struct StatusData {
    pub tracker_running: bool,
    pub current: Option<CurrentSession>,
    pub today: Vec<(String, u64)>,
    /// Most recent first, including today.
    pub week: Vec<DayRow>,
    pub skipped_lines: usize,
    pub data_dir: String,
}

pub fn render_status(d: &StatusData) -> String {
    let mut out = String::new();
    let _ = writeln!(
        out,
        "Tracker: {}",
        if d.tracker_running {
            "running"
        } else {
            "not running"
        }
    );
    match &d.current {
        Some(c) => {
            let _ = writeln!(
                out,
                "Current session: {} ({}), started {}, active {}{}",
                c.game,
                c.source,
                c.started_local,
                fmt_duration(c.active_s),
                if d.tracker_running {
                    ""
                } else {
                    " [stale: tracker not running]"
                }
            );
        }
        None => out.push_str("Current session: no game running\n"),
    }

    let today_total: u64 = d.today.iter().map(|g| g.1).sum();
    out.push('\n');
    if d.today.is_empty() {
        out.push_str("Today: no play time yet\n");
    } else {
        let _ = writeln!(out, "Today: {}", fmt_duration(today_total));
        let w = width_of("", d.today.iter().map(|g| g.0.as_str()));
        for (g, s) in &d.today {
            let _ = writeln!(out, "  {}  {}", pad(g, w), fmt_duration(*s));
        }
    }

    out.push_str("\nLast 7 days:\n");
    let totals: Vec<String> = d
        .week
        .iter()
        .map(|r| {
            if r.total_s == 0 {
                "-".to_string()
            } else {
                fmt_duration(r.total_s)
            }
        })
        .collect();
    let w_tot = width_of("TOTAL", totals.iter().map(String::as_str));
    let _ = writeln!(
        out,
        "  {}  {}  TOP GAME",
        pad("DATE", 10),
        pad("TOTAL", w_tot)
    );
    for (r, t) in d.week.iter().zip(&totals) {
        let _ = writeln!(
            out,
            "  {}  {}  {}",
            fmt_date(r.date),
            pad(t, w_tot),
            r.top_game.as_deref().unwrap_or("-")
        );
    }

    if d.skipped_lines > 0 {
        let _ = writeln!(
            out,
            "\nWarning: {} unreadable line(s) skipped in sessions.jsonl",
            d.skipped_lines
        );
    }
    let _ = writeln!(out, "\nData: {}", d.data_dir);
    out
}

/// Build the status data from loaded records. `offset` is the local UTC
/// offset function; `now` is the current UTC time. The checkpointed session
/// is counted too, unless it is already in `records`.
pub fn build_status(
    records: &[SessionRecord],
    checkpoint: Option<&SessionRecord>,
    skipped_lines: usize,
    tracker_running: bool,
    now: u64,
    offset: impl Fn(u64) -> i32 + Copy,
    data_dir: &Path,
) -> StatusData {
    let mut all: Vec<&SessionRecord> = records.iter().collect();
    if let Some(c) = checkpoint {
        if !records
            .iter()
            .any(|r| r.game == c.game && r.start_utc == c.start_utc)
        {
            all.push(c);
        }
    }
    let daily = stats::daily(all, offset);
    let today_date = {
        let (y, m, d) = civil_date(now, offset);
        (y, m, d)
    };
    let (_, today) = stats::day_summary(&daily, today_date);
    let week = stats::last_days(today_date, 7)
        .into_iter()
        .map(|date| {
            let (total_s, games) = stats::day_summary(&daily, date);
            DayRow {
                date,
                total_s,
                top_game: games.into_iter().next().map(|g| g.0),
            }
        })
        .collect();
    let current = checkpoint.map(|c| CurrentSession {
        game: c.game.clone(),
        source: c.source.clone(),
        started_local: win32::format_iso_local(c.start_utc),
        active_s: c.active_s,
    });
    StatusData {
        tracker_running,
        current,
        today,
        week,
        skipped_lines,
        data_dir: data_dir.display().to_string(),
    }
}

fn civil_date(utc: u64, offset: impl Fn(u64) -> i32) -> Date {
    let (y, m, d) = win32::civil_from_days((utc as i64 + offset(utc) as i64).div_euclid(86400));
    (y as i32, m, d)
}

pub fn status(args: &[String]) -> ExitCode {
    if let Some(code) = reject_args("status", args) {
        return code;
    }
    let dir = match config::data_dir() {
        Ok(d) => d,
        Err(e) => {
            eprintln!("cannot open data directory: {e}");
            return ExitCode::FAILURE;
        }
    };
    let store = match Store::open(&dir) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("cannot open store: {e}");
            return ExitCode::FAILURE;
        }
    };
    let (records, skipped) = match store.load_all_with_stats() {
        Ok(r) => r,
        Err(e) => {
            eprintln!("cannot read sessions: {e}");
            return ExitCode::FAILURE;
        }
    };
    let data = build_status(
        &records,
        store.load_checkpoint().as_ref(),
        skipped,
        win32::is_running(MUTEX_NAME),
        win32::now_utc_secs(),
        win32::local_offset_secs,
        &dir,
    );
    print!("{}", render_status(&data));
    ExitCode::SUCCESS
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::detect::Source;
    use std::path::PathBuf;

    fn game(name: &str, source: Source, dir: &str) -> Game {
        Game {
            name: name.into(),
            source,
            install_dir: PathBuf::from(dir),
        }
    }

    #[test]
    fn durations() {
        assert_eq!(fmt_duration(0), "<1m");
        assert_eq!(fmt_duration(59), "<1m");
        assert_eq!(fmt_duration(60), "1m");
        assert_eq!(fmt_duration(45 * 60 + 59), "45m");
        assert_eq!(fmt_duration(3600), "1h 00m");
        assert_eq!(fmt_duration(2 * 3600 + 5 * 60), "2h 05m");
        assert_eq!(fmt_duration(100 * 3600 + 59 * 60), "100h 59m");
    }

    #[test]
    fn dates() {
        assert_eq!(fmt_date((2026, 1, 5)), "2026-01-05");
    }

    #[test]
    fn pad_counts_chars() {
        assert_eq!(pad("Pokémon", 9), "Pokémon  ");
        assert_eq!(pad("abc", 2), "abc");
    }

    #[test]
    fn scan_table_aligned_and_sorted() {
        let games = vec![
            game("Zelda", Source::Epic, "C:\\E\\Zelda"),
            game("Pokémon", Source::Steam, "C:\\S\\Pokemon"),
            game("alpha", Source::Steam, "C:\\S\\alpha"),
        ];
        let s = render_scan(&games, "C:\\cfg.json");
        let lines: Vec<&str> = s.lines().collect();
        assert_eq!(lines[0], "NAME     SOURCE  INSTALL DIR");
        assert_eq!(lines[1], "alpha    steam   C:\\S\\alpha");
        assert_eq!(lines[2], "Pokémon  steam   C:\\S\\Pokemon");
        assert_eq!(lines[3], "Zelda    epic    C:\\E\\Zelda");
        assert_eq!(lines[5], "3 games (epic: 1, steam: 2)");
    }

    #[test]
    fn scan_singular_and_empty() {
        let s = render_scan(&[game("A", Source::Gog, "C:\\A")], "x");
        assert!(s.ends_with("1 game (gog: 1)\n"));
        let e = render_scan(
            &[],
            "C:\\Users\\me\\AppData\\Roaming\\GameTime\\config.json",
        );
        assert!(e.contains("No games found"));
        assert!(e.contains("extra_games"));
        assert!(e.contains("C:\\Users\\me\\AppData\\Roaming\\GameTime\\config.json"));
    }

    fn sample() -> StatusData {
        StatusData {
            tracker_running: true,
            current: Some(CurrentSession {
                game: "Hades".into(),
                source: "steam".into(),
                started_local: "2026-01-10T20:00:00+01:00".into(),
                active_s: 3900,
            }),
            today: vec![("Hades".into(), 3900), ("Celeste".into(), 120)],
            week: vec![
                DayRow {
                    date: (2026, 1, 10),
                    total_s: 4020,
                    top_game: Some("Hades".into()),
                },
                DayRow {
                    date: (2026, 1, 9),
                    total_s: 0,
                    top_game: None,
                },
            ],
            skipped_lines: 2,
            data_dir: "C:\\d".into(),
        }
    }

    #[test]
    fn status_render_full() {
        let s = render_status(&sample());
        assert!(s.contains("Tracker: running\n"));
        assert!(s.contains(
            "Current session: Hades (steam), started 2026-01-10T20:00:00+01:00, active 1h 05m\n"
        ));
        assert!(s.contains("Today: 1h 07m\n  Hades    1h 05m\n  Celeste  2m\n"));
        assert!(s.contains("  DATE        TOTAL   TOP GAME\n"));
        assert!(s.contains("  2026-01-10  1h 07m  Hades\n"));
        assert!(s.contains("  2026-01-09  -       -\n"));
        assert!(s.contains("2 unreadable line(s)"));
        assert!(s.ends_with("Data: C:\\d\n"));
    }

    #[test]
    fn status_render_idle() {
        let mut d = sample();
        d.tracker_running = false;
        d.current = None;
        d.today.clear();
        d.skipped_lines = 0;
        let s = render_status(&d);
        assert!(s.contains("Tracker: not running"));
        assert!(s.contains("Current session: no game running"));
        assert!(s.contains("Today: no play time yet"));
        assert!(!s.contains("unreadable"));
        d.current = sample().current;
        assert!(render_status(&d).contains("[stale: tracker not running]"));
    }

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

    #[test]
    fn build_status_aggregates() {
        let day = 86400u64;
        let d0 = 20463 * day; // 2026-01-10 UTC
        let now = d0 + 12 * 3600;
        let records = vec![
            rec("A", d0 + 3600, d0 + 7200, 3600),
            rec("B", d0 + day / 2 - 100, d0 + day / 2 - 40, 60),
            rec("A", d0 - day + 100, d0 - day + 700, 600),
        ];
        let cp = rec("C", d0 + 9 * 3600, d0 + 10 * 3600, 1800);
        let s = build_status(&records, Some(&cp), 0, true, now, |_| 0, Path::new("D"));
        assert_eq!(
            s.today,
            vec![("A".into(), 3600), ("C".into(), 1800), ("B".into(), 60)]
        );
        assert_eq!(s.week.len(), 7);
        assert_eq!(s.week[0].date, (2026, 1, 10));
        assert_eq!(s.week[0].top_game.as_deref(), Some("A"));
        assert_eq!(s.week[1].total_s, 600);
        assert_eq!(s.week[2].total_s, 0);
        assert_eq!(s.current.as_ref().unwrap().game, "C");

        // A checkpoint already in the log is not double counted.
        let dup = rec("A", d0 + 3600, d0 + 7200, 3600);
        let s = build_status(&records, Some(&dup), 0, true, now, |_| 0, Path::new("D"));
        assert_eq!(s.week[0].total_s, 3660);
    }
}
