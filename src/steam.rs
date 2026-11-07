//! Steam library discovery from `libraryfolders.vdf` and `appmanifest_*.acf`.
//! The registry lookup of the Steam root is done elsewhere.

use crate::vdf::{self, Vdf};
use std::path::{Path, PathBuf};

const REDISTRIBUTABLES_APPID: &str = "228980";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SteamGame {
    pub appid: String,
    pub name: String,
    pub install_dir: PathBuf,
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
            out.push(PathBuf::from(p));
        }
    }
    out
}

pub fn library_paths(steam_root: &Path) -> Vec<PathBuf> {
    let mut out = vec![steam_root.to_path_buf()];
    let file = steam_root.join("steamapps").join("libraryfolders.vdf");
    if let Ok(text) = std::fs::read_to_string(file) {
        if let Ok(root) = vdf::parse(&text) {
            for p in paths_from_vdf(&root) {
                push_unique(&mut out, p);
            }
        }
    }
    out
}

fn game_from_manifest(lib: &Path, text: &str) -> Option<SteamGame> {
    let root = vdf::parse(text).ok()?;
    let st = root.get("AppState")?;
    let appid = st.get("appid")?.as_str()?.trim();
    let name = st.get("name")?.as_str()?;
    let dir = st.get("installdir")?.as_str()?;
    if appid.is_empty() || dir.is_empty() || appid == REDISTRIBUTABLES_APPID {
        return None;
    }
    Some(SteamGame {
        appid: appid.to_string(),
        name: name.to_string(),
        install_dir: lib.join("steamapps").join("common").join(dir),
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
        if let Ok(text) = std::fs::read_to_string(entry.path()) {
            if let Some(g) = game_from_manifest(lib, &text) {
                games.push(g);
            }
        }
    }
    games.sort_by(|a, b| a.appid.cmp(&b.appid));
    games
}

pub fn scan(steam_root: &Path) -> Vec<SteamGame> {
    library_paths(steam_root)
        .iter()
        .flat_map(|l| scan_library(l))
        .collect()
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
    fn missing_root_is_empty() {
        let p = std::env::temp_dir().join("gt_steam_nonexistent_xyz");
        assert!(scan(&p).is_empty());
        assert_eq!(library_paths(&p), vec![p]);
    }
}
