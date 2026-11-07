//! Steam library discovery from `libraryfolders.vdf` and `appmanifest_*.acf`.
//! The registry lookup of the Steam root is done elsewhere.

use crate::vdf::{self, Vdf};
use std::path::{Component, Path, PathBuf};

const REDISTRIBUTABLES_APPID: &str = "228980";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SteamGame {
    pub appid: String,
    pub name: String,
    pub install_dir: PathBuf,
}

/// Normalise a Windows path textually: `/` becomes `\`, repeated separators
/// collapse (a leading `\\` for UNC is kept), and trailing separators are
/// stripped except for a drive root such as `C:\`.
pub fn normalize_path(p: &Path) -> PathBuf {
    let s = p.to_string_lossy().replace('/', "\\");
    let unc = s.starts_with("\\\\");
    let mut out = String::with_capacity(s.len());
    if unc {
        out.push('\\');
    }
    let mut prev_sep = false;
    for ch in s.chars() {
        if ch == '\\' {
            if prev_sep {
                continue;
            }
            prev_sep = true;
        } else {
            prev_sep = false;
        }
        out.push(ch);
    }
    while out.ends_with('\\') {
        let is_drive_root = out.len() == 3 && out.as_bytes()[1] == b':';
        if is_drive_root || out.len() <= 2 {
            break;
        }
        out.pop();
    }
    PathBuf::from(out)
}

fn key_of(p: &Path) -> String {
    p.to_string_lossy().to_lowercase()
}

fn push_unique(out: &mut Vec<PathBuf>, p: PathBuf) {
    let k = key_of(&p);
    if !out.iter().any(|e| key_of(e) == k) {
        out.push(p);
    }
}

fn read_lossy(p: &Path) -> Option<String> {
    let bytes = std::fs::read(p).ok()?;
    Some(String::from_utf8_lossy(&bytes).into_owned())
}

fn paths_from_vdf(root: &Vdf) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let Some(folders) = root.get("libraryfolders") else {
        return out;
    };
    for (k, v) in folders.iter_obj() {
        if k.is_empty() || !k.bytes().all(|b| b.is_ascii_digit()) {
            continue;
        }
        let path = match v {
            Vdf::Str(s) => Some(s.as_str()),
            Vdf::Obj(_) => v.get("path").and_then(Vdf::as_str),
        };
        if let Some(p) = path.filter(|p| !p.is_empty()) {
            out.push(normalize_path(Path::new(p)));
        }
    }
    out
}

pub fn library_paths(steam_root: &Path) -> Vec<PathBuf> {
    let mut out = vec![normalize_path(steam_root)];
    let file = steam_root.join("steamapps").join("libraryfolders.vdf");
    if let Some(text) = read_lossy(&file) {
        if let Ok(root) = vdf::parse(&text) {
            for p in paths_from_vdf(&root) {
                push_unique(&mut out, p);
            }
        }
    }
    out
}

/// `installdir` must be a relative path made only of normal components.
fn safe_installdir(dir: &str) -> bool {
    !dir.is_empty()
        && Path::new(dir)
            .components()
            .all(|c| matches!(c, Component::Normal(_)))
}

fn game_from_manifest(lib: &Path, text: &str) -> Option<SteamGame> {
    let root = vdf::parse(text).ok()?;
    let st = root.get("AppState")?;
    let appid = st.get("appid")?.as_str()?.trim();
    let dir = st.get("installdir")?.as_str()?;
    if appid.is_empty() || appid == REDISTRIBUTABLES_APPID || !safe_installdir(dir) {
        return None;
    }
    let name = st
        .get("name")
        .and_then(Vdf::as_str)
        .map(str::trim)
        .filter(|n| !n.is_empty())
        .unwrap_or(dir);
    Some(SteamGame {
        appid: appid.to_string(),
        name: name.to_string(),
        install_dir: normalize_path(&lib.join("steamapps").join("common").join(dir)),
    })
}

pub fn scan_library(lib: &Path) -> Vec<SteamGame> {
    let mut games = Vec::new();
    let Ok(rd) = std::fs::read_dir(lib.join("steamapps")) else {
        return games;
    };
    for entry in rd.flatten() {
        let fname = entry.file_name();
        let Some(n) = fname.to_str().map(str::to_ascii_lowercase) else {
            continue;
        };
        if !(n.starts_with("appmanifest_") && n.ends_with(".acf")) {
            continue;
        }
        if let Some(text) = read_lossy(&entry.path()) {
            if let Some(g) = game_from_manifest(lib, &text) {
                games.push(g);
            }
        }
    }
    games
}

