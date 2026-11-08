//! Export of the session history as CSV, JSON (data contract v1) and ICS.
//!
//! Formats
//! - JSON: `{ "version": 1, "generated": <ISO local>, "sessions": [ {game, source,
//!   start, end, active_s} ] }`, sorted by start. Times carry the local UTC offset
//!   that was in effect at that instant. The in-progress session (auto-export
//!   only; the CLI reads closed sessions from the store and does not include the
//!   checkpoint) is added with `"open": true`.
//! - CSV: header `game,source,start,end,minutes,active_s`, RFC 4180 quoting, CRLF
//!   line endings, UTF-8 with a BOM so Excel detects the encoding. Closed sessions only.
//!   Fields starting with `=`, `+`, `-` or `@` are deliberately left unchanged (data
//!   fidelity over spreadsheet formula-injection hardening).
//! - ICS (RFC 5545): one VEVENT per closed session, times in UTC, lines folded at 75
//!   octets, CRLF.
//!
//! CLI: `gametime export --format csv|json|ics [--since YYYY-MM-DD] [--out PATH|-]`.
//! With `--out -` the data goes to stdout and the "wrote N sessions" note to stderr.

use serde::Serialize;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use crate::stats;
use crate::store::{self, SessionRecord, Store};
use crate::win32;

pub const SESSIONS_JSON: &str = "sessions.json";
pub const CALENDAR_ICS: &str = "gametime.ics";
pub const DAILY_CSV: &str = "daily.csv";
pub const DAILY_JSON: &str = "daily.json";

// ------------------------------------------------------------------- formatting

#[derive(Serialize)]
struct JsonSession<'a> {
    game: &'a str,
    source: &'a str,
    start: String,
    end: String,
    active_s: u64,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    open: bool,
}

#[derive(Serialize)]
struct JsonDoc<'a> {
    version: u32,
    generated: String,
    sessions: Vec<JsonSession<'a>>,
}

/// `sessions.json`. `open`, if given, is inserted (and flagged) in start order.
pub fn to_json(closed: &[SessionRecord], open: Option<&SessionRecord>, now: u64) -> String {
    let mut sessions: Vec<JsonSession> = closed.iter().map(|r| json_session(r, false)).collect();
    if let Some(o) = open {
        let at = closed.partition_point(|r| r.start_utc <= o.start_utc);
        sessions.insert(at, json_session(o, true));
    }
    let doc = JsonDoc {
        version: 1,
        generated: win32::format_iso_local(now),
        sessions,
    };
    let mut s = serde_json::to_string_pretty(&doc).unwrap_or_default();
    s.push('\n');
    s
}

fn json_session(r: &SessionRecord, open: bool) -> JsonSession<'_> {
    JsonSession {
        game: &r.game,
        source: &r.source,
        start: win32::format_iso_local(r.start_utc),
        end: win32::format_iso_local(r.end_utc),
        active_s: r.active_s,
        open,
    }
}

fn csv_field(out: &mut String, f: &str) {
    if f.contains(['"', ',', '\r', '\n']) {
        out.push('"');
        for c in f.chars() {
            if c == '"' {
                out.push('"');
            }
            out.push(c);
        }
        out.push('"');
    } else {
        out.push_str(f);
    }
}

/// CSV bytes: UTF-8 BOM, header, one CRLF-terminated row per session.
pub fn to_csv(sessions: &[SessionRecord]) -> Vec<u8> {
    let mut s = String::from("\u{feff}game,source,start,end,minutes,active_s\r\n");
    for r in sessions {
        csv_field(&mut s, &r.game);
        s.push(',');
        csv_field(&mut s, &r.source);
        s.push(',');
        s.push_str(&win32::format_iso_local(r.start_utc));
        s.push(',');
        s.push_str(&win32::format_iso_local(r.end_utc));
        s.push_str(&format!(
            ",{:.1},{}\r\n",
            r.active_s as f64 / 60.0,
            r.active_s
        ));
    }
    s.into_bytes()
}

// ------------------------------------------------------------ daily aggregate

/// Minutes (one decimal) from aggregated seconds. Always converted after the
/// per-(day, game) sum, never per session piece.
fn daily_minutes(secs: u64) -> f64 {
    (secs as f64 / 6.0).round() / 10.0
}

fn date_str((y, m, d): stats::Date) -> String {
    format!("{y:04}-{m:02}-{d:02}")
}

