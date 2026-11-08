//! Game detection: scans launcher installs and maps an exe path to a game by
//! longest component-wise install-directory prefix.
//!
//! Every scanner is a pure parser plus a thin system reader.

use crate::config::Config;
use crate::steam::{self, normalize_path};
use crate::win32::{self, Hive};
use serde::{Deserialize, Serialize};
use std::fmt;
use std::path::{Component, Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Source {
    Steam,
    Epic,
    Gog,
    Ubisoft,
    Ea,
    Xbox,
    Manual,
}

impl Source {
    pub fn as_str(self) -> &'static str {
        match self {
            Source::Steam => "steam",
            Source::Epic => "epic",
            Source::Gog => "gog",
            Source::Ubisoft => "ubisoft",
            Source::Ea => "ea",
            Source::Xbox => "xbox",
            Source::Manual => "manual",
        }
    }
}

impl fmt::Display for Source {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Game {
    pub name: String,
    pub source: Source,
    pub install_dir: PathBuf,
}

// ------------------------------------------------------------ path hygiene

/// Normalise `raw` and reject unsafe or degenerate install directories:
/// the path must be absolute, have at least one normal component
/// below the drive (or share) root (for non-manual sources that component
/// must not be a library root such as `Games`), and must not be a broad system folder
/// (`C:\Windows\...`, `C:\Program Files`, `C:\Program Files (x86)`,
/// `C:\ProgramData`, `C:\Users`, `C:\Users\<name>`).
fn clean_dir(raw: &Path, manual: bool) -> Option<PathBuf> {
    let p = normalize_path(raw);
    if !p.is_absolute() {
        return None;
    }
    let mut disk = false;
    let mut normals: Vec<String> = Vec::new();
    for c in p.components() {
        match c {
            Component::Prefix(pre) => {
                disk = matches!(
                    pre.kind(),
                    std::path::Prefix::Disk(_) | std::path::Prefix::VerbatimDisk(_)
                );
            }
            Component::Normal(n) => normals.push(n.to_string_lossy().to_lowercase()),
            Component::RootDir => {}
            _ => return None, // `..`, `.`
        }
    }
    if normals.is_empty() {
        return None;
    }
    // A single-component dir that is a well-known library root would swallow
    // every game in it. Manual entries are the user's explicit choice.
    if !manual && normals.len() == 1 && LIBRARY_ROOTS.contains(&normals[0].as_str()) {
        return None;
    }
    if !manual && is_launcher_default_folder(&normals) {
        return None;
    }
    if disk {
        let broad = match normals[0].as_str() {
            "windows" => true,
            "program files" | "program files (x86)" | "programdata" => normals.len() == 1,
            "users" => normals.len() <= 2,
            _ => false,
        };
        if broad {
            return None;
        }
    }
    Some(p)
}

/// Launcher-owned default library folders, as lowercase components below the
/// drive root (and below `Program Files` / `Program Files (x86)`). Auto sources
/// pointing exactly at one of these would swallow every game inside, so they
/// are rejected like a library root.
const LAUNCHER_DEFAULT_DIRS: &[&str] = &[
    "epic games",
    "ea games",
    "origin games",
    "xboxgames",
    "gog galaxy\\games",
    "steam\\steamapps\\common",
    "ubisoft\\ubisoft game launcher\\games",
    "electronic arts",
];

fn is_launcher_default_folder(normals: &[String]) -> bool {
    // `steamapps\common` below any folder is a Steam library's game folder.
    if let [.., a, b] = normals {
        if a == "steamapps" && b == "common" {
            return true;
        }
    }
    let rest = match normals.first().map(String::as_str) {
        Some("program files") | Some("program files (x86)") => &normals[1..],
        _ => normals,
    };
    let joined = rest.join("\\");
    LAUNCHER_DEFAULT_DIRS.contains(&joined.as_str())
}

fn last_component(p: &Path) -> Option<String> {
    p.components().rev().find_map(|c| match c {
        Component::Normal(n) => Some(n.to_string_lossy().into_owned()),
        _ => None,
    })
}

fn make_game(source: Source, name: Option<&str>, dir: &Path) -> Option<Game> {
    let install_dir = clean_dir(dir, source == Source::Manual)?;
    let name = name
        .map(str::trim)
        .filter(|n| !n.is_empty())
        .map(str::to_string)
        .or_else(|| last_component(&install_dir))?;
    Some(Game {
        name,
        source,
        install_dir,
    })
}

const LIBRARY_ROOTS: &[&str] = &[
    "games",
    "epic games",
    "gog games",
    "gog galaxy",
    "steamlibrary",
    "steam",
    "xboxgames",
    "ea games",
    "origin games",
    "ubisoft",
    "ubisoft game launcher",
];

// -------------------------------------------------------------------- Steam

fn scan_steam() -> Vec<Game> {
    let Some(root) =
        win32::reg_get_string(Hive::CurrentUser, "Software\\Valve\\Steam", "SteamPath")
    else {
        return Vec::new();
    };
    steam::scan(Path::new(&root))
        .into_iter()
        .filter_map(|g| make_game(Source::Steam, Some(&g.name), &g.install_dir))
        .collect()
}

// ---------------------------------------------------------------------- Epic

#[derive(Deserialize, Default)]
#[serde(default)]
struct EpicManifest {
    #[serde(rename = "DisplayName")]
    display_name: Option<String>,
    #[serde(rename = "InstallLocation")]
    install_location: Option<String>,
    #[serde(rename = "bIsIncompleteInstall")]
    incomplete: bool,
    #[serde(rename = "AppCategories")]
    categories: Option<Vec<String>>,
    #[serde(rename = "AppName")]
    app_name: Option<String>,
    #[serde(rename = "MainGameAppName")]
    main_game: Option<String>,
}

/// Parse one `*.item` manifest. Lenient: unknown fields are ignored, and a
/// missing or null `AppCategories` is accepted. Engine/plugin/addon entries
/// are rejected.
pub fn parse_epic_manifest(text: &str) -> Option<Game> {
    let m: EpicManifest = serde_json::from_str(text.trim_start_matches('\u{feff}')).ok()?;
    if m.incomplete {
        return None;
    }
    const NON_GAME: &[&str] = &["plugins", "plugins/engine", "engines", "addons"];
    let non_game = m
        .categories
        .iter()
        .flatten()
        .any(|c| NON_GAME.iter().any(|n| c.eq_ignore_ascii_case(n)));
    if non_game {
        return None;
    }
    // DLC manifests point at their base game's folder.
    if let (Some(a), Some(main)) = (&m.app_name, &m.main_game) {
        if !main.is_empty() && !a.eq_ignore_ascii_case(main) {
            return None;
        }
    }
    let loc = m.install_location.as_deref().filter(|l| !l.is_empty())?;
    make_game(Source::Epic, m.display_name.as_deref(), Path::new(loc))
}

fn scan_epic() -> Vec<Game> {
    let pd = std::env::var_os("ProgramData")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .unwrap_or_else(|| PathBuf::from("C:\\ProgramData"));
    scan_epic_dir(&pd.join("Epic\\EpicGamesLauncher\\Data\\Manifests"))
}

fn scan_epic_dir(dir: &Path) -> Vec<Game> {
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir(dir) else {
        return out;
    };
    for e in rd.flatten() {
        let path = e.path();
        let is_item = path
            .extension()
            .is_some_and(|x| x.eq_ignore_ascii_case("item"));
        if !is_item {
            continue;
        }
        if let Ok(bytes) = std::fs::read(&path) {
            if let Some(g) = parse_epic_manifest(&String::from_utf8_lossy(&bytes)) {
                out.push(g);
            }
        }
    }
    out
}

// ---------------------------------------------------- registry-based sources

/// GOG: values `gameName` and `path` of `...\GOG.com\Games\<id>`.
pub fn gog_game(get: impl Fn(&str) -> Option<String>) -> Option<Game> {
    let path = get("path")?;
    make_game(Source::Gog, get("gameName").as_deref(), Path::new(&path))
}

/// Ubisoft: value `InstallDir`; the name is the last folder component.
pub fn ubisoft_game(get: impl Fn(&str) -> Option<String>) -> Option<Game> {
    let dir = get("InstallDir")?;
    make_game(Source::Ubisoft, None, Path::new(&dir))
}

/// EA: value `Install Dir`; the name is the registry subkey name.
pub fn ea_game(key_name: &str, get: impl Fn(&str) -> Option<String>) -> Option<Game> {
    let dir = get("Install Dir")?;
    make_game(Source::Ea, Some(key_name), Path::new(&dir))
}

/// EA titles that register under a studio key (or only in Add/Remove
/// Programs): an `Uninstall` entry whose `Publisher` contains "Electronic Arts"
/// and that has an `InstallLocation`. The EA app / Origin themselves are skipped.
pub fn ea_uninstall_game(get: impl Fn(&str) -> Option<String>) -> Option<Game> {
    let publisher = get("Publisher")?;
    if !publisher.to_lowercase().contains("electronic arts") {
        return None;
    }
    let dir = get("InstallLocation").filter(|d| !d.trim().is_empty())?;
    let name = get("DisplayName");
    if let Some(n) = name.as_deref() {
        let l = n.trim().to_lowercase();
        const DENY: &[&str] = &[
            "anticheat",
            "anti-cheat",
            "anti cheat",
            "redistributable",
            "background service",
            "helper",
        ];
        if l == "origin"
            || l.starts_with("ea app")
            || l.starts_with("ea desktop")
            || DENY.iter().any(|d| l.contains(d))
        {
            return None;
        }
    }
    let norm = lower_key(Path::new(&dir));
    let last = norm.rsplit('\\').next().unwrap_or("");
    if matches!(last, "ea desktop" | "origin" | "ac")
        || norm.contains("\\electronic arts\\ea desktop")
    {
        return None;
    }
    make_game(Source::Ea, name.as_deref(), Path::new(&dir))
}

fn scan_ea_uninstall() -> Vec<Game> {
    let mut out = Vec::new();
    for base in [
        "SOFTWARE\\Microsoft\\Windows\\CurrentVersion\\Uninstall",
        "SOFTWARE\\WOW6432Node\\Microsoft\\Windows\\CurrentVersion\\Uninstall",
    ] {
        out.extend(scan_reg_subkeys(base, |_, get| ea_uninstall_game(get)));
    }
    out
}

fn scan_reg_subkeys(
    base: &str,
    mut parse: impl FnMut(&str, &dyn Fn(&str) -> Option<String>) -> Option<Game>,
) -> Vec<Game> {
    let mut out = Vec::new();
    for sub in win32::reg_subkeys(Hive::LocalMachine, base) {
        let key = format!("{base}\\{sub}");
        let get = |v: &str| win32::reg_get_string(Hive::LocalMachine, &key, v);
        if let Some(g) = parse(&sub, &get) {
            out.push(g);
        }
    }
    out
}

fn scan_gog() -> Vec<Game> {
    scan_reg_subkeys("SOFTWARE\\WOW6432Node\\GOG.com\\Games", |_, get| {
        gog_game(get)
    })
}

fn scan_ubisoft() -> Vec<Game> {
    scan_reg_subkeys(
        "SOFTWARE\\WOW6432Node\\Ubisoft\\Launcher\\Installs",
        |_, get| ubisoft_game(get),
    )
}

fn scan_ea() -> Vec<Game> {
    let mut out = Vec::new();
    for root in ["SOFTWARE\\WOW6432Node", "SOFTWARE"] {
        for name in ["Electronic Arts", "EA Games"] {
            out.extend(scan_reg_subkeys(&format!("{root}\\{name}"), |n, get| {
                ea_game(n, get)
            }));
        }
    }
    out.extend(scan_ea_uninstall());
    out // duplicates (same install dir) are removed by GameDb::new
}
// --------------------------------------------------------------------- Xbox

/// Games under one `<drive>:\XboxGames` folder: each subfolder that has a
/// `Content` subfolder.
fn xbox_games_in(xbox_root: &Path) -> Vec<Game> {
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir(xbox_root) else {
        return out;
    };
    for e in rd.flatten() {
        let dir = e.path();
        if !dir.join("Content").is_dir() {
            continue;
        }
        let name = e.file_name().to_string_lossy().into_owned();
        if let Some(g) = make_game(Source::Xbox, Some(&name), &dir) {
            out.push(g);
        }
    }
    out
}

fn fixed_drives() -> Vec<String> {
    use windows_sys::Win32::Storage::FileSystem::{GetDriveTypeW, GetLogicalDrives};
    const DRIVE_FIXED: u32 = 3;
    let mask = unsafe { GetLogicalDrives() };
    let mut out = Vec::new();
    for i in 0..26u32 {
        if mask & (1 << i) == 0 {
            continue;
        }
        let letter = (b'A' + i as u8) as char;
        let root: [u16; 4] = [letter as u16, ':' as u16, '\\' as u16, 0];
        if unsafe { GetDriveTypeW(root.as_ptr()) } == DRIVE_FIXED {
            out.push(format!("{letter}:\\"));
        }
    }
    out
}

/// Maximum number of folders accepted from a `.GamingRoot` file.
const GAMING_ROOT_MAX_FOLDERS: usize = 64;
/// Maximum `.GamingRoot` file size read (the real file is under 100 bytes).
const GAMING_ROOT_MAX_BYTES: u64 = 16 * 1024;

/// Parse a `.GamingRoot` file. Two header layouts are accepted, since the
/// real one is not verified (see #14):
/// (a) `RGBX`, LE u32 version, LE u32 count, then `count` NUL-terminated
///     UTF-16LE relative paths at offset 12 (used when count is 1..=64 and at
///     least one valid entry parses);
/// (b) `RGBX`, LE u32, then NUL-terminated UTF-16LE paths from offset 8 to EOF
///     (at most 64 entries).
/// Returns `None` for a bad magic. A truncated tail yields the complete
/// entries. Entries that are absolute, contain `..`, a drive prefix or control
/// characters, or are empty are dropped.
pub fn parse_gaming_root(data: &[u8]) -> Option<Vec<String>> {
    if data.len() < 8 || &data[..4] != b"RGBX" {
        return None;
    }
    if data.len() >= 12 {
        let count = u32::from_le_bytes(data[8..12].try_into().ok()?) as usize;
        if (1..=GAMING_ROOT_MAX_FOLDERS).contains(&count) {
            let v = parse_utf16_paths(&data[12..], count);
            if !v.is_empty() {
                return Some(v);
            }
        }
    }
    Some(parse_utf16_paths(&data[8..], GAMING_ROOT_MAX_FOLDERS))
}

/// Up to `max` NUL-terminated UTF-16LE strings; an unterminated tail is dropped.
fn parse_utf16_paths(data: &[u8], max: usize) -> Vec<String> {
    let mut out = Vec::new();
    let mut units: Vec<u16> = Vec::new();
    let mut pos = 0;
    for _ in 0..max {
        units.clear();
        let mut terminated = false;
        while pos + 2 <= data.len() {
            let u = u16::from_le_bytes([data[pos], data[pos + 1]]);
            pos += 2;
            if u == 0 {
                terminated = true;
                break;
            }
            units.push(u);
        }
        if !terminated {
            break;
        }
        if let Some(rel) = safe_relative(&String::from_utf16_lossy(&units)) {
            out.push(rel);
        }
    }
    out
}

/// A relative folder below a drive root: no drive, no leading separator, no
/// `..`/`.` components. Returned with `\` separators.
fn safe_relative(s: &str) -> Option<String> {
    let s = s.trim().replace('/', "\\");
    if s.contains(':') || s.starts_with('\\') || s.chars().any(char::is_control) {
        return None;
    }
    let parts: Vec<&str> = s.split('\\').filter(|p| !p.is_empty()).collect();
    if parts.is_empty() || parts.iter().any(|p| p.ends_with(['.', ' '])) {
        return None;
    }
    Some(parts.join("\\"))
}

/// Xbox library folders on one drive (relative names): `XboxGames` plus
/// whatever `.GamingRoot` lists, de-duplicated case-insensitively. A missing
/// or unparsable file falls back to `XboxGames` alone.
fn xbox_folders(gaming_root: Option<&[u8]>) -> Vec<String> {
    let mut out = vec!["XboxGames".to_string()];
    for f in gaming_root.and_then(parse_gaming_root).unwrap_or_default() {
        if !out.iter().any(|o| o.eq_ignore_ascii_case(&f)) {
            out.push(f);
        }
    }
    out
}

fn read_gaming_root(drive: &str) -> Option<Vec<u8>> {
    use std::io::Read;
    let f = std::fs::File::open(Path::new(drive).join(".GamingRoot")).ok()?;
    let mut buf = Vec::new();
    f.take(GAMING_ROOT_MAX_BYTES).read_to_end(&mut buf).ok()?;
    Some(buf)
}

/// Re-probe every fixed drive at least this often (seconds), so Xbox content
/// that appears later (first Game Pass install, a late-mounted drive) is found.
const XBOX_FULL_SWEEP_S: u64 = 6 * 3600;

#[derive(Debug, Clone, PartialEq, Eq)]
struct XboxCache {
    /// Drives that had a `.GamingRoot` or `XboxGames` at the last sweep.
    drives: Vec<String>,
    last_sweep: u64,
}

static XBOX_CACHE: std::sync::Mutex<Option<XboxCache>> = std::sync::Mutex::new(None);

/// Decide which drives to probe. Normally only the known Xbox drives, so idle
/// HDDs are not spun up every 30 minutes; on the first scan and when the last
/// full sweep is older than `XBOX_FULL_SWEEP_S`, all fixed drives.
/// Returns `(drives, full_sweep)`.
fn xbox_plan(cache: Option<&XboxCache>, now: u64, fixed: &[String]) -> (Vec<String>, bool) {
    match cache {
        Some(c) if now.saturating_sub(c.last_sweep) < XBOX_FULL_SWEEP_S => {
            (c.drives.clone(), false)
        }
        _ => (fixed.to_vec(), true),
    }
}

/// New cache after a scan. A full sweep replaces the set with what it found
/// (plus nothing stale); a fast scan keeps the cache unchanged.
fn xbox_update(cache: Option<XboxCache>, found: Vec<String>, full: bool, now: u64) -> XboxCache {
    match cache {
        Some(c) if !full => c,
        Some(c) => {
            let mut drives = c.drives;
            drives.retain(|d| found.contains(d));
            for d in found {
                if !drives.contains(&d) {
                    drives.push(d);
                }
            }
            XboxCache {
                drives,
                last_sweep: now,
            }
        }
        None => XboxCache {
            drives: found,
            last_sweep: now,
        },
    }
}

fn scan_xbox() -> Vec<Game> {
    static LOGGED_BAD_ROOT: std::sync::atomic::AtomicBool =
        std::sync::atomic::AtomicBool::new(false);
    let now = win32::now_utc_secs();
    let cache = XBOX_CACHE.lock().ok().and_then(|g| g.clone());
    let (drives, full) = xbox_plan(cache.as_ref(), now, &fixed_drives());
    let mut found_drives = Vec::new();
    let mut out = Vec::new();
    for d in drives {
        let gr = read_gaming_root(&d);
        let mut found = gr.is_some();
        if let Some(bytes) = gr.as_deref() {
            let empty = parse_gaming_root(bytes).is_some_and(|v| v.is_empty());
            if empty && !LOGGED_BAD_ROOT.swap(true, std::sync::atomic::Ordering::Relaxed) {
                let hex: String = bytes.iter().take(32).map(|b| format!("{b:02x}")).collect();
                crate::log::log_line(&format!("{d}.GamingRoot: no entries parsed: {hex}"));
            }
        }
        for f in xbox_folders(gr.as_deref()) {
            let root = Path::new(&d).join(&f);
            if full && !root.is_dir() {
                continue;
            }
            found = true;
            out.extend(xbox_games_in(&root));
        }
        if found {
            found_drives.push(d);
        }
    }
    if let Ok(mut g) = XBOX_CACHE.lock() {
        *g = Some(xbox_update(cache, found_drives, full, now));
    }
    out
}

// ------------------------------------------------------------------- Manual

fn manual_games(cfg: &Config) -> Vec<Game> {
    cfg.extra_games
        .iter()
        .filter(|(path, _)| !is_separator_only(path, "extra_games"))
        .filter_map(|(path, name)| make_game(Source::Manual, Some(name), Path::new(path)))
        .collect()
}

/// True (with a log warning) for keys made only of separators, which would
/// otherwise match everything.
fn is_separator_only(key: &str, field: &str) -> bool {
    let k = key.trim();
    let bad = !k.is_empty() && k.chars().all(|c| c == '\\' || c == '/');
    if bad {
        // Rescans repeat this every 30 minutes; warn once per process.
        static WARNED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
        if !WARNED.swap(true, std::sync::atomic::Ordering::Relaxed) {
            crate::log::log_line(&format!("config: ignoring {field} entry {key:?}"));
        }
    }
    bad
}

// ------------------------------------------------------------------ matcher

struct Entry {
    prefix: String, // lowercase, normalised
    game: usize,
}

pub struct GameDb {
    games: Vec<Game>,
    /// Sorted by prefix length descending, manual first on ties.
    entries: Vec<Entry>,
    ignore: Vec<String>,
    /// Lowercase bare file names from `ignore`.
    ignore_names: Vec<String>,
}

fn lower_key(p: &Path) -> String {
    normalize_path(p).to_string_lossy().to_lowercase()
}

/// True if `prefix` is a prefix of `path` on a component boundary.
fn has_prefix(path: &str, prefix: &str) -> bool {
    path.starts_with(prefix)
        && (path.len() == prefix.len()
            || prefix.ends_with('\\')
            || path.as_bytes()[prefix.len()] == b'\\')
}

const EXCLUDED_EXACT: &[&str] = &[
    "steam.exe",
    "steamwebhelper.exe",
    "steamservice.exe",
    "gameoverlayui.exe",
    "epicgameslauncher.exe",
    "epicwebhelper.exe",
    "upc.exe",
    "ubisoftconnect.exe",
    "eadesktop.exe",
    "eabackgroundservice.exe",
    "ealauncher.exe",
    "eacefsubprocess.exe",
    "origin.exe",
    "originwebhelperservice.exe",
    "originclientservice.exe",
    "originthinsetupinternal.exe",
    "originer.exe",
    "dxsetup.exe",
    "lockapp.exe",
    "explorer.exe",
    "shellexperiencehost.exe",
    "startmenuexperiencehost.exe",
    "searchhost.exe",
];

const EXCLUDED_PREFIX: &[&str] = &[
    "galaxyclient",
    "unitycrashhandler",
    "crashreport",
    "crashsender",
    "vc_redist",
    "dxsetup",
    "ue4prereqsetup",
];

/// `name` is a lowercase file name.
fn is_excluded_name(name: &str) -> bool {
    if !name.ends_with(".exe") {
        return false;
    }
    EXCLUDED_EXACT.contains(&name)
        || EXCLUDED_PREFIX.iter().any(|p| name.starts_with(p))
        || name.contains("crashpad")
}

impl GameDb {
    pub fn new(games: Vec<Game>, ignore: &[String]) -> GameDb {
        // Entries without a path separator are bare file names.
        let mut ignore_names = Vec::new();
        let mut paths = Vec::new();
        for s in ignore.iter().map(|s| s.trim()).filter(|s| !s.is_empty()) {
            if is_separator_only(s, "ignore") {
                continue;
            }
            let b = s.as_bytes();
            if b.len() == 2 && b[1] == b':' && b[0].is_ascii_alphabetic() {
                // `D:` means the whole drive.
                paths.push(format!("{}:\\", (b[0] as char).to_ascii_lowercase()));
            } else if s.contains(['\\', '/']) {
                paths.push(lower_key(Path::new(s)));
            } else {
                ignore_names.push(s.to_lowercase());
            }
        }
        let ignore = paths;

        // Manual entries first, so they win on dedup and on equal length.
        let mut all: Vec<(String, Game)> = games
            .into_iter()
            .map(|mut g| {
                g.install_dir = normalize_path(&g.install_dir);
                (lower_key(&g.install_dir), g)
            })
            .filter(|(k, _)| !ignore.iter().any(|i| has_prefix(k, i)))
            .collect();
        all.sort_by_key(|(_, g)| g.source != Source::Manual); // stable
        let mut seen = std::collections::HashSet::new();
        all.retain(|(k, _)| seen.insert(k.clone()));

        let mut entries: Vec<Entry> = all
            .iter()
            .enumerate()
            .map(|(i, (k, _))| Entry {
                prefix: k.clone(),
                game: i,
            })
            .collect();
        // `all` is manual-first, so a stable sort by length keeps the tie rule.
        entries.sort_by_key(|e| std::cmp::Reverse(e.prefix.len()));

        // Public list is sorted by name; remap entry indices accordingly.
        let mut order: Vec<usize> = (0..all.len()).collect();
        order.sort_by(|&a, &b| {
            let (ga, gb) = (&all[a].1, &all[b].1);
            ga.name
                .to_lowercase()
                .cmp(&gb.name.to_lowercase())
                .then_with(|| all[a].0.cmp(&all[b].0))
        });
        let mut pos = vec![0usize; all.len()];
        for (new, &old) in order.iter().enumerate() {
            pos[old] = new;
        }
        for e in &mut entries {
            e.game = pos[e.game];
        }
        let mut slots: Vec<Option<Game>> = all.into_iter().map(|(_, g)| Some(g)).collect();
        let games = order
            .iter()
            .map(|&o| slots[o].take().expect("each index used once"))
            .collect();

        GameDb {
            games,
            entries,
            ignore,
            ignore_names,
        }
    }

