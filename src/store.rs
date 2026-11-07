//! Append-only session log (`sessions.jsonl`) plus a checkpoint (`current.json`).
//!
//! Crash safety: a closed session is appended to `sessions.jsonl` first and the
//! checkpoint is cleared afterwards. A crash in between leaves both on disk;
//! [`Store::recover`] and [`Store::load_all`] de-duplicate on `(game, start_utc)`.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

const SESSIONS_FILE: &str = "sessions.jsonl";
const CHECKPOINT_FILE: &str = "current.json";

/// One closed (or checkpointed) session.
///
/// Any field added in the future MUST use `#[serde(default)]` so that existing
/// log lines and checkpoints keep parsing.
#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
pub struct SessionRecord {
    pub game: String,
    pub source: String,
    pub exe: String,
    pub start_utc: u64,
    pub end_utc: u64,
    pub active_s: u64,
}

/// Write `bytes` to `<path>.tmp`, fsync, then rename over `path`.
/// The temp file is removed on error.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    // Unique per process so concurrent writers never share a temp file.
    let mut name = path.file_name().unwrap_or_default().to_owned();
    name.push(format!(".{}.tmp", std::process::id()));
    let tmp = path.with_file_name(name);
    let res = (|| {
        let mut f = File::create(&tmp)?;
        f.write_all(bytes)?;
        f.flush()?;
        f.sync_all()?;
        drop(f);
        fs::rename(&tmp, path)
    })();
    if res.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    res
}

/// Failure of [`Store::close_session`], telling the two steps apart.
#[derive(Debug)]
pub enum CloseError {
    /// The append failed: nothing was written, retrying is safe.
    Append(io::Error),
    /// The record IS in the log but the checkpoint could not be removed. Do NOT
    /// retry `append`; retry `clear_checkpoint` (or let `recover` handle it).
    Clear(io::Error),
}

impl std::fmt::Display for CloseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CloseError::Append(e) => write!(f, "append failed: {e}"),
            CloseError::Clear(e) => write!(f, "clear checkpoint failed: {e}"),
        }
    }
}

impl std::error::Error for CloseError {}

pub struct Store {
    dir: PathBuf,
}

impl Store {
    pub fn open(dir: &Path) -> io::Result<Store> {
        fs::create_dir_all(dir)?;
        Ok(Store {
            dir: dir.to_path_buf(),
        })
    }

    fn sessions_path(&self) -> PathBuf {
        self.dir.join(SESSIONS_FILE)
    }

    fn checkpoint_path(&self) -> PathBuf {
        self.dir.join(CHECKPOINT_FILE)
    }

    /// Append one record as a single JSON line. If the file does not end in a
    /// newline (a write truncated by a crash), a newline is written first so the
    /// new record starts on its own line.
    pub fn append(&self, rec: &SessionRecord) -> io::Result<()> {
        let mut line = serde_json::to_vec(rec).map_err(io::Error::other)?;
        line.push(b'\n');
        let mut f = OpenOptions::new()
            .read(true)
            .append(true)
            .create(true)
            .open(self.sessions_path())?;
        let len = f.metadata()?.len();
        if len > 0 {
            f.seek(SeekFrom::Start(len - 1))?;
            let mut last = [0u8; 1];
            f.read_exact(&mut last)?;
            if last[0] != b'\n' {
                line.insert(0, b'\n');
            }
        }
        // Append mode: writes always go to the end regardless of the seek.
        f.write_all(&line)?;
        f.sync_data()
    }

    pub fn write_checkpoint(&self, rec: &SessionRecord) -> io::Result<()> {
        let bytes = serde_json::to_vec(rec).map_err(io::Error::other)?;
        write_atomic(&self.checkpoint_path(), &bytes)
    }