#[derive(Serialize)]
struct DailyRow<'a> {
    date: String,
    game: &'a str,
    minutes: f64,
}

#[derive(Serialize)]
struct DailyDoc<'a> {
    version: u32,
    generated: String,
    days: Vec<DailyRow<'a>>,
}

/// `daily.json`: `{ "version": 1, "generated": <ISO local>, "days": [ {date
/// "YYYY-MM-DD" (local), game, minutes} ] }`, sorted by date then game.
pub fn to_daily_json(daily: &stats::Daily, now: u64) -> String {
    let doc = DailyDoc {
        version: 1,
        generated: win32::format_iso_local(now),
        days: daily
            .iter()
            .map(|((d, g), s)| DailyRow {
                date: date_str(*d),
                game: g,
                minutes: daily_minutes(*s),
            })
            .collect(),
    };
    let mut s = serde_json::to_string_pretty(&doc).unwrap_or_default();
    s.push('\n');
    s
}

/// `daily.csv`: BOM, header `date,game,minutes`, CRLF rows.
pub fn to_daily_csv(daily: &stats::Daily) -> Vec<u8> {
    let mut s = String::from("\u{feff}date,game,minutes\r\n");
    for ((d, g), secs) in daily {
        s.push_str(&date_str(*d));
        s.push(',');
        csv_field(&mut s, g);
        s.push_str(&format!(",{:.1}\r\n", daily_minutes(*secs)));
    }
    s.into_bytes()
}

/// 64-bit FNV-1a.
fn fnv1a(parts: &[&[u8]]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for p in parts {
        for &b in *p {
            h ^= b as u64;
            h = h.wrapping_mul(0x0100_0000_01b3);
        }
    }
    h
}

/// Stable per session: depends only on game and start.
fn ics_uid(r: &SessionRecord) -> String {
    let h = fnv1a(&[r.game.as_bytes(), &[0], r.start_utc.to_string().as_bytes()]);
    format!("{h:016x}@gametime")
}

/// `YYYYMMDDTHHMMSSZ`
fn ics_utc(secs: u64) -> String {
    let days = (secs / 86400) as i64;
    let rem = secs % 86400;
    let (y, m, d) = win32::civil_from_days(days);
    format!(
        "{y:04}{m:02}{d:02}T{:02}{:02}{:02}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

/// `1h 23m`, `23m`, `45s`.
pub fn format_duration(secs: u64) -> String {
    let (h, m) = (secs / 3600, secs % 3600 / 60);
    if h > 0 {
        format!("{h}h {m}m")
    } else if m > 0 {
        format!("{m}m")
    } else {
        format!("{secs}s")
    }
}

fn ics_escape(s: &str) -> String {
    let mut o = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\\' => o.push_str("\\\\"),
            ';' => o.push_str("\\;"),
            ',' => o.push_str("\\,"),
            '\n' => o.push_str("\\n"),
            '\t' => o.push('\t'),
            c if (c as u32) < 0x20 => {} // control characters are not valid TEXT
            c => o.push(c),
        }
    }
    o
}

/// Append `line` folded at 75 octets (never inside a UTF-8 sequence), CRLF-terminated.
fn push_folded(out: &mut String, line: &str) {
    let mut used = 0;
    for c in line.chars() {
        let n = c.len_utf8();
        if used + n > 75 {
            out.push_str("\r\n ");
            used = 1; // the leading space counts
        }
        out.push(c);
        used += n;
    }
    out.push_str("\r\n");
}

/// The calendar for closed sessions; `now` is the DTSTAMP.
pub fn to_ics(sessions: &[SessionRecord], now: u64) -> String {
    let stamp = ics_utc(now);
    let mut s = String::new();
    for l in [
        "BEGIN:VCALENDAR",
        "VERSION:2.0",
        "PRODID:-//GameTime//EN",
        "CALSCALE:GREGORIAN",
        "METHOD:PUBLISH",
        "X-WR-CALNAME:GameTime",
    ] {
        push_folded(&mut s, l);
    }
    for r in sessions {
        let active = format_duration(r.active_s);
        let wall = format_duration(r.end_utc.saturating_sub(r.start_utc));
        push_folded(&mut s, "BEGIN:VEVENT");
        push_folded(&mut s, &format!("UID:{}", ics_uid(r)));
        push_folded(&mut s, &format!("DTSTAMP:{stamp}"));
        push_folded(&mut s, &format!("DTSTART:{}", ics_utc(r.start_utc)));
        push_folded(&mut s, &format!("DTEND:{}", ics_utc(r.end_utc)));
        push_folded(
            &mut s,
            &format!("SUMMARY:{}", ics_escape(&format!("{} ({active})", r.game))),
        );
        let desc = format!("Source: {}\nPlayed: {active}\nWall time: {wall}", r.source);
        push_folded(&mut s, &format!("DESCRIPTION:{}", ics_escape(&desc)));
        push_folded(&mut s, "CATEGORIES:Games");
        push_folded(&mut s, "TRANSP:TRANSPARENT");
        push_folded(&mut s, "END:VEVENT");
    }
    push_folded(&mut s, "END:VCALENDAR");
    s
}