    /// All detected games, sorted by name.
    pub fn games(&self) -> &[Game] {
        &self.games
    }

    /// Map an exe path to its game. Allocation: the normalised `PathBuf`
    /// (plus its lossy `String` when non-UTF-8-clean) and the lowercase
    /// `String`; matching itself allocates nothing.
    pub fn match_exe(&self, exe: &Path) -> Option<&Game> {
        let lower = lower_key(exe);
        let name = lower.rsplit('\\').next().unwrap_or(&lower);
        if is_excluded_name(name) {
            return None;
        }
        if self.ignore_names.iter().any(|n| n == name)
            || self.ignore.iter().any(|i| has_prefix(&lower, i))
        {
            return None;
        }
        self.entries
            .iter()
            .find(|e| has_prefix(&lower, &e.prefix))
            .map(|e| &self.games[e.game])
    }
}

/// Scan all launchers on this machine plus `cfg.extra_games`.
pub fn scan_system(cfg: &Config) -> GameDb {
    let mut games = manual_games(cfg);
    games.extend(scan_steam());
    games.extend(scan_epic());
    games.extend(scan_gog());
    games.extend(scan_ubisoft());
    games.extend(scan_ea());
    games.extend(scan_xbox());
    GameDb::new(games, &cfg.ignore)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn g(name: &str, source: Source, dir: &str) -> Game {
        Game {
            name: name.into(),
            source,
            install_dir: PathBuf::from(dir),
        }
    }

    fn m(db: &GameDb, exe: &str) -> Option<String> {
        db.match_exe(Path::new(exe)).map(|g| g.name.clone())
    }

    const EPIC: &str = r#"{
        "FormatVersion": 0,
        "bIsIncompleteInstall": false,
        "LaunchExecutable": "Foo/Binaries/Win64/Foo.exe",
        "InstallLocation": "D:\\Epic Games\\Foo",
        "AppCategories": ["public", "games", "applications"],
        "AppName": "abc123",
        "DisplayName": "Foo Game",
        "MainGameAppName": "abc123",
        "InstallationGuid": "X"
    }"#;