fn appid_order(a: &str, b: &str) -> std::cmp::Ordering {
    match (a.parse::<u64>(), b.parse::<u64>()) {
        (Ok(x), Ok(y)) => x.cmp(&y),
        _ => a.cmp(b),
    }
}

pub fn scan(steam_root: &Path) -> Vec<SteamGame> {
    let mut games: Vec<SteamGame> = library_paths(steam_root)
        .iter()
        .flat_map(|l| scan_library(l))
        .collect();
    games.sort_by(|a, b| appid_order(&a.appid, &b.appid).then_with(|| a.appid.cmp(&b.appid)));
    let mut seen = std::collections::HashSet::new();
    games.retain(|g| seen.insert((g.appid.clone(), key_of(&g.install_dir))));
    games
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    const MODERN: &str = r#""libraryfolders"
{
	"0"
	{
		"path"		"C:\\Program Files (x86)\\Steam"
		"label"		""
		"contentid"		"1234567890"
		"totalsize"		"0"
		"apps"
		{
			"228980"		"123456"
		}
	}
	"1"
	{
		"path"		"D:\\SteamLibrary"
		"label"		""
		"contentid"		"987654321"
		"totalsize"		"500107862016"
		"apps"
		{
			"730"		"30000000000"
		}
	}
}
"#;

    const LEGACY: &str = "\"LibraryFolders\"\n{\n\t\"TimeNextStatsReport\"\t\t\"1234\"\n\t\"ContentStatsID\"\t\t\"-55\"\n\t\"1\"\t\t\"E:\\\\Games\\\\Steam\"\n\t\"2\"\t\t\"e:\\\\games\\\\steam\"\n}\n";

    fn acf(appid: &str, name: &str, dir: &str) -> String {
        format!(
            "\"AppState\"\n{{\n\t\"appid\"\t\t\"{appid}\"\n\t\"Universe\"\t\t\"1\"\n\t\"name\"\t\t\"{name}\"\n\t\"installdir\"\t\t\"{dir}\"\n\t\"StateFlags\"\t\t\"4\"\n\t\"UserConfig\"\n\t{{\n\t\t\"language\"\t\t\"english\"\n\t}}\n\t\"MountedDepots\"\n\t{{\n\t\t\"731\"\t\t\"1234567890123456789\"\n\t}}\n}}\n"
        )
    }

    #[test]
    fn modern_format() {
        let root = vdf::parse(MODERN).unwrap();
        let p = paths_from_vdf(&root);
        assert_eq!(
            p,
            vec![
                PathBuf::from("C:\\Program Files (x86)\\Steam"),
                PathBuf::from("D:\\SteamLibrary")
            ]
        );
    }

    #[test]
    fn legacy_format() {
        let root = vdf::parse(LEGACY).unwrap();
        let p = paths_from_vdf(&root);
        assert_eq!(p.len(), 2);
        assert_eq!(p[0], PathBuf::from("E:\\Games\\Steam"));
    }

    #[test]
    fn legacy_dedup_case_insensitive() {
        let mut out = Vec::new();
        let root = vdf::parse(LEGACY).unwrap();
        for p in paths_from_vdf(&root) {
            push_unique(&mut out, p);
        }
        assert_eq!(out.len(), 1);
    }

    #[test]
    fn manifest_parsing() {
        let lib = Path::new("D:\\L");
        let g = game_from_manifest(lib, &acf("730", "Counter-Strike 2", "CS2")).unwrap();
        assert_eq!(g.appid, "730");
        assert_eq!(g.name, "Counter-Strike 2");
        assert_eq!(
            g.install_dir,
            lib.join("steamapps").join("common").join("CS2")
        );
        assert!(game_from_manifest(lib, &acf("228980", "Redist", "x")).is_none());
    }

    struct Tmp(PathBuf);
    impl Drop for Tmp {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn scan_fake_root() {
        let base = std::env::temp_dir().join(format!("gt_steam_{}_scan", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        let guard = Tmp(base.clone());
        let root = base.join("Steam");
        let lib2 = base.join("Lib2");
        fs::create_dir_all(root.join("steamapps")).unwrap();
        fs::create_dir_all(lib2.join("steamapps")).unwrap();
        let vdf_text = format!(
            "\"libraryfolders\"\n{{\n\"0\"\n{{\n\"path\" \"{}\"\n}}\n\"1\"\n{{\n\"path\" \"{}\"\n}}\n}}\n",
            root.to_string_lossy().replace('\\', "\\\\"),
            lib2.to_string_lossy().replace('\\', "\\\\")
        );
        let sa = root.join("steamapps");
        fs::write(sa.join("libraryfolders.vdf"), vdf_text).unwrap();
        fs::write(sa.join("appmanifest_10.acf"), acf("10", "Game A", "GameA")).unwrap();
        fs::write(
            sa.join("appmanifest_228980.acf"),
            acf(
                "228980",
                "Steamworks Common Redistributables",
                "Steamworks Shared",
            ),
        )
        .unwrap();
        fs::write(sa.join("appmanifest_99.acf"), "\"AppState\" { \"appid\" ").unwrap();
        let sb = lib2.join("steamapps");
        fs::write(sb.join("appmanifest_20.acf"), acf("20", "Game B", "GameB")).unwrap();
        fs::write(sb.join("other.txt"), "x").unwrap();

        assert_eq!(library_paths(&root).len(), 2);
        let games = scan(&root);
        let ids: Vec<_> = games.iter().map(|g| g.appid.as_str()).collect();
        assert_eq!(ids, vec!["10", "20"]);
        assert_eq!(games[1].install_dir, sb.join("common").join("GameB"));
        drop(guard);
        assert!(!base.exists());
    }

    #[test]
    fn normalize_cases() {
        let n = |s: &str| normalize_path(Path::new(s));
        assert_eq!(
            n("c:/program files (x86)/steam"),
            PathBuf::from("c:\\program files (x86)\\steam")
        );
        assert_eq!(n("C:\\Steam\\"), PathBuf::from("C:\\Steam"));
        assert_eq!(n("C:\\\\a//b\\\\"), PathBuf::from("C:\\a\\b"));
        assert_eq!(n("C:\\"), PathBuf::from("C:\\"));
        assert_eq!(n("C:/"), PathBuf::from("C:\\"));
        assert_eq!(n("\\\\srv\\share\\"), PathBuf::from("\\\\srv\\share"));
    }

    #[test]
    fn unsafe_installdir_rejected() {
        let lib = Path::new("D:\\L");
        for d in ["", "..\\x", "a\\..\\b", "C:\\Windows", "\\abs", "/abs"] {
            assert!(game_from_manifest(lib, &acf("5", "N", d)).is_none(), "{d}");
        }
        assert!(game_from_manifest(lib, &acf("5", "N", "sub\\dir")).is_some());
    }

    #[test]
    fn empty_name_falls_back_to_installdir() {
        let g = game_from_manifest(Path::new("D:\\L"), &acf("5", "", "Dir")).unwrap();
        assert_eq!(g.name, "Dir");
    }

    #[test]
    fn root_spelling_variants_give_one_library() {
        let base = std::env::temp_dir().join(format!("gt_steam_{}_norm", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        let _guard = Tmp(base.clone());
        let root = base.join("Steam");
        let sa = root.join("steamapps");
        fs::create_dir_all(&sa).unwrap();
        let root_s = root.to_string_lossy().to_string();
        let vdf_text = format!(
            "\"libraryfolders\"\n{{\n\"0\"\n{{\n\"path\" \"{}\"\n}}\n}}\n",
            root_s.replace('\\', "\\\\")
        );
        // Invalid UTF-8 byte in a comment must not drop the file.
        let mut bytes = vdf_text.into_bytes();
        bytes.extend_from_slice(b"// \xff\n");
        fs::write(sa.join("libraryfolders.vdf"), bytes).unwrap();
        let mut m = acf("7", "Caf\u{e9} \u{65e5}\u{672c}", "G7").into_bytes();
        m.extend_from_slice(b"// \xfe\n");
        fs::write(sa.join("appmanifest_7.acf"), m).unwrap();
        fs::write(sa.join("appmanifest_100.acf"), acf("100", "Hundred", "H")).unwrap();
        fs::write(sa.join("appmanifest_9.acf"), acf("9", "Nine", "N")).unwrap();

        let variant = format!("{}/", root_s.to_lowercase().replace('\\', "/"));
        let libs = library_paths(Path::new(&variant));
        assert_eq!(libs.len(), 1);
        let games = scan(Path::new(&variant));
        let ids: Vec<_> = games.iter().map(|g| g.appid.as_str()).collect();
        assert_eq!(ids, vec!["7", "9", "100"]);
        assert_eq!(games[0].name, "Caf\u{e9} \u{65e5}\u{672c}");
        for g in &games {
            assert!(!g.install_dir.to_string_lossy().contains('/'));
        }
    }

    #[test]
    fn missing_root_is_empty() {
        let p = std::env::temp_dir().join("gt_steam_nonexistent_xyz");
        assert!(scan(&p).is_empty());
        assert_eq!(library_paths(&p), vec![p]);
    }
}