    pub fn clear_checkpoint(&self) -> io::Result<()> {
        match fs::remove_file(self.checkpoint_path()) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
            _ => Ok(()),
        }
    }

    /// Append the closed session, THEN clear the checkpoint. If the order were
    /// reversed, a crash in between would lose the session; this way the worst
    /// case is a duplicate, which recovery and loading de-duplicate.
    ///
    /// `append` and `clear_checkpoint` are also public for callers that want to
    /// handle the steps separately. Never retry `append` after
    /// [`CloseError::Clear`].
    pub fn close_session(&self, rec: &SessionRecord) -> Result<(), CloseError> {
        self.append(rec).map_err(CloseError::Append)?;
        self.clear_checkpoint().map_err(CloseError::Clear)
    }

    pub fn load_checkpoint(&self) -> Option<SessionRecord> {
        let bytes = fs::read(self.checkpoint_path()).ok()?;
        serde_json::from_slice(&bytes).ok()
    }

    /// Startup recovery of leftover checkpoints. Returns the records that were
    /// appended to the log, in processing order (pending files first).
    ///
    /// - A record already in the log with the same `(game, start_utc)` and an
    ///   `end_utc` at least as large is not appended again (crash between
    ///   append and clear); one with a larger `end_utc` is appended and
    ///   `load_all` keeps the longer one.
    /// - An unparseable checkpoint is renamed to `*.bad` and ignored.
    /// - If `current.json` cannot be read or appended, it is renamed aside to
    ///   `current.<utc_now>.pending.json` so a new checkpoint can never destroy
    ///   it; every `recover` call retries all `current.*.pending.json` files
    ///   and deletes each after success.
    ///
    /// Any failure is returned as `Err` after all work was attempted. If even
    /// the rename-aside fails, `current.json` is still in place: the caller must
    /// NOT call `write_checkpoint` until `recover` succeeds.
    pub fn recover(&self) -> io::Result<Vec<SessionRecord>> {
        let mut recovered = Vec::new();
        let mut first_err: Option<io::Error> = None;
        let mut note = |e: io::Error| {
            first_err.get_or_insert(e);
        };

        let mut pending = Vec::new();
        for entry in fs::read_dir(&self.dir)?.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name.starts_with("current.") && name.ends_with(".pending.json") {
                pending.push(entry.path());
            }
        }
        pending.sort();

        let mut log = self.load_all_with_stats()?.0;
        for path in pending {
            match self.recover_file(&path, &mut log) {
                Ok(Some(r)) => recovered.push(r),
                Ok(None) => {}
                Err(e) => note(e),
            }
        }

        let path = self.checkpoint_path();
        match self.recover_file(&path, &mut log) {
            Ok(Some(r)) => recovered.push(r),
            Ok(None) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => {
                // Keep the crashed session safe from the next write_checkpoint.
                if let Err(re) = self.set_aside(&path) {
                    note(re);
                }
                note(e);
            }
        }
        match first_err {
            Some(e) => Err(e),
            None => Ok(recovered),
        }
    }

    /// Process one checkpoint-like file. Deletes it on success; a parse failure
    /// renames it to `<name>.bad` and returns `Ok(None)`.
    fn recover_file(
        &self,
        path: &Path,
        log: &mut Vec<SessionRecord>,
    ) -> io::Result<Option<SessionRecord>> {
        let bytes = fs::read(path)?;
        let Ok(rec) = serde_json::from_slice::<SessionRecord>(&bytes) else {
            let mut bad = path.as_os_str().to_owned();
            bad.push(".bad");
            let bad = PathBuf::from(bad);
            let _ = fs::remove_file(&bad);
            fs::rename(path, &bad)?;
            return Ok(None);
        };
        let have = log
            .iter()
            .filter(|r| r.game == rec.game && r.start_utc == rec.start_utc)
            .map(|r| r.end_utc)
            .max();
        let append = have.is_none_or(|e| rec.end_utc > e);
        if append {
            self.append(&rec)?;
            log.push(rec.clone());
        }
        fs::remove_file(path)?;
        Ok(append.then_some(rec))
    }

    fn set_aside(&self, path: &Path) -> io::Result<()> {
        let mut now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        loop {
            let to = self.dir.join(format!("current.{now}.pending.json"));
            if !to.exists() {
                return fs::rename(path, to);
            }
            now += 1;
        }
    }

    pub fn load_all(&self) -> io::Result<Vec<SessionRecord>> {
        Ok(self.load_all_with_stats()?.0)
    }

    /// Load all records, skipping blank or unparseable lines (returned as the
    /// count). Duplicates on `(game, start_utc)` keep the largest `end_utc`.
    /// The result is sorted by `start_utc`.
    pub fn load_all_with_stats(&self) -> io::Result<(Vec<SessionRecord>, usize)> {
        let data = match fs::read(self.sessions_path()) {
            Ok(d) => d,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok((Vec::new(), 0)),
            Err(e) => return Err(e),
        };
        let mut skipped = 0;
        let mut out: Vec<SessionRecord> = Vec::new();
        let mut index: HashMap<(String, u64), usize> = HashMap::new();
        for line in data.split(|&b| b == b'\n') {
            if line.iter().all(u8::is_ascii_whitespace) {
                continue;
            }
            let Ok(rec) = serde_json::from_slice::<SessionRecord>(line) else {
                skipped += 1;
                continue;
            };
            match index.get(&(rec.game.clone(), rec.start_utc)) {
                Some(&i) => {
                    if rec.end_utc > out[i].end_utc {
                        out[i] = rec;
                    }
                }
                None => {
                    index.insert((rec.game.clone(), rec.start_utc), out.len());
                    out.push(rec);
                }
            }
        }
        out.sort_by_key(|r| r.start_utc);
        Ok((out, skipped))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    struct TempDir(PathBuf);
    impl TempDir {
        fn new() -> TempDir {
            static N: AtomicU32 = AtomicU32::new(0);
            let p = std::env::temp_dir().join(format!(
                "gametime-store-test-{}-{}",
                std::process::id(),
                N.fetch_add(1, Ordering::Relaxed)
            ));
            let _ = fs::remove_dir_all(&p);
            TempDir(p)
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn rec(game: &str, start: u64, end: u64) -> SessionRecord {
        SessionRecord {
            game: game.into(),
            source: "steam".into(),
            exe: format!("C:\\Games\\{game}.exe"),
            start_utc: start,
            end_utc: end,
            active_s: end - start,
        }
    }

    fn tmp_files(dir: &Path) -> bool {
        fs::read_dir(dir)
            .unwrap()
            .flatten()
            .any(|e| e.file_name().to_string_lossy().ends_with(".tmp"))
    }

    fn pending_files(dir: &Path) -> Vec<PathBuf> {
        fs::read_dir(dir)
            .unwrap()
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.to_string_lossy().ends_with(".pending.json"))
            .collect()
    }

    fn lines(store: &Store) -> usize {
        fs::read_to_string(store.sessions_path())
            .unwrap()
            .lines()
            .count()
    }

    #[test]
    fn append_load_round_trip() {
        let d = TempDir::new();
        let s = Store::open(&d.0).unwrap();
        assert!(s.load_all().unwrap().is_empty());
        let a = rec("A", 100, 200);
        let b = rec("B", 50, 80);
        s.append(&a).unwrap();
        s.append(&b).unwrap();
        assert_eq!(s.load_all().unwrap(), vec![b, a]);
    }

    #[test]
    fn truncated_last_line_skipped_and_next_append_fresh() {
        let d = TempDir::new();
        let s = Store::open(&d.0).unwrap();
        s.append(&rec("A", 1, 10)).unwrap();
        let mut f = OpenOptions::new()
            .append(true)
            .open(s.sessions_path())
            .unwrap();
        f.write_all(b"{\"game\":\"B\",\"sou").unwrap();
        drop(f);
        let (v, skipped) = s.load_all_with_stats().unwrap();
        assert_eq!((v.len(), skipped), (1, 1));
        s.append(&rec("C", 20, 30)).unwrap();
        let (v, skipped) = s.load_all_with_stats().unwrap();
        assert_eq!(v.len(), 2);
        assert_eq!(skipped, 1);
        assert_eq!(v[1].game, "C");
    }

    #[test]
    fn garbage_lines_skipped_and_counted() {
        let d = TempDir::new();
        let s = Store::open(&d.0).unwrap();
        let good = serde_json::to_string(&rec("A", 1, 5)).unwrap();
        let mut data = format!("garbage\n\n{good}\n   \n{{\"x\":1}}\n").into_bytes();
        data.extend_from_slice(b"\xff\xfe\n");
        fs::write(s.sessions_path(), data).unwrap();
        let (v, skipped) = s.load_all_with_stats().unwrap();
        assert_eq!(v.len(), 1);
        assert_eq!(skipped, 3);
    }

    #[test]
    fn dedupe_keeps_max_end_and_sorts() {
        let d = TempDir::new();
        let s = Store::open(&d.0).unwrap();
        s.append(&rec("A", 100, 150)).unwrap();
        s.append(&rec("A", 100, 300)).unwrap();
        s.append(&rec("A", 100, 200)).unwrap();
        s.append(&rec("B", 100, 120)).unwrap();
        s.append(&rec("A", 10, 20)).unwrap();
        let v = s.load_all().unwrap();
        assert_eq!(v.len(), 3);
        assert_eq!(v[0], rec("A", 10, 20));
        let a = v
            .iter()
            .find(|r| r.game == "A" && r.start_utc == 100)
            .unwrap();
        assert_eq!(a.end_utc, 300);
    }

    #[test]
    fn recover_appends_checkpoint() {
        let d = TempDir::new();
        let s = Store::open(&d.0).unwrap();
        let r = rec("A", 100, 160);
        s.write_checkpoint(&r).unwrap();
        assert_eq!(s.load_checkpoint(), Some(r.clone()));
        assert_eq!(s.recover().unwrap(), vec![r.clone()]);
        assert_eq!(s.load_all().unwrap(), vec![r]);
        assert_eq!(s.load_checkpoint(), None);
        assert!(s.recover().unwrap().is_empty());
    }

    #[test]
    fn recover_skips_duplicate() {
        let d = TempDir::new();
        let s = Store::open(&d.0).unwrap();
        let r = rec("A", 100, 160);
        s.append(&rec("A", 100, 170)).unwrap();
        s.write_checkpoint(&r).unwrap();
        assert!(s.recover().unwrap().is_empty());
        assert_eq!(lines(&s), 1);
        assert!(!s.checkpoint_path().exists());
    }

    #[test]
    fn corrupt_checkpoint_renamed() {
        let d = TempDir::new();
        let s = Store::open(&d.0).unwrap();
        fs::write(s.checkpoint_path(), b"{not json").unwrap();
        assert_eq!(s.load_checkpoint(), None);
        assert!(s.recover().unwrap().is_empty());
        assert!(!s.checkpoint_path().exists());
        assert_eq!(
            fs::read(d.0.join("current.json.bad")).unwrap(),
            b"{not json"
        );
        assert!(s.load_all().unwrap().is_empty());
    }

    #[test]
    fn close_session_order_survives_crash() {
        let d = TempDir::new();
        let s = Store::open(&d.0).unwrap();
        let r = rec("A", 100, 160);
        s.write_checkpoint(&r).unwrap();
        s.close_session(&r).unwrap();
        assert_eq!(s.load_checkpoint(), None);
        // Crash after append, before clear: checkpoint still present.
        s.write_checkpoint(&r).unwrap();
        assert!(s.recover().unwrap().is_empty());
        assert_eq!(s.load_all().unwrap(), vec![r]);
        assert_eq!(lines(&s), 1);
    }

    #[test]
    fn clear_checkpoint_missing_is_ok() {
        let d = TempDir::new();
        let s = Store::open(&d.0).unwrap();
        s.clear_checkpoint().unwrap();
    }

    #[test]
    fn write_atomic_replaces_and_leaves_no_tmp() {
        let d = TempDir::new();
        fs::create_dir_all(&d.0).unwrap();
        let p = d.0.join("f.json");
        write_atomic(&p, b"one").unwrap();
        write_atomic(&p, b"two").unwrap();
        assert_eq!(fs::read(&p).unwrap(), b"two");
        assert!(!tmp_files(&d.0));
        // Failure (target is a directory) removes the temp file.
        let dir_target = d.0.join("sub");
        fs::create_dir_all(&dir_target).unwrap();
        assert!(write_atomic(&dir_target, b"x").is_err());
        assert!(!tmp_files(&d.0));
    }

    #[test]
    fn non_ascii_game_names() {
        let d = TempDir::new();
        let s = Store::open(&d.0).unwrap();
        let r = rec("Ведьмак 3: Дикая Охота ゲーム 🎮", 5, 50);
        s.append(&r).unwrap();
        s.write_checkpoint(&rec("Ünïcödé", 60, 70)).unwrap();
        assert_eq!(s.load_all().unwrap(), vec![r]);
        assert_eq!(s.load_checkpoint().unwrap().game, "Ünïcödé");
    }

    #[test]
    fn valid_last_line_without_newline_then_append() {
        let d = TempDir::new();
        let s = Store::open(&d.0).unwrap();
        let a = rec("A", 1, 10);
        fs::write(s.sessions_path(), serde_json::to_vec(&a).unwrap()).unwrap();
        let b = rec("B", 20, 30);
        s.append(&b).unwrap();
        assert_eq!(lines(&s), 2);
        assert_eq!(s.load_all().unwrap(), vec![a, b]);
    }

    #[test]
    fn unreadable_checkpoint_kept_as_pending_and_recovered_later() {
        let d = TempDir::new();
        let s = Store::open(&d.0).unwrap();
        // A directory named current.json cannot be read as a file.
        fs::create_dir(s.checkpoint_path()).unwrap();
        assert!(s.recover().is_err());
        assert!(!s.checkpoint_path().exists());
        let pend = pending_files(&d.0);
        assert_eq!(pend.len(), 1);
        // The unreadable pending entry is still not lost; make it readable.
        let r = rec("A", 100, 160);
        fs::remove_dir(&pend[0]).unwrap();
        fs::write(&pend[0], serde_json::to_vec(&r).unwrap()).unwrap();
        assert_eq!(s.recover().unwrap(), vec![r.clone()]);
        assert!(pending_files(&d.0).is_empty());
        assert_eq!(s.load_all().unwrap(), vec![r]);
    }

    #[test]
    fn pending_recovery_dedupes() {
        let d = TempDir::new();
        let s = Store::open(&d.0).unwrap();
        let r = rec("A", 100, 160);
        s.append(&r).unwrap();
        fs::write(
            d.0.join("current.5.pending.json"),
            serde_json::to_vec(&r).unwrap(),
        )
        .unwrap();
        fs::write(
            d.0.join("current.6.pending.json"),
            serde_json::to_vec(&rec("B", 200, 260)).unwrap(),
        )
        .unwrap();
        let got = s.recover().unwrap();
        assert_eq!(got, vec![rec("B", 200, 260)]);
        assert!(pending_files(&d.0).is_empty());
        assert_eq!(lines(&s), 2);
    }

    #[test]
    fn longer_checkpoint_than_log_is_appended() {
        let d = TempDir::new();
        let s = Store::open(&d.0).unwrap();
        s.append(&rec("A", 100, 150)).unwrap();
        let longer = rec("A", 100, 190);
        s.write_checkpoint(&longer).unwrap();
        assert_eq!(s.recover().unwrap(), vec![longer.clone()]);
        assert_eq!(s.load_all().unwrap(), vec![longer]);
    }

    #[test]
    fn close_session_ok() {
        let d = TempDir::new();
        let s = Store::open(&d.0).unwrap();
        let r = rec("A", 1, 2);
        s.write_checkpoint(&r).unwrap();
        s.close_session(&r).unwrap();
        assert!(s.load_checkpoint().is_none());
    }
}
