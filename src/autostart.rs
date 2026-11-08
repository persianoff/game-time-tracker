//! Autostart via the HKCU Run key, with StartupApproved awareness and
//! self-heal. The tracker never re-enables an entry the user disabled in
//! Task Manager.

use std::io;
use std::path::{Path, PathBuf};

use crate::config::{self, Config};
use crate::log;
use crate::win32::{self, Hive};

pub const VALUE_NAME: &str = "GameTime";
const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
const APPROVED_KEY: &str =
    r"Software\Microsoft\Windows\CurrentVersion\Explorer\StartupApproved\Run";
const TRACKER_EXE: &str = "gametime-tracker.exe";

/// Registry key paths (HKCU), parameterised so tests can use a scratch key.
#[derive(Clone, Copy)]
struct Keys<'a> {
    run: &'a str,
    approved: &'a str,
}

const REAL_KEYS: Keys<'static> = Keys {
    run: RUN_KEY,
    approved: APPROVED_KEY,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Status {
    pub registered: Option<String>,
    pub matches: bool,
    pub disabled_by_user: bool,
    pub config_autostart: bool,
}

/// The Run value for a tracker path: the absolute path, quoted.
fn run_value(tracker_exe: &Path) -> String {
    format!("\"{}\"", tracker_exe.display())
}

/// Parse a StartupApproved binary value. The first byte's low bit set (0x03,
/// 0x07) means the user disabled the entry; even (0x02, 0x06) means enabled.
/// Missing or empty data counts as enabled.
pub fn parse_startup_approved(data: Option<&[u8]>) -> bool {
    matches!(data.and_then(|d| d.first()), Some(b) if b & 1 == 1)
}

fn status_in(keys: Keys, tracker_exe: &Path, config_autostart: bool) -> Status {
    let registered = win32::reg_get_string(Hive::CurrentUser, keys.run, VALUE_NAME);
    let expected = run_value(tracker_exe);
    let matches = registered
        .as_deref()
        .is_some_and(|r| r.eq_ignore_ascii_case(&expected));
    let approved = win32::reg_get_binary(Hive::CurrentUser, keys.approved, VALUE_NAME);
    Status {
        registered,
        matches,
        disabled_by_user: parse_startup_approved(approved.as_deref()),
        config_autostart,
    }
}

fn enable_in(keys: Keys, tracker_exe: &Path) -> io::Result<()> {
    win32::reg_set_string(
        Hive::CurrentUser,
        keys.run,
        VALUE_NAME,
        &run_value(tracker_exe),
    )
}

fn disable_in(keys: Keys) -> io::Result<()> {
    match win32::reg_delete_value(Hive::CurrentUser, keys.run, VALUE_NAME) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        other => other,
    }
}

pub fn status(tracker_exe: &Path, config_autostart: bool) -> Status {
    status_in(REAL_KEYS, tracker_exe, config_autostart)
}

pub fn enable(tracker_exe: &Path) -> io::Result<()> {
    enable_in(REAL_KEYS, tracker_exe)
}

pub fn disable() -> io::Result<()> {
    disable_in(REAL_KEYS)
}

#[derive(Debug, PartialEq, Eq)]
pub enum Heal {
    Skipped,
    Ok,
    Rewritten,
    DisabledByUser,
}

fn self_heal_in(keys: Keys, cfg: &Config, exe: &Path) -> io::Result<Heal> {
    if !cfg.autostart {
        return Ok(Heal::Skipped);
    }
    let st = status_in(keys, exe, cfg.autostart);
    if st.disabled_by_user {
        return Ok(Heal::DisabledByUser);
    }
    if st.matches {
        return Ok(Heal::Ok);
    }
    enable_in(keys, exe)?;
    Ok(Heal::Rewritten)
}

