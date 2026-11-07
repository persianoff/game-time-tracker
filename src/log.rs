//! Minimal file logger with single-file rotation.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

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

/// Append a timestamped line (`[<unix seconds>] msg`). No-op before `init`.
pub fn log_line(msg: &str) {
    let mut guard = LOGGER.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(l) = guard.as_mut() {
        l.write(msg);
    }
}

impl Logger {
    fn open(&mut self) {
        self.file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .ok();
    }

    fn write(&mut self, msg: &str) {
        let secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        if self.size >= MAX_SIZE {
            self.file = None;
            let _ = std::fs::remove_file(&self.old_path);
            let _ = std::fs::rename(&self.path, &self.old_path);
            self.size = 0;
            self.open();
        }
        if self.file.is_none() {
            self.open();
        }
        if let Some(f) = self.file.as_mut() {
            let line = format!("[{secs}] {msg}\n");
            if f.write_all(line.as_bytes()).is_ok() {
                self.size += line.len() as u64;
            }
        }
        if self.verbose {
            eprintln!("[{secs}] {msg}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rotates_when_over_limit() {
        let dir = std::env::temp_dir().join(format!("gametime-test-{}-log", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // Pre-fill log.txt beyond the limit.
        std::fs::write(dir.join("log.txt"), vec![b'x'; MAX_SIZE as usize + 1]).unwrap();

        init(&dir, false);
        log_line("hello");

        let old = std::fs::metadata(dir.join("log.old.txt")).unwrap();
        assert_eq!(old.len(), MAX_SIZE + 1);
        let cur = std::fs::read_to_string(dir.join("log.txt")).unwrap();
        assert!(cur.ends_with("] hello\n"));
        assert!(cur.len() < 100);
        *LOGGER.lock().unwrap() = None;
        let _ = std::fs::remove_dir_all(&dir);
    }
}