    #[test]
    fn epic_manifest_ok() {
        let g = parse_epic_manifest(EPIC).unwrap();
        assert_eq!(g.name, "Foo Game");
        assert_eq!(g.source, Source::Epic);
        assert_eq!(g.install_dir, PathBuf::from("D:\\Epic Games\\Foo"));
        assert!(parse_epic_manifest(&format!("\u{feff}{EPIC}")).is_some());
    }

    #[test]
    fn epic_manifest_rejections() {
        let incomplete = EPIC.replace(
            "\"bIsIncompleteInstall\": false",
            "\"bIsIncompleteInstall\": true",
        );
        assert!(parse_epic_manifest(&incomplete).is_none());
        for cat in ["plugins", "plugins/engine", "engines", "addons"] {
            let bad = EPIC.replace("\"games\"", &format!("\"{cat}\""));
            assert!(parse_epic_manifest(&bad).is_none(), "{cat}");
        }
        // No "games" category is fine now.
        let other = EPIC.replace("\"games\"", "\"applications2\"");
        assert!(parse_epic_manifest(&other).is_some());
        let null = EPIC.replace("[\"public\", \"games\", \"applications\"]", "null");
        assert!(parse_epic_manifest(&null).is_some());
        let dlc = EPIC.replace(
            "\"MainGameAppName\": \"abc123\"",
            "\"MainGameAppName\": \"zzz\"",
        );
        assert!(parse_epic_manifest(&dlc).is_none());
        assert!(parse_epic_manifest("not json").is_none());
        assert!(parse_epic_manifest("{}").is_none());
    }