/// Called from the tracker after config load. Logs the outcome.
pub fn self_heal(cfg: &Config) {
    let exe = match std::env::current_exe() {
        Ok(e) => e,
        Err(e) => {
            log::log_line(&format!("autostart: cannot resolve own path: {e}"));
            return;
        }
    };
    match self_heal_in(REAL_KEYS, cfg, &exe) {
        Ok(Heal::Skipped) => log::log_line("autostart: disabled in config; not registering"),
        Ok(Heal::Ok) => log::log_line("autostart: Run entry ok"),
        Ok(Heal::Rewritten) => log::log_line("autostart: Run entry (re)written"),
        Ok(Heal::DisabledByUser) => {
            log::log_line("autostart: disabled in Task Manager; leaving as is")
        }
        Err(e) => log::log_line(&format!("autostart: cannot write Run entry: {e}")),
    }
}

// ------------------------------------------------------------------ CLI

const USAGE: &str = "Usage: gametime autostart <on|off|status>";

fn sibling_tracker() -> io::Result<PathBuf> {
    let me = std::env::current_exe()?;
    let p = me
        .parent()
        .map(|d| d.join(TRACKER_EXE))
        .ok_or_else(|| io::Error::other("cannot resolve executable directory"))?;
    if p.is_file() {
        Ok(p)
    } else {
        Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("{} not found next to gametime.exe", p.display()),
        ))
    }
}

fn spawn_detached(tracker: &Path) -> io::Result<()> {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    const DETACHED_PROCESS: u32 = 0x0000_0008;
    std::process::Command::new(tracker)
        .creation_flags(CREATE_NO_WINDOW | DETACHED_PROCESS)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map(|_| ())
}

fn warn_if_disabled(st: &Status) {
    if st.disabled_by_user {
        println!(
            "warning: Task Manager has GameTime disabled (Startup apps). \
             Re-enable it there (Task Manager > Startup apps > GameTime > Enable); \
             GameTime will not override this."
        );
    }
}

fn print_status(st: &Status, tracker: &Path) {
    println!(
        "registered:     {}",
        st.registered.as_deref().unwrap_or("(none)")
    );
    println!("expected:       {}", run_value(tracker));
    println!("matches:        {}", st.matches);
    println!(
        "task manager:   {}",
        if st.disabled_by_user {
            "disabled by user"
        } else {
            "enabled"
        }
    );
    println!("config enabled: {}", st.config_autostart);
}

/// Handle `gametime autostart ...`; `args` includes the `autostart` word.
/// Returns the process exit code.
pub fn cli(args: &[String]) -> u8 {
    let sub = match args {
        [_, sub] if matches!(sub.as_str(), "on" | "off" | "status") => sub.as_str(),
        _ => {
            eprintln!("{USAGE}");
            return 2;
        }
    };
    match run_cli(sub) {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("error: {e}");
            1
        }
    }
}

