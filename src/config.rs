//! Configuration file handling (`config.json` in the data directory).

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};

const CONFIG_FILE: &str = "config.json";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub merge_gap_s: u64,
    pub autostart: bool,
    pub export_dir: Option<PathBuf>,
    pub export_interval_min: u64,
    pub http_port: Option<u16>,
    /// exe/dir path -> display name
    pub extra_games: BTreeMap<String, String>,
    pub ignore: Vec<String>,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            merge_gap_s: 60,
            autostart: true,
            export_dir: None,
            export_interval_min: 15,
            http_port: None,
            extra_games: BTreeMap::new(),
            ignore: Vec::new(),
        }
    }
}

/// `%APPDATA%\GameTime`, created if missing.
pub fn data_dir() -> io::Result<PathBuf> {
    // An empty or relative %APPDATA% is treated as unset.
    let appdata = std::env::var_os("APPDATA")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(crate::win32::roaming_appdata)
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no roaming AppData folder"))?;
    let dir = appdata.join("GameTime");
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

impl Config {
    /// Load `config.json` from `dir`. A missing file is created with defaults;
    /// a malformed file yields an error and is left untouched.
    pub fn load_or_create(dir: &Path) -> io::Result<Config> {
        let path = dir.join(CONFIG_FILE);
        match std::fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes).map_err(|e| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("{}: {e}", path.display()),
                )
            }),
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                let cfg = Config::default();
                cfg.save(dir)?;
                Ok(cfg)
            }
            Err(e) => Err(e),
        }
    }

    pub fn save(&self, dir: &Path) -> io::Result<()> {
        std::fs::create_dir_all(dir)?;
        let json = serde_json::to_string_pretty(self).map_err(io::Error::other)?;
        std::fs::write(dir.join(CONFIG_FILE), json)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_subdir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("gametime-test-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn round_trip() {
        let dir = temp_subdir("cfg-roundtrip");
        let mut cfg = Config {
            merge_gap_s: 5,
            http_port: Some(8080),
            export_dir: Some(PathBuf::from("C:\\x")),
            ..Config::default()
        };
        cfg.extra_games.insert("C:\\g\\a.exe".into(), "A".into());
        cfg.ignore.push("b.exe".into());
        cfg.save(&dir).unwrap();
        assert_eq!(Config::load_or_create(&dir).unwrap(), cfg);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_file_writes_defaults() {
        let dir = temp_subdir("cfg-missing");
        let cfg = Config::load_or_create(&dir).unwrap();
        assert_eq!(cfg, Config::default());
        assert!(dir.join(CONFIG_FILE).exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn partial_file_uses_defaults() {
        let dir = temp_subdir("cfg-partial");
        std::fs::write(dir.join(CONFIG_FILE), r#"{"merge_gap_s": 10}"#).unwrap();
        let cfg = Config::load_or_create(&dir).unwrap();
        assert_eq!(cfg.merge_gap_s, 10);
        assert!(cfg.autostart);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn malformed_file_errors_and_is_untouched() {
        let dir = temp_subdir("cfg-bad");
        let path = dir.join(CONFIG_FILE);
        std::fs::write(&path, "{ not json").unwrap();
        assert!(Config::load_or_create(&dir).is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "{ not json");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