    #[test]
    fn epic_lenient() {
        let g = parse_epic_manifest(r#"{"InstallLocation":"E:/Games/Bar/"}"#).unwrap();
        assert_eq!(g.name, "Bar");
        assert_eq!(g.install_dir, PathBuf::from("E:\\Games\\Bar"));
    }

    fn map_get<'a>(m: &'a HashMap<&str, &str>) -> impl Fn(&str) -> Option<String> + 'a {
        move |k| m.get(k).map(|v| v.to_string())
    }

    #[test]
    fn gog_parser() {
        let vals = HashMap::from([
            ("gameName", "The Witcher 3"),
            ("path", "C:\\GOG Games\\The Witcher 3"),
        ]);
        let g = gog_game(map_get(&vals)).unwrap();
        assert_eq!(g.name, "The Witcher 3");
        assert_eq!(g.source, Source::Gog);
        assert!(gog_game(map_get(&HashMap::new())).is_none());
    }

    #[test]
    fn ubisoft_parser() {
        let vals = HashMap::from([(
            "InstallDir",
            "C:/Program Files (x86)/Ubisoft/Ubisoft Game Launcher/games/Far Cry 5/",
        )]);
        let g = ubisoft_game(map_get(&vals)).unwrap();
        assert_eq!(g.name, "Far Cry 5");
        assert_eq!(
            g.install_dir,
            PathBuf::from(
                "C:\\Program Files (x86)\\Ubisoft\\Ubisoft Game Launcher\\games\\Far Cry 5"
            )
        );
    }

    #[test]
    fn ea_parser() {
        let vals = HashMap::from([(
            "Install Dir",
            "C:\\Program Files\\EA Games\\Battlefield 4\\",
        )]);
        let g = ea_game("Battlefield 4", map_get(&vals)).unwrap();
        assert_eq!(g.name, "Battlefield 4");
        assert_eq!(g.source, Source::Ea);
        assert!(ea_game("X", map_get(&HashMap::new())).is_none());
    }

    #[test]
    fn unsafe_dirs_rejected() {
        for d in [
            "C:\\",
            "C:\\Games",
            "C:\\Program Files",
            "C:\\Program Files (x86)",
            "C:\\Windows\\System32",
            "C:\\Users\\bob",
            "relative\\dir",
            "\\rooted",
            "",
            "C:\\a\\..\\b",
        ] {
            assert!(clean_dir(Path::new(d), false).is_none(), "{d}");
        }
        assert!(clean_dir(Path::new("C:\\Program Files\\Foo"), false).is_some());
        assert!(clean_dir(Path::new("D:\\Games\\Foo"), false).is_some());
        assert!(clean_dir(Path::new("\\\\srv\\share\\Games\\Foo"), false).is_some());
        assert!(clean_dir(Path::new("\\\\?\\D:\\Games\\Foo"), false).is_some());
        // Manual: one level is fine, but never a broad folder.
        assert!(clean_dir(Path::new("D:\\Foo"), true).is_some());
        assert!(clean_dir(Path::new("C:\\Program Files"), true).is_none());
        assert!(clean_dir(Path::new("C:\\"), true).is_none());
    }

    fn sample_db() -> GameDb {
        GameDb::new(
            vec![
                g("Foo", Source::Steam, "C:\\Games\\Foo"),
                g("FooBar", Source::Epic, "C:\\Games\\FooBar"),
                g("Outer", Source::Gog, "D:\\Outer"),
                g("Inner", Source::Manual, "D:\\Outer\\Inner"),
                g("Zed", Source::Xbox, "E:\\XboxGames\\Zed"),
                g("Alpha", Source::Ea, "E:\\EA\\Alpha"),
            ],
            &["D:\\Outer\\Tools".to_string()],
        )
    }

    #[test]
    fn match_basic_and_case() {
        let db = sample_db();
        assert_eq!(
            m(&db, "C:\\Games\\Foo\\bin\\x64\\foo.exe").as_deref(),
            Some("Foo")
        );
        assert_eq!(m(&db, "c:\\games\\FOO\\Foo.EXE").as_deref(), Some("Foo"));
        assert_eq!(m(&db, "C:/Games/Foo/foo.exe").as_deref(), Some("Foo"));
        assert_eq!(m(&db, "C:\\Other\\foo.exe"), None);
    }

    #[test]
    fn match_component_boundary() {
        let db = sample_db();
        assert_eq!(
            m(&db, "C:\\Games\\FooBar\\x.exe").as_deref(),
            Some("FooBar")
        );
        assert_eq!(m(&db, "C:\\Games\\Foo2\\x.exe"), None);
        let only_foo = GameDb::new(vec![g("Foo", Source::Steam, "C:\\Games\\Foo")], &[]);
        assert_eq!(m(&only_foo, "C:\\Games\\FooBar\\x.exe"), None);
    }

    #[test]
    fn match_longest_prefix_and_manual_priority() {
        let db = sample_db();
        assert_eq!(m(&db, "D:\\Outer\\Inner\\a.exe").as_deref(), Some("Inner"));
        assert_eq!(m(&db, "D:\\Outer\\Other\\a.exe").as_deref(), Some("Outer"));
        let tie = GameDb::new(
            vec![
                g("Auto", Source::Steam, "D:\\Games\\X"),
                g("Mine", Source::Manual, "d:\\games\\x\\"),
            ],
            &[],
        );
        assert_eq!(m(&tie, "D:\\Games\\X\\x.exe").as_deref(), Some("Mine"));
        assert_eq!(tie.games().len(), 1);
    }

    #[test]
    fn match_manual_exe_entry() {
        let db = GameDb::new(vec![g("Solo", Source::Manual, "D:\\Apps\\solo.exe")], &[]);
        assert_eq!(m(&db, "D:\\Apps\\solo.exe").as_deref(), Some("Solo"));
        assert_eq!(m(&db, "D:\\Apps\\other.exe"), None);
    }

    #[test]
    fn epic_one_below_root() {
        let g =
            parse_epic_manifest(r#"{"DisplayName":"Fortnite","InstallLocation":"D:\\Fortnite"}"#)
                .unwrap();
        assert_eq!(g.install_dir, PathBuf::from("D:\\Fortnite"));
        assert!(
            parse_epic_manifest(r#"{"DisplayName":"X","InstallLocation":"D:\\Games"}"#).is_none()
        );
        let vals = HashMap::from([("InstallDir", "D:/Far Cry 5/")]);
        assert!(ubisoft_game(map_get(&vals)).is_some());
        let vals = HashMap::from([("path", "D:\\The Witcher 3"), ("gameName", "W3")]);
        assert!(gog_game(map_get(&vals)).is_some());
    }

    #[test]
    fn library_roots_auto_vs_manual() {
        for d in [
            "D:\\Games",
            "C:\\Games",
            "D:\\SteamLibrary",
            "D:\\Epic Games",
            "D:\\XboxGames",
        ] {
            assert!(clean_dir(Path::new(d), false).is_none(), "{d}");
            assert!(clean_dir(Path::new(d), true).is_some(), "{d}");
        }
        assert!(clean_dir(Path::new("D:\\Games\\Foo"), false).is_some());
    }

    #[test]
    fn library_root_spellings_and_launcher_defaults() {
        for d in [
            "D:/Games/",
            "\\\\?\\D:\\Games",
            "d:\\GAMES",
            "D:\\XBOXGAMES\\",
        ] {
            assert!(clean_dir(Path::new(d), false).is_none(), "{d}");
            assert!(clean_dir(Path::new(d), true).is_some(), "{d}");
        }
        for d in [
            "C:\\Program Files\\Epic Games",
            "C:\\Program Files (x86)\\Steam\\steamapps\\common",
            "E:\\Foo\\SteamApps\\Common",
            "C:\\Program Files\\EA Games",
            "C:\\Program Files (x86)\\GOG Galaxy\\Games",
            "C:\\Program Files (x86)\\Ubisoft\\Ubisoft Game Launcher\\games",
            "C:\\Program Files\\Electronic Arts",
            "D:\\Epic Games",
        ] {
            assert!(clean_dir(Path::new(d), false).is_none(), "{d}");
        }
        assert!(clean_dir(Path::new("C:\\Program Files\\Epic Games\\Foo"), false).is_some());
        assert!(clean_dir(
            Path::new("C:\\Program Files (x86)\\Steam\\steamapps\\common\\Foo"),
            false
        )
        .is_some());
    }

    #[test]
    fn ignore_bare_drive_and_separator_only() {
        let db = GameDb::new(
            vec![
                g("Bar", Source::Gog, "D:\\Bar"),
                g("Baz", Source::Gog, "E:\\Baz"),
            ],
            &["D:".to_string(), "\\".to_string(), "/".to_string()],
        );
        assert_eq!(m(&db, "D:\\Bar\\a.exe"), None);
        assert_eq!(m(&db, "E:\\Baz\\a.exe").as_deref(), Some("Baz"));
        assert!(is_separator_only("\\/", "ignore"));
        assert!(!is_separator_only("D:\\", "ignore"));
    }

    fn gaming_root(version: u32, count: u32, paths: &[&str]) -> Vec<u8> {
        let mut v = b"RGBX".to_vec();
        v.extend(version.to_le_bytes());
        v.extend(count.to_le_bytes());
        for p in paths {
            for u in p.encode_utf16().chain([0]) {
                v.extend(u.to_le_bytes());
            }
        }
        v
    }

    #[test]
    fn gaming_root_valid() {
        let d = gaming_root(0, 2, &["XboxGames", "Games\\Xbox"]);
        assert_eq!(
            parse_gaming_root(&d).unwrap(),
            vec!["XboxGames", "Games\\Xbox"]
        );
    }

    #[test]
    fn gaming_root_defensive() {
        let mut d = gaming_root(0, 2, &["XboxGames", "Second"]);
        d.truncate(d.len() - 4);
        assert_eq!(parse_gaming_root(&d).unwrap(), vec!["XboxGames"]);
        d.truncate(13); // odd length, mid-unit
        assert_eq!(parse_gaming_root(&d).unwrap(), Vec::<String>::new());
        assert!(parse_gaming_root(b"RGB").is_none());
        let mut bad = gaming_root(0, 1, &["X"]);
        bad[0] = b'X';
        assert!(parse_gaming_root(&bad).is_none());
        assert!(parse_gaming_root(&gaming_root(0, 0, &[]))
            .unwrap()
            .is_empty());
        // implausible count: falls to the layout-(b) reading, never panics
        let _ = parse_gaming_root(&gaming_root(0, 1_000_000, &["A"]));
        // unsafe entries dropped, other version tolerated
        let d = gaming_root(7, 4, &["..\\x", "C:\\x", "\\x", "Ok/Sub"]);
        assert_eq!(parse_gaming_root(&d).unwrap(), vec!["Ok\\Sub"]);
    }

    #[test]
    fn gaming_root_layout_b() {
        // RGBX + u32, then paths from offset 8
        let mut v = b"RGBX".to_vec();
        v.extend(1u32.to_le_bytes());
        for p in ["XboxGames", "Games\\Xbox"] {
            for u in p.encode_utf16().chain([0]) {
                v.extend(u.to_le_bytes());
            }
        }
        assert_eq!(
            parse_gaming_root(&v).unwrap(),
            vec!["XboxGames", "Games\\Xbox"]
        );
        // header only: magic ok, nothing parsed
        assert_eq!(parse_gaming_root(b"RGBX\0\0\0\0").unwrap().len(), 0);
    }

    #[test]
    fn safe_relative_rejects_dotty_components() {
        for bad in [
            "...", ".. ", "a\\b.", "a \\b", "..", ".", "a\\..\\b", "x\u{1}y",
        ] {
            assert!(safe_relative(bad).is_none(), "{bad:?}");
        }
        assert_eq!(safe_relative("A b\\C.d").as_deref(), Some("A b\\C.d"));
    }

    fn drives(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn xbox_plan_cases() {
        let fixed = drives(&["C:\\", "D:\\", "E:\\"]);
        // first scan: full sweep
        assert_eq!(xbox_plan(None, 1000, &fixed), (fixed.clone(), true));
        // fast rescan: known drives only
        let c = XboxCache {
            drives: drives(&["D:\\"]),
            last_sweep: 1000,
        };
        assert_eq!(
            xbox_plan(Some(&c), 1000 + 1800, &fixed),
            (drives(&["D:\\"]), false)
        );
        // sweep due
        let now = 1000 + XBOX_FULL_SWEEP_S;
        assert_eq!(xbox_plan(Some(&c), now, &fixed), (fixed.clone(), true));
        // merge new drives on a full sweep; vanished ones drop out
        let c2 = xbox_update(Some(c.clone()), drives(&["D:\\", "E:\\"]), true, now);
        assert_eq!(c2.drives, drives(&["D:\\", "E:\\"]));
        assert_eq!(c2.last_sweep, now);
        let c3 = xbox_update(Some(c2), drives(&["E:\\"]), true, now + 1);
        assert_eq!(c3.drives, drives(&["E:\\"]));
        // fast scan leaves the cache alone; first scan creates it
        let c4 = xbox_update(Some(c3.clone()), drives(&["Z:\\"]), false, now + 2);
        assert_eq!(c4, c3);
        assert_eq!(xbox_update(None, drives(&["D:\\"]), true, 5).last_sweep, 5);
    }

    #[test]
    fn xbox_folders_dedup_and_fallback() {
        let d = gaming_root(0, 2, &["xboxgames", "Other"]);
        assert_eq!(xbox_folders(Some(&d)), vec!["XboxGames", "Other"]);
        assert_eq!(xbox_folders(Some(b"junk")), vec!["XboxGames"]);
        assert_eq!(xbox_folders(None), vec!["XboxGames"]);
    }

    #[test]
    fn ea_uninstall_filtering() {
        let ok = HashMap::from([
            ("Publisher", "Electronic Arts, Inc."),
            ("DisplayName", "Mass Effect"),
            ("InstallLocation", "D:\\Mass Effect"),
        ]);
        let g = ea_uninstall_game(map_get(&ok)).unwrap();
        assert_eq!((g.name.as_str(), g.source), ("Mass Effect", Source::Ea));
        let with = |k: &'static str, v: &'static str| {
            let mut h = ok.clone();
            h.insert(k, v);
            h
        };
        assert!(ea_uninstall_game(map_get(&with("Publisher", "Valve"))).is_none());
        assert!(ea_uninstall_game(map_get(&with("InstallLocation", "D:\\Games"))).is_none());
        assert!(ea_uninstall_game(map_get(&with("InstallLocation", " "))).is_none());
        assert!(ea_uninstall_game(map_get(&with(
            "InstallLocation",
            "C:\\Program Files\\Electronic Arts"
        )))
        .is_none());
        assert!(ea_uninstall_game(map_get(&with("DisplayName", "EA app"))).is_none());
        for n in [
            "Foo Anticheat",
            "Foo Anti-Cheat",
            "EA Anti Cheat",
            "VC Redistributable",
            "EA Background Service",
            "EA Helper",
        ] {
            assert!(
                ea_uninstall_game(map_get(&with("DisplayName", n))).is_none(),
                "{n}"
            );
        }
        for loc in [
            "D:\\Stuff\\EA Desktop",
            "D:\\Stuff\\Origin",
            "D:\\Foo\\AC",
            "C:\\Program Files\\Electronic Arts\\EA Desktop\\EA Desktop",
            "C:\\Program Files\\Electronic Arts\\EA Desktop\\x",
        ] {
            assert!(
                ea_uninstall_game(map_get(&with("InstallLocation", loc))).is_none(),
                "{loc}"
            );
        }
        let mut v = ok.clone();
        v.remove("InstallLocation");
        assert!(ea_uninstall_game(map_get(&v)).is_none());
    }

    #[test]
    fn match_ignore_names_and_drive() {
        let db = GameDb::new(
            vec![
                g("Foo", Source::Steam, "C:\\Games\\Foo"),
                g("Bar", Source::Gog, "D:\\Bar"),
            ],
            &["Foo.EXE".to_string(), "D:\\".to_string()],
        );
        assert_eq!(m(&db, "C:\\Games\\Foo\\foo.exe"), None);
        assert_eq!(m(&db, "C:\\Games\\Foo\\other.exe").as_deref(), Some("Foo"));
        assert_eq!(m(&db, "D:\\Bar\\a.exe"), None);
        assert!(db.games().iter().all(|g| g.name != "Bar"));
    }

    #[test]
    fn ea_exclusions_and_origin_exact() {
        let db = GameDb::new(vec![g("Foo", Source::Steam, "C:\\Games\\Foo")], &[]);
        for e in [
            "EALauncher.exe",
            "EACefSubProcess.exe",
            "OriginThinSetupInternal.exe",
            "Originer.exe",
        ] {
            assert_eq!(m(&db, &format!("C:\\Games\\Foo\\{e}")), None, "{e}");
        }
        assert_eq!(
            m(&db, "C:\\Games\\Foo\\OriginQuest.exe").as_deref(),
            Some("Foo")
        );
    }

    #[test]
    fn match_ignore() {
        let db = sample_db();
        assert_eq!(m(&db, "D:\\Outer\\Tools\\t.exe"), None);
        assert_eq!(m(&db, "d:\\outer\\tools\\sub\\t.exe"), None);
        assert_eq!(m(&db, "D:\\Outer\\ToolsX\\t.exe").as_deref(), Some("Outer"));
    }

    #[test]
    fn match_exclusions() {
        let db = GameDb::new(vec![g("Foo", Source::Steam, "C:\\Games\\Foo")], &[]);
        for exe in [
            "steam.exe",
            "Steam.exe",
            "steamwebhelper.exe",
            "EpicGamesLauncher.exe",
            "GalaxyClient Helper.exe",
            "UnityCrashHandler64.exe",
            "CrashReportClient.exe",
            "crashpad_handler.exe",
            "vc_redist.x64.exe",
            "DXSETUP.exe",
            "UE4PrereqSetup_x64.exe",
            "Origin.exe",
            "UbisoftConnect.exe",
            "LockApp.exe",
            "explorer.exe",
            "SearchHost.exe",
        ] {
            let p = format!("C:\\Games\\Foo\\{exe}");
            assert_eq!(m(&db, &p), None, "{exe}");
        }
        assert_eq!(
            m(&db, "C:\\Games\\Foo\\FooLauncher.exe").as_deref(),
            Some("Foo")
        );
        assert_eq!(m(&db, "C:\\Games\\Foo\\originals.dat"), Some("Foo".into()));
    }

    #[test]
    fn lockapp_even_in_game_dir() {
        let db = GameDb::new(
            vec![g("Win", Source::Manual, "C:\\Windows\\SystemApps\\Foo")],
            &[],
        );
        // Windows dirs are rejected by scanners, but the matcher still must
        // never report the lock screen.
        assert_eq!(m(&db, "C:\\Windows\\SystemApps\\Foo\\LockApp.exe"), None);
    }

    #[test]
    fn match_unc_verbatim_and_dot_segments() {
        let db = GameDb::new(
            vec![
                g("Net", Source::Manual, "\\\\srv\\share\\Games\\Net"),
                g("Foo", Source::Steam, "C:\\Games\\Foo"),
            ],
            &[],
        );
        assert_eq!(
            m(&db, "\\\\?\\UNC\\SRV\\share\\games\\net\\a.exe").as_deref(),
            Some("Net")
        );
        assert_eq!(
            m(&db, "\\\\srv\\share\\Games\\Net\\a.exe").as_deref(),
            Some("Net")
        );
        assert_eq!(
            m(&db, "\\\\?\\C:\\Games\\Foo\\a.exe").as_deref(),
            Some("Foo")
        );
        assert_eq!(
            m(&db, "C:\\Games\\.\\Foo\\.\\a.exe").as_deref(),
            Some("Foo")
        );
    }

    #[test]
    fn games_sorted_by_name() {
        let db = sample_db();
        let names: Vec<_> = db.games().iter().map(|g| g.name.as_str()).collect();
        assert_eq!(
            names,
            vec!["Alpha", "Foo", "FooBar", "Inner", "Outer", "Zed"]
        );
    }

    #[test]
    fn ignored_games_not_listed() {
        let db = GameDb::new(
            vec![
                g("A", Source::Steam, "C:\\Games\\A"),
                g("B", Source::Steam, "C:\\Games\\B"),
            ],
            &["c:\\games\\b".to_string()],
        );
        assert_eq!(db.games().len(), 1);
    }

    #[test]
    fn source_display_and_serde() {
        assert_eq!(Source::Gog.to_string(), "gog");
        assert_eq!(
            serde_json::to_string(&Source::Ubisoft).unwrap(),
            "\"ubisoft\""
        );
        assert_eq!(serde_json::to_string(&Source::Ea).unwrap(), "\"ea\"");
    }

    struct Tmp(PathBuf);
    impl Drop for Tmp {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn xbox_scan_fake_root() {
        let base = std::env::temp_dir().join(format!("gt_detect_{}_xbox", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let _g = Tmp(base.clone());
        let root = base.join("XboxGames");
        std::fs::create_dir_all(root.join("Halo\\Content")).unwrap();
        std::fs::create_dir_all(root.join("GamingServices")).unwrap();
        std::fs::write(root.join("file.txt"), "x").unwrap();
        let games = xbox_games_in(&root);
        assert_eq!(games.len(), 1);
        assert_eq!(games[0].name, "Halo");
        assert_eq!(games[0].install_dir, root.join("Halo"));
    }

    #[test]
    fn epic_scan_fake_dir() {
        let base = std::env::temp_dir().join(format!("gt_detect_{}_epic", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let _g = Tmp(base.clone());
        std::fs::create_dir_all(&base).unwrap();
        std::fs::write(base.join("A.item"), EPIC).unwrap();
        std::fs::write(base.join("B.txt"), EPIC).unwrap();
        std::fs::write(base.join("C.item"), "garbage").unwrap();
        assert_eq!(scan_epic_dir(&base).len(), 1);
    }

    #[test]
    fn live_scan_does_not_panic() {
        let db = scan_system(&Config::default());
        for g in db.games() {
            assert!(g.install_dir.is_absolute(), "{:?}", g.install_dir);
        }
    }
}