fn run_cli(sub: &str) -> io::Result<()> {
    let dir = config::data_dir()?;
    let mut cfg = Config::load_or_create(&dir)?;
    match sub {
        "on" => {
            let tracker = sibling_tracker()?;
            cfg.autostart = true;
            cfg.save(&dir)?;
            enable(&tracker)?;
            println!("autostart enabled: {}", run_value(&tracker));
            warn_if_disabled(&status(&tracker, true));
            spawn_detached(&tracker)?;
            println!("tracker started (an already running tracker exits by itself)");
        }
        "off" => {
            cfg.autostart = false;
            cfg.save(&dir)?;
            disable()?;
            println!("autostart disabled; Run entry removed");
            println!("the tracker keeps running; use `gametime stop` to stop it");
        }
        _ => {
            let tracker = sibling_tracker().unwrap_or_else(|_| PathBuf::from(TRACKER_EXE));
            print_status(&status(&tracker, cfg.autostart), &tracker);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows_sys::Win32::System::Registry::{
        RegDeleteTreeW, RegSetKeyValueW, HKEY_CURRENT_USER, REG_BINARY,
    };

    #[test]
    fn parse_approved() {
        assert!(!parse_startup_approved(None));
        assert!(!parse_startup_approved(Some(&[])));
        assert!(!parse_startup_approved(Some(&[0x02, 0, 0, 0])));
        assert!(!parse_startup_approved(Some(&[0x06, 0, 0, 0])));
        assert!(parse_startup_approved(Some(&[0x03, 0, 0, 0])));
        assert!(parse_startup_approved(Some(&[0x07, 0, 0, 0])));
    }

    struct TestKeys {
        root: String,
        run: String,
        approved: String,
    }

    impl TestKeys {
        fn new(tag: &str) -> TestKeys {
            let root = format!("Software\\GameTimeTest-{}-{tag}", std::process::id());
            TestKeys {
                run: format!("{root}\\Run"),
                approved: format!("{root}\\Approved"),
                root,
            }
        }
        fn keys(&self) -> Keys<'_> {
            Keys {
                run: &self.run,
                approved: &self.approved,
            }
        }
        fn set_approved(&self, data: &[u8]) {
            let sub: Vec<u16> = self.approved.encode_utf16().chain([0]).collect();
            let val: Vec<u16> = VALUE_NAME.encode_utf16().chain([0]).collect();
            // SAFETY: NUL-terminated strings and a valid data buffer.
            let rc = unsafe {
                RegSetKeyValueW(
                    HKEY_CURRENT_USER,
                    sub.as_ptr(),
                    val.as_ptr(),
                    REG_BINARY,
                    data.as_ptr().cast(),
                    data.len() as u32,
                )
            };
            assert_eq!(rc, 0);
        }
    }

    impl Drop for TestKeys {
        fn drop(&mut self) {
            let root: Vec<u16> = self.root.encode_utf16().chain([0]).collect();
            // SAFETY: NUL-terminated key path under HKCU.
            unsafe { RegDeleteTreeW(HKEY_CURRENT_USER, root.as_ptr()) };
        }
    }

    const EXE: &str = r"C:\Games\GameTime\gametime-tracker.exe";

    #[test]
    fn enable_status_disable_round_trip() {
        let t = TestKeys::new("rt");
        let exe = Path::new(EXE);
        let st = status_in(t.keys(), exe, true);
        assert_eq!(st.registered, None);
        assert!(!st.matches && !st.disabled_by_user);

        enable_in(t.keys(), exe).unwrap();
        let st = status_in(t.keys(), exe, true);
        assert_eq!(
            st.registered.as_deref(),
            Some(format!("\"{EXE}\"").as_str())
        );
        assert!(st.matches);

        assert!(!status_in(t.keys(), Path::new(r"D:\other.exe"), true).matches);

        disable_in(t.keys()).unwrap();
        assert_eq!(status_in(t.keys(), exe, true).registered, None);
        disable_in(t.keys()).unwrap(); // missing is fine
    }

    #[test]
    fn self_heal_rewrites_wrong_path() {
        let t = TestKeys::new("heal");
        let cfg = Config::default();
        enable_in(t.keys(), Path::new(r"D:\old\gametime-tracker.exe")).unwrap();
        let exe = Path::new(EXE);
        assert_eq!(self_heal_in(t.keys(), &cfg, exe).unwrap(), Heal::Rewritten);
        assert!(status_in(t.keys(), exe, true).matches);
        assert_eq!(self_heal_in(t.keys(), &cfg, exe).unwrap(), Heal::Ok);
    }

    #[test]
    fn self_heal_creates_missing() {
        let t = TestKeys::new("create");
        let exe = Path::new(EXE);
        let cfg = Config::default();
        assert_eq!(self_heal_in(t.keys(), &cfg, exe).unwrap(), Heal::Rewritten);
        assert!(status_in(t.keys(), exe, true).matches);
    }

    #[test]
    fn self_heal_leaves_disabled_alone() {
        let t = TestKeys::new("dis");
        let cfg = Config::default();
        t.set_approved(&[0x03, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
        let exe = Path::new(EXE);
        assert_eq!(
            self_heal_in(t.keys(), &cfg, exe).unwrap(),
            Heal::DisabledByUser
        );
        assert_eq!(status_in(t.keys(), exe, true).registered, None);
    }

    #[test]
    fn self_heal_noop_when_config_off() {
        let t = TestKeys::new("off");
        let cfg = Config {
            autostart: false,
            ..Config::default()
        };
        let exe = Path::new(EXE);
        assert_eq!(self_heal_in(t.keys(), &cfg, exe).unwrap(), Heal::Skipped);
        assert_eq!(status_in(t.keys(), exe, false).registered, None);
    }
}
