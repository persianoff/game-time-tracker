//! Minimal file logger with single-file rotation.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

const MAX_SIZE: u64 = 1024 * 1024;

struct Logger {
    path: PathBuf,
    old_path: PathBuf,
    verbose: bool,
    file: Option<File>,
    size: u64,
}

static LOGGER: Mutex<Option<Logger>> = Mutex::new(None);

/// Initialise logging into `dir/log.txt`.
pub fn init(dir: &Path, verbose: bool) {
    let path = dir.join("log.txt");
    let old_path = dir.join("log.old.txt");
    let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
    let mut l = Logger {
        path,
        old_path,
        verbose,
        file: None,
        size,
    };
    l.open();
    *LOGGER.lock().unwrap_or_else(|e| e.into_inner()) = Some(l);
}

/// Append a timestamped line (`YYYY-MM-DD HH:MM:SS msg`, UTC). Newlines in the
/// message become spaces. No-op before `init`.
pub fn log_line(msg: &str) {
    let mut guard = LOGGER.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(l) = guard.as_mut() {
        l.write(crate::win32::now_utc_secs(), msg);
    }
}

/// Like [`log_line`] but never blocks: the line is dropped if the logger is
/// busy (used from the panic hook, which may run while the lock is held).
pub fn try_log_line(msg: &str) {
    let mut guard = match LOGGER.try_lock() {
        Ok(g) => g,
        Err(std::sync::TryLockError::Poisoned(e)) => e.into_inner(),
        Err(std::sync::TryLockError::WouldBlock) => return,
    };
    if let Some(l) = guard.as_mut() {
        l.write(crate::win32::now_utc_secs(), msg);
    }
}

fn sanitize(msg: &str) -> String {
    msg.replace(['\n', '\r'], " ")
}

fn format_line(secs: u64, msg: &str) -> String {
    format!("{} {}\n", crate::win32::format_utc(secs), sanitize(msg))
}

impl Logger {
    fn open(&mut self) {
        self.file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .ok();
    }

    fn rotate(&mut self) {
        // Another process (e.g. `run --verbose`) may have rotated already.
        let on_disk = std::fs::metadata(&self.path).map(|m| m.len()).unwrap_or(0);
        if on_disk < MAX_SIZE {
            self.size = on_disk;
            return;
        }
        self.file = None;
        let _ = std::fs::remove_file(&self.old_path);
        if std::fs::rename(&self.path, &self.old_path).is_ok() {
            self.size = 0;
        }
        self.open();
    }

    fn write(&mut self, secs: u64, msg: &str) {
        if self.size >= MAX_SIZE {
            self.rotate();
        }
        if self.file.is_none() {
            self.open();
        }
        let line = format_line(secs, msg);
        if let Some(f) = self.file.as_mut() {
            if f.write_all(line.as_bytes()).is_ok() {
                self.size += line.len() as u64;
            }
        }
        if self.verbose {
            eprint!("{line}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("gametime-test-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn new_logger(dir: &Path, size: u64) -> Logger {
        let mut l = Logger {
            path: dir.join("log.txt"),
            old_path: dir.join("log.old.txt"),
            verbose: false,
            file: None,
            size,
        };
        l.open();
        l
    }

    #[test]
    fn rotates_when_over_limit() {
        let dir = tmp("log-rot");
        std::fs::write(dir.join("log.txt"), vec![b'x'; MAX_SIZE as usize + 1]).unwrap();
        let mut l = new_logger(&dir, MAX_SIZE + 1);
        l.write(0, "hello");
        let old = std::fs::metadata(dir.join("log.old.txt")).unwrap();
        assert_eq!(old.len(), MAX_SIZE + 1);
        let cur = std::fs::read_to_string(dir.join("log.txt")).unwrap();
        assert_eq!(cur, "1970-01-01 00:00:00 hello\n");
        assert_eq!(l.size, cur.len() as u64);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn stale_size_is_rechecked_against_disk() {
        let dir = tmp("log-stale");
        std::fs::write(dir.join("log.txt"), b"small\n").unwrap();
        // In-memory size is stale (someone else rotated the file).
        let mut l = new_logger(&dir, MAX_SIZE + 5);
        l.write(0, "x");
        assert!(!dir.join("log.old.txt").exists());
        assert_eq!(l.size, 6 + "1970-01-01 00:00:00 x\n".len() as u64);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn failed_rename_keeps_size() {
        let dir = tmp("log-norename");
        std::fs::write(dir.join("log.txt"), vec![b'x'; MAX_SIZE as usize]).unwrap();
        // A directory in the way makes the rename (and remove_file) fail.
        std::fs::create_dir(dir.join("log.old.txt")).unwrap();
        std::fs::write(dir.join("log.old.txt").join("f"), b"1").unwrap();
        let mut l = new_logger(&dir, MAX_SIZE);
        l.rotate();
        assert_eq!(l.size, MAX_SIZE);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn newlines_are_replaced() {
        assert_eq!(sanitize("a\nb\r\nc"), "a b  c");
        let line = format_line(20764 * 86400 + 21 * 3600 + 31 * 60 + 5, "x\ny");
        assert_eq!(line, "2026-11-07 21:31:05 x y\n");
    }
}