// ----------------------------------------------------------------- auto-export

/// The bytes of the auto-export files, built on the poll thread so that the
/// (possibly slow) disk writes can happen elsewhere.
pub struct Payload {
    pub json: Vec<u8>,
    pub ics: Vec<u8>,
    pub daily_csv: Vec<u8>,
    pub daily_json: Vec<u8>,
    /// Records skipped (invalid times) by the daily aggregation.
    pub dropped: usize,
}

/// Load the closed sessions and format `sessions.json` (with the open session,
/// if any), `gametime.ics`, `daily.csv` and `daily.json`. An open session that
/// is already in the log (same game and start) is not added twice.
pub fn build_payload(store: &Store, open: Option<&SessionRecord>, now: u64) -> io::Result<Payload> {
    let closed = store.load_all()?;
    let open = open.filter(|o| {
        !closed
            .iter()
            .any(|c| c.game == o.game && c.start_utc == o.start_utc)
    });
    let (daily, dropped) =
        stats::daily_checked(closed.iter().chain(open), win32::local_offset_secs);
    Ok(Payload {
        json: to_json(&closed, open, now).into_bytes(),
        ics: to_ics(&closed, now).into_bytes(),
        daily_csv: to_daily_csv(&daily),
        daily_json: to_daily_json(&daily, now).into_bytes(),
        dropped,
    })
}

/// Write the files into `dir`, creating it if needed. No fsync: the files are
/// derived data and `dir` may be a slow network share.
pub fn write_payload(dir: &Path, p: &Payload) -> io::Result<()> {
    std::fs::create_dir_all(dir)?;
    store::write_atomic_nosync(&dir.join(SESSIONS_JSON), &p.json)?;
    store::write_atomic_nosync(&dir.join(CALENDAR_ICS), &p.ics)?;
    store::write_atomic_nosync(&dir.join(DAILY_CSV), &p.daily_csv)?;
    store::write_atomic_nosync(&dir.join(DAILY_JSON), &p.daily_json)
}

// ------------------------------------------------------------------------ CLI

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    Csv,
    Json,
    Ics,
}

impl Format {
    fn ext(self) -> &'static str {
        match self {
            Format::Csv => "csv",
            Format::Json => "json",
            Format::Ics => "ics",
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub struct Args {
    pub format: Format,
    pub since: Option<(i32, u32, u32)>,
    /// `None` = default file in the current directory; `Some("-")` = stdout.
    pub out: Option<String>,
}

/// Parse `YYYY-MM-DD` (a real calendar date).
pub fn parse_date(s: &str) -> Option<(i32, u32, u32)> {
    let b = s.as_bytes();
    if b.len() != 10 || b[4] != b'-' || b[7] != b'-' {
        return None;
    }
    let num = |r: std::ops::Range<usize>| -> Option<u32> {
        let t = s.get(r)?;
        if t.bytes().all(|c| c.is_ascii_digit()) {
            t.parse().ok()
        } else {
            None
        }
    };
    let (y, m, d) = (num(0..4)?, num(5..7)?, num(8..10)?);
    if !(1..=12).contains(&m) || d == 0 {
        return None;
    }
    // Round-trip rejects 2026-02-30 and friends.
    let back = win32::civil_from_days(win32::days_from_civil(y as i64, m, d));
    (back == (y as i64, m, d)).then_some((y as i32, m, d))
}

/// Parse the arguments following `export`.
pub fn parse_args(args: &[String]) -> Result<Args, String> {
    let mut format = None;
    let mut since = None;
    let mut out = None;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        let name = a.as_str();
        match name {
            "--format" | "--since" | "--out" => {}
            other => return Err(format!("unknown argument '{other}'")),
        }
        let v = it
            .next()
            .ok_or_else(|| format!("{name} needs a value"))?
            .clone();
        match name {
            "--format" => {
                format = Some(match v.as_str() {
                    "csv" => Format::Csv,
                    "json" => Format::Json,
                    "ics" => Format::Ics,
                    _ => return Err(format!("unknown format '{v}' (expected csv, json or ics)")),
                });
            }
            "--since" => {
                since = Some(
                    parse_date(&v)
                        .ok_or_else(|| format!("invalid date '{v}' (expected YYYY-MM-DD)"))?,
                );
            }
            _ => out = Some(v),
        }
    }
    let format = format.ok_or("missing --format csv|json|ics")?;
    Ok(Args { format, since, out })
}

/// Keep sessions whose local start date is on or after `since`.
pub fn filter_since(sessions: &mut Vec<SessionRecord>, since: Option<(i32, u32, u32)>) {
    if let Some(s) = since {
        sessions.retain(|r| win32::local_date(r.start_utc) >= s);
    }
}

const EXPORT_USAGE: &str =
    "Usage: gametime export --format csv|json|ics [--since YYYY-MM-DD] [--out PATH|-]\n  --since keeps sessions whose local start date is on or after the given date";

/// `gametime export ...`; `args` are the arguments after `export`.
pub fn cli(args: &[String]) -> ExitCode {
    let args = match parse_args(args) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("export: {e}\n{EXPORT_USAGE}");
            return ExitCode::from(2);
        }
    };
    match run_cli(&args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("export failed: {e}");
            ExitCode::FAILURE
        }
    }
}

fn run_cli(args: &Args) -> io::Result<()> {
    let dir = crate::config::data_dir()?;
    let (mut sessions, skipped) = Store::open(&dir)?.load_all_with_stats()?;
    if skipped > 0 {
        eprintln!("warning: skipped {skipped} unreadable line(s) in sessions.jsonl");
    }
    filter_since(&mut sessions, args.since);
    if sessions.is_empty() {
        eprintln!(
            "warning: no sessions to export (an empty calendar may be rejected by some importers)"
        );
    }
    let now = win32::now_utc_secs();
    let bytes = match args.format {
        Format::Csv => to_csv(&sessions),
        Format::Json => to_json(&sessions, None, now).into_bytes(),
        Format::Ics => to_ics(&sessions, now).into_bytes(),
    };
    match args.out.as_deref() {
        Some("-") => {
            let mut so = io::stdout().lock();
            so.write_all(&bytes)?;
            so.flush()?;
            eprintln!("wrote {} sessions to stdout", sessions.len());
        }
        out => {
            let path = out.map(PathBuf::from).unwrap_or_else(|| {
                PathBuf::from(format!("gametime-sessions.{}", args.format.ext()))
            });
            std::fs::write(&path, &bytes)?;
            println!("wrote {} sessions to {}", sessions.len(), path.display());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(game: &str, source: &str, start: u64, end: u64, active: u64) -> SessionRecord {
        SessionRecord {
            game: game.into(),
            source: source.into(),
            exe: String::new(),
            start_utc: start,
            end_utc: end,
            active_s: active,
        }
    }

    // 2026-01-02 03:04:05 UTC
    const T: u64 = 1_767_323_045;

    #[test]
    fn daily_formats() {
        let mut d = stats::Daily::new();
        d.insert(((2026, 1, 2), "A, B".into()), 90); // 1.5 min
        d.insert(((2026, 1, 2), "C".into()), 60); // 1.0
        d.insert(((2026, 1, 10), "C".into()), 7); // 0.1
        let csv = String::from_utf8(to_daily_csv(&d)).unwrap();
        assert_eq!(
            csv,
            "\u{feff}date,game,minutes\r\n2026-01-02,\"A, B\",1.5\r\n2026-01-02,C,1.0\r\n2026-01-10,C,0.1\r\n"
        );
        let v: serde_json::Value = serde_json::from_str(&to_daily_json(&d, T)).unwrap();
        assert_eq!(v["version"], 1);
        assert!(v["generated"].is_string());
        let days = v["days"].as_array().unwrap();
        assert_eq!(days.len(), 3);
        assert_eq!(days[0]["date"], "2026-01-02");
        assert_eq!(days[0]["game"], "A, B");
        assert_eq!(days[0]["minutes"], 1.5);
        assert_eq!(days[2]["minutes"], 0.1);
    }

    #[test]
    fn json_shape_order_and_open_flag() {
        let closed = vec![
            rec("A", "steam", T, T + 100, 90),
            rec("B", "epic", T + 500, T + 600, 100),
        ];
        let open = rec("C", "gog", T + 300, T + 400, 50);
        let s = to_json(&closed, Some(&open), T + 1000);
        let v: serde_json::Value = serde_json::from_str(&s).unwrap();
        assert_eq!(v["version"], 1);
        assert!(v["generated"].as_str().unwrap().starts_with("2026-01-0"));
        let ss = v["sessions"].as_array().unwrap();
        let games: Vec<_> = ss.iter().map(|x| x["game"].as_str().unwrap()).collect();
        assert_eq!(games, ["A", "C", "B"]);
        assert!(ss[0].get("open").is_none());
        assert_eq!(ss[1]["open"], true);
        assert_eq!(ss[0]["active_s"], 90);
        assert_eq!(ss[0]["source"], "steam");
        // Field order is part of the contract.
        let pos: Vec<usize> = [
            "\"game\"",
            "\"source\"",
            "\"start\"",
            "\"end\"",
            "\"active_s\"",
        ]
        .iter()
        .map(|k| s.find(k).unwrap())
        .collect();
        assert!(pos.windows(2).all(|w| w[0] < w[1]));
        assert!(s.find("\"version\"").unwrap() < s.find("\"generated\"").unwrap());
    }

    #[test]
    fn json_empty() {
        let v: serde_json::Value = serde_json::from_str(&to_json(&[], None, T)).unwrap();
        assert_eq!(v["sessions"].as_array().unwrap().len(), 0);
    }

    #[test]
    fn csv_bom_crlf_and_quoting() {
        let r = vec![rec("Say \"Hi\", Bob\nX", "steam", T, T + 90, 90)];
        let b = to_csv(&r);
        assert_eq!(&b[..3], [0xEF, 0xBB, 0xBF]);
        let s = String::from_utf8(b).unwrap();
        assert!(s.starts_with("\u{feff}game,source,start,end,minutes,active_s\r\n"));
        assert!(s.contains("\"Say \"\"Hi\"\", Bob\nX\",steam,"));
        assert!(s.ends_with(",1.5,90\r\n"));
        assert_eq!(s.matches("\r\n").count(), 2);
    }

    #[test]
    fn csv_plain_fields_unquoted() {
        let s = String::from_utf8(to_csv(&[rec("Doom", "epic", T, T, 0)])).unwrap();
        assert!(s.contains("\r\nDoom,epic,"));
    }

    #[test]
    fn ics_structure_and_utc() {
        let s = to_ics(&[rec("Doom", "steam", T, T + 4980 + 120, 4980)], T + 9999);
        assert!(s.starts_with(
            "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//GameTime//EN\r\nCALSCALE:GREGORIAN\r\nMETHOD:PUBLISH\r\nX-WR-CALNAME:GameTime\r\n"
        ));
        assert!(s.ends_with("END:VEVENT\r\nEND:VCALENDAR\r\n"));
        assert!(s.contains("\r\nDTSTART:20260102T030405Z\r\n"));
        assert!(s.contains("\r\nDTEND:20260102T042905Z\r\n"));
        assert!(s.contains("\r\nSUMMARY:Doom (1h 23m)\r\n"));
        assert!(s.contains("DESCRIPTION:Source: steam\\nPlayed: 1h 23m\\nWall time: 1h 25m\r\n"));
        assert!(s.contains("CATEGORIES:Games\r\nTRANSP:TRANSPARENT\r\n"));
        assert!(s.contains("DTSTAMP:20260102T055044Z\r\n"));
        assert!(!s.replace("\r\n", "").contains('\n'));
    }

    #[test]
    fn ics_escaping() {
        assert_eq!(ics_escape("a\\b;c,d\ne\r"), "a\\\\b\\;c\\,d\\ne");
        assert_eq!(ics_escape("a\u{0}b\u{7}c\td\u{1b}"), "abc\td");
        let s = to_ics(&[rec("A;B,C", "x", T, T + 10, 10)], T);
        assert!(s.contains("SUMMARY:A\\;B\\,C (10s)\r\n"));
    }

    #[test]
    fn ics_uid_is_stable_and_distinct() {
        let a = rec("Doom", "steam", T, T + 10, 10);
        let b = rec("Doom", "steam", T, T + 999, 500); // same session, grown
        let c = rec("Doom", "steam", T + 1, T + 10, 10);
        let d = rec("Quake", "steam", T, T + 10, 10);
        assert_eq!(ics_uid(&a), ics_uid(&b));
        assert_ne!(ics_uid(&a), ics_uid(&c));
        assert_ne!(ics_uid(&a), ics_uid(&d));
        assert!(ics_uid(&a).ends_with("@gametime"));
        assert_eq!(ics_uid(&a).len(), 16 + "@gametime".len());
        // Known FNV-1a vector.
        assert_eq!(fnv1a(&[b"a"]), 0xaf63_dc4c_8601_ec8c);
    }

    #[test]
    fn folding_respects_75_octets_and_utf8() {
        let long = format!("SUMMARY:{}", "x".repeat(200));
        let mut s = String::new();
        push_folded(&mut s, &long);
        for l in s.split("\r\n").filter(|l| !l.is_empty()) {
            assert!(l.len() <= 75, "{}", l.len());
        }
        assert_eq!(s.replace("\r\n ", "").trim_end(), long);

        // 73 ASCII + 'é'(2) = 75 fits; the next 'é' wraps.
        let line = format!("{}{}", "a".repeat(73), "é".repeat(5));
        let mut s = String::new();
        push_folded(&mut s, &line);
        assert!(s.starts_with(&format!("{}é\r\n ", "a".repeat(73))));
        for l in s.split("\r\n").filter(|l| !l.is_empty()) {
            assert!(l.len() <= 75);
        }
        assert_eq!(s.replace("\r\n ", "").trim_end(), line);

        // A 4-byte char at 74 octets must move to the next line whole.
        let line = format!("{}\u{1F600}", "a".repeat(74));
        let mut s = String::new();
        push_folded(&mut s, &line);
        assert_eq!(s, format!("{}\r\n \u{1F600}\r\n", "a".repeat(74)));
    }

    #[test]
    fn durations() {
        assert_eq!(format_duration(0), "0s");
        assert_eq!(format_duration(59), "59s");
        assert_eq!(format_duration(60), "1m");
        assert_eq!(format_duration(3600), "1h 0m");
        assert_eq!(format_duration(4980), "1h 23m");
    }

    #[test]
    fn date_parsing() {
        assert_eq!(parse_date("2026-02-28"), Some((2026, 2, 28)));
        assert_eq!(parse_date("2024-02-29"), Some((2024, 2, 29)));
        for bad in [
            "2026-02-30",
            "2026-13-01",
            "2026-00-10",
            "2026-1-01",
            "26-01-01",
            "2026/01/01",
            "",
            "2026-01-0x",
            "+026-01-01",
            "2026-01-\u{e9}1",
        ] {
            assert_eq!(parse_date(bad), None, "{bad}");
        }
    }

    fn sv(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn arg_parsing() {
        let a = parse_args(&sv(&[
            "--format",
            "ics",
            "--since",
            "2026-01-05",
            "--out",
            "-",
        ]))
        .unwrap();
        assert_eq!(
            a,
            Args {
                format: Format::Ics,
                since: Some((2026, 1, 5)),
                out: Some("-".into())
            }
        );
        let a = parse_args(&sv(&["--format", "csv"])).unwrap();
        assert_eq!(
            a,
            Args {
                format: Format::Csv,
                since: None,
                out: None
            }
        );
        for bad in [
            sv(&[]),
            sv(&["--format"]),
            sv(&["--format", "xml"]),
            sv(&["--format", "csv", "--since", "yesterday"]),
            sv(&["--format", "csv", "--since"]),
            sv(&["--format", "csv", "--out"]),
            sv(&["--format", "csv", "--bogus"]),
            sv(&["csv"]),
        ] {
            assert!(parse_args(&bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn since_filters_on_local_start_date() {
        let day = |d: u32| win32::days_from_civil(2026, 3, d) as u64 * 86400 + 12 * 3600;
        let mut v = vec![
            rec("A", "x", day(9), day(9) + 10, 10),
            rec("B", "x", day(10), day(10) + 10, 10),
            rec("C", "x", day(11), day(11) + 10, 10),
        ];
        filter_since(&mut v, Some((2026, 3, 10)));
        let g: Vec<_> = v.iter().map(|r| r.game.as_str()).collect();
        assert_eq!(g, ["B", "C"]);
        filter_since(&mut v, None);
        assert_eq!(v.len(), 2);
    }
}
