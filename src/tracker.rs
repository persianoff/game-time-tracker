//! The tracker main loop: polls the foreground window once per second, feeds
//! the session state machine and persists sessions through the [`Store`].
//!
//! [`Engine`] is free of Win32 calls (everything environmental is injected), so
//! it is unit-tested with a fake clock and a scripted foreground sequence.
//! Every store/IO error is logged and swallowed: the loop never exits on, or
//! panics because of, a failing disk.

use std::collections::{BTreeMap, HashSet};
use std::path::PathBuf;

use crate::config::{self, Config};
use crate::detect::{self, GameDb, Source};
use crate::log;
use crate::session::{Params, Session, Tracker};
use crate::store::{CloseError, SessionRecord, Store};
use crate::win32::{self, Foreground};

pub const MUTEX_NAME: &str = "Local\\GameTime";
pub const STOP_EVENT_NAME: &str = "Local\\GameTimeStop";

const CHECKPOINT_EVERY_S: u64 = 30;
const RESCAN_EVERY_S: u64 = 30 * 60;
/// Give up on a failed append after this many attempts (loudly).
const MAX_APPEND_ATTEMPTS: u8 = 6;
/// Seconds between append retries.
const RETRY_EVERY_S: u64 = 10;
/// Backoff for `recover()` while the checkpoint slot stays busy.
const RECOVER_WAIT_MIN_S: u64 = 30;
const RECOVER_WAIT_MAX_S: u64 = 600;
/// At most this many closed sessions wait for an append retry.
const MAX_RETRY_SLOTS: usize = 8;
/// Bound on the set of access-denied pids that were already logged.
const MAX_DENIED_PIDS: usize = 256;

/// Identity of a game that survives rescans (the `GameDb` is rebuilt).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GameKey {
    pub name: String,
    pub source: Source,
}

/// Last resolved foreground process, so `match_exe` runs only on change.
struct Cached {
    pid: u32,
    path: PathBuf,
    key: Option<GameKey>,
}

struct PendingAppend {
    rec: SessionRecord,
    attempts: u8,
    next_at: u64,
}

pub struct Engine {
    cfg: Config,
    db: GameDb,
    scan: fn(&Config) -> GameDb,
    tracker: Tracker<GameKey>,
    store: Store,
    cache: Option<Cached>,
    denied: HashSet<u32>,
    /// Exe path first seen for the open session.
    exe: Option<(GameKey, String)>,
    /// Closed sessions whose append failed.
    retry: Vec<PendingAppend>,
    /// It is safe to write `current.json` (no unrecovered file in the way).
    slot_ok: bool,
    /// The session whose prefix we last wrote to `current.json` (game, start).
    ckpt: Option<(GameKey, u64)>,
    /// Current wait between `recover()` calls while the slot stays busy.
    recover_wait: u64,
    next_recover: u64,
    /// The last recover() reported errors (e.g. pending files): try again.
    need_recover: bool,
    last_ckpt: Option<u64>,
    last_scan: Option<u64>,
    /// Time of the latest tick (used by `shutdown`).
    last_seen: u64,
    #[cfg(test)]
    match_calls: u32,
    #[cfg(test)]
    recover_calls: u32,
}

impl Engine {
    /// `checkpoint_free` comes from the startup [`crate::store::Recovery`].
    pub fn new(
        cfg: Config,
        db: GameDb,
        scan: fn(&Config) -> GameDb,
        store: Store,
        checkpoint_free: bool,
    ) -> Engine {
        let params = Params {
            merge_gap_s: cfg.merge_gap_s,
            ..Params::default()
        };
        Engine {
            cfg,
            db,
            scan,
            tracker: Tracker::new(params),
            store,
            cache: None,
            denied: HashSet::new(),
            exe: None,
            retry: Vec::new(),
            slot_ok: checkpoint_free,
            ckpt: None,
            recover_wait: RECOVER_WAIT_MIN_S,
            next_recover: 0,
            need_recover: false,
            last_ckpt: None,
            last_scan: None,
            last_seen: 0,
            #[cfg(test)]
            match_calls: 0,
            #[cfg(test)]
            recover_calls: 0,
        }
    }

    /// One poll. `now` is UTC epoch seconds.
    pub fn tick(&mut self, now: u64, fg: Foreground) {
        self.last_seen = now;
        if !self.retry.is_empty() {
            self.retry_appends(now, false);
        }

        let have_game = match fg {
            Foreground::None | Foreground::Unknown(_) => false,
            Foreground::Denied(pid) => {
                if !self.denied.contains(&pid) {
                    log::log_line(&format!("access denied opening foreground pid {pid}"));
                    if self.denied.len() >= MAX_DENIED_PIDS {
                        self.denied.clear();
                    }
                    self.denied.insert(pid);
                }
                false
            }
            Foreground::Exe { pid, path } => {
                let stale = !matches!(&self.cache, Some(c) if c.pid == pid && c.path == path);
                if stale {
                    #[cfg(test)]
                    {
                        self.match_calls += 1;
                    }
                    let key = self.db.match_exe(&path).map(|g| GameKey {
                        name: g.name.clone(),
                        source: g.source,
                    });
                    self.cache = Some(Cached { pid, path, key });
                }
                matches!(&self.cache, Some(Cached { key: Some(_), .. }))
            }
        };
        let key = if have_game {
            self.cache.as_ref().and_then(|c| c.key.as_ref())
        } else {
            None
        };

        let closed = self.tracker.step(now, key);
        let closed_exe = if closed.is_some() {
            self.exe.take()
        } else {
            None
        };
        if let (Some(k), Some(c)) = (key, self.cache.as_ref()) {
            if !matches!(&self.exe, Some((g, _)) if g == k) {
                log::log_line(&format!("tracking {} ({})", k.name, k.source));
                self.exe = Some((k.clone(), c.path.to_string_lossy().into_owned()));
            }
        }
        if let Some(sess) = closed {
            let exe = closed_exe.map(|(_, e)| e).unwrap_or_default();
            self.commit(sess, exe, now);
        }
        if self.tracker.is_idle() {
            self.exe = None;
        }

        self.maybe_checkpoint(now);
        // Last, so a slow scan never delays a poll's clock reading.
        self.maybe_rescan(now);
    }

    /// Close the open session (stop/shutdown) and make a last attempt at any
    /// pending appends.
    pub fn shutdown(&mut self) {
        if let Some(sess) = self.tracker.flush() {
            let exe = self.exe.take().map(|(_, e)| e).unwrap_or_default();
            self.commit(sess, exe, self.last_seen);
        }
        if !self.retry.is_empty() {
            self.retry_appends(self.last_seen, true);
        }
    }

    fn record(&self, s: &Session<GameKey>, exe: String) -> SessionRecord {
        SessionRecord {
            game: s.game.name.clone(),
            source: s.game.source.as_str().to_string(),
            exe,
            start_utc: s.start,
            end_utc: s.end,
            active_s: s.active_s,
        }
    }

    fn commit(&mut self, sess: Session<GameKey>, exe: String, now: u64) {
        let rec = self.record(&sess, exe);
        let owns = self.slot_ok
            && matches!(&self.ckpt, Some((g, st)) if *g == sess.game && *st == sess.start);
        if owns {
            match self.store.close_session(&rec) {
                Ok(()) => {
                    self.ckpt = None;
                    log::log_line(&format!(
                        "session closed: {} {}s active",
                        rec.game, rec.active_s
                    ));
                }
                Err(CloseError::Clear(e)) => {
                    // Recorded; never re-append. recover() removes the leftover.
                    log::log_line(&format!("session {} recorded, but {e}", rec.game));
                    self.ckpt = None;
                    self.slot_ok = false;
                }
                Err(CloseError::Append(e)) => {
                    log::log_line(&format!("append failed for {} ({e}); will retry", rec.game));
                    // current.json still holds a prefix of this session: leave
                    // it for recover() instead of overwriting it.
                    self.ckpt = None;
                    self.slot_ok = false;
                    self.queue_retry(rec, now);
                }
            }
        } else {
            // The checkpoint file (if any) is not this session's: never touch it.
            match self.store.append(&rec) {
                Ok(()) => log::log_line(&format!(
                    "session closed: {} {}s active",
                    rec.game, rec.active_s
                )),
                Err(e) => {
                    log::log_line(&format!("append failed for {} ({e}); will retry", rec.game));
                    self.queue_retry(rec, now);
                }
            }
        }
    }

    fn queue_retry(&mut self, rec: SessionRecord, now: u64) {
        if self.retry.len() >= MAX_RETRY_SLOTS {
            let lost = self.retry.remove(0);
            log::log_line(&format!(
                "retry queue full, dropping {} start={}: {}",
                lost.rec.game,
                lost.rec.start_utc,
                serde_json::to_string(&lost.rec).unwrap_or_default()
            ));
        }
        self.retry.push(PendingAppend {
            rec,
            attempts: 1,
            next_at: now + RETRY_EVERY_S,
        });
    }

    fn retry_appends(&mut self, now: u64, force: bool) {
        let mut pending = std::mem::take(&mut self.retry);
        for mut p in pending.drain(..) {
            if p.next_at > now + RETRY_EVERY_S {
                p.next_at = now + RETRY_EVERY_S; // clock went backwards
            }
            if !force && now < p.next_at {
                self.retry.push(p);
                continue;
            }
            match self.store.append(&p.rec) {
                Ok(()) => log::log_line(&format!(
                    "append retry succeeded for {} start={}",
                    p.rec.game, p.rec.start_utc
                )),
                Err(e) => {
                    p.attempts += 1;
                    p.next_at = now + RETRY_EVERY_S;
                    if p.attempts > MAX_APPEND_ATTEMPTS {
                        log::log_line(&format!(
                            "giving up appending after {} attempts ({e}): {}",
                            MAX_APPEND_ATTEMPTS,
                            serde_json::to_string(&p.rec).unwrap_or_default()
                        ));
                    } else {
                        self.retry.push(p);
                    }
                }
            }
        }
    }

    fn maybe_rescan(&mut self, now: u64) {
        match self.last_scan {
            None => self.last_scan = Some(now),
            Some(t) if now < t => self.last_scan = Some(now),
            // Defer while a session is open or paused: a slow scan (offline
            // drive) must not look like a sleep gap and split the session.
            Some(t) if now - t >= RESCAN_EVERY_S && self.tracker.is_idle() => {
                self.last_scan = Some(now);
                self.db = (self.scan)(&self.cfg);
                self.cache = None;
                log::log_line(&format!("rescan: {} games", self.db.games().len()));
                win32::trim_working_set();
            }
            _ => {}
        }
    }

    /// Run `recover()`; back off while it keeps failing (busy slot or pending
    /// files that still cannot be appended).
    fn run_recover(&mut self, now: u64) {
        #[cfg(test)]
        {
            self.recover_calls += 1;
        }
        let rv = self.store.recover();
        for r in &rv.recovered {
            log::log_line(&format!(
                "recovered session {} start={}",
                r.game, r.start_utc
            ));
        }
        for e in &rv.errors {
            log::log_line(&format!("recover error: {e}"));
        }
        self.slot_ok = rv.checkpoint_free;
        self.need_recover = !rv.errors.is_empty();
        if self.slot_ok && !self.need_recover {
            self.recover_wait = RECOVER_WAIT_MIN_S;
        } else {
            self.next_recover = now + self.recover_wait;
            self.recover_wait = (self.recover_wait * 2).min(RECOVER_WAIT_MAX_S);
        }
    }

    fn maybe_checkpoint(&mut self, now: u64) {
        let due = match self.last_ckpt {
            None => {
                self.last_ckpt = Some(now);
                false
            }
            Some(t) if now < t => {
                self.last_ckpt = Some(now);
                false
            }
            Some(t) => now - t >= CHECKPOINT_EVERY_S,
        };
        if !due {
            return;
        }
        self.last_ckpt = Some(now);

        if !self.slot_ok {
            self.slot_ok = self.store.checkpoint_slot_free();
        }
        if !self.slot_ok || self.need_recover {
            if self.next_recover > now + RECOVER_WAIT_MAX_S {
                self.next_recover = now; // clock went backwards
            }
            if now >= self.next_recover {
                self.run_recover(now);
            }
            if !self.slot_ok {
                return;
            }
        }

        match self.tracker.current() {
            Some(sess) => {
                let exe = match &self.exe {
                    Some((g, e)) if *g == sess.game => e.clone(),
                    _ => String::new(),
                };
                let rec = self.record(&sess, exe);
                let mut res = self.store.write_checkpoint(&rec);
                if res.is_err() {
                    std::thread::sleep(std::time::Duration::from_millis(50));
                    res = self.store.write_checkpoint(&rec);
                }
                match res {
                    Ok(()) => self.ckpt = Some((sess.game.clone(), sess.start)),
                    Err(e) => log::log_line(&format!("checkpoint write failed: {e}")),
                }
            }
            None if self.ckpt.is_some() => match self.store.clear_checkpoint() {
                Ok(()) => self.ckpt = None,
                Err(e) => log::log_line(&format!("checkpoint clear failed: {e}")),
            },
            None => {}
        }
    }
}

fn write_err_file(msg: &str) {
    let path = std::env::temp_dir().join("gametime-tracker.err");
    let _ = std::fs::write(path, format!("{msg}\n"));
}

/// Run the tracker until the stop event is signaled. Returns the exit code.
pub fn run(verbose: bool) -> i32 {
    let dir = match config::data_dir() {
        Ok(d) => d,
        Err(e) => {
            write_err_file(&format!("cannot resolve data directory: {e}"));
            return 1;
        }
    };
    log::init(&dir, verbose);
    std::panic::set_hook(Box::new(|info| {
        log::try_log_line(&format!("panic: {info}"));
    }));

    let cfg = match Config::load_or_create(&dir) {
        Ok(c) => c,
        Err(e) => {
            log::log_line(&format!("config error ({e}); using defaults"));
            Config::default()
        }
    };

    // Lifetimes: the mutex is declared first so the event drops before it.
    let Some(_instance) = win32::SingleInstance::acquire(MUTEX_NAME) else {
        log::log_line("tracker already running; exiting");
        return 0;
    };
    let Some(stop) = win32::StopEvent::create(STOP_EVENT_NAME) else {
        log::log_line("cannot create stop event; exiting");
        return 1;
    };

    let store = match Store::open(&dir) {
        Ok(s) => s,
        Err(e) => {
            log::log_line(&format!("cannot open store: {e}"));
            return 1;
        }
    };
    let rv = store.recover();
    log::log_line(&format!(
        "tracker started; recovered {} session(s)",
        rv.recovered.len()
    ));
    for e in &rv.errors {
        log::log_line(&format!("recover error: {e}"));
    }

    let db = detect::scan_system(&cfg);
    let mut per_source: BTreeMap<&str, usize> = BTreeMap::new();
    for g in db.games() {
        *per_source.entry(g.source.as_str()).or_insert(0) += 1;
    }
    log::log_line(&format!(
        "detected {} games {:?}",
        db.games().len(),
        per_source
    ));

    win32::trim_working_set();
    let mut engine = Engine::new(cfg, db, detect::scan_system, store, rv.checkpoint_free);
    engine.need_recover = !rv.errors.is_empty();
    while !stop.wait(1000) {
        engine.tick(win32::now_utc_secs(), win32::foreground());
    }
    engine.shutdown();
    log::log_line("tracker stopped");
    0
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::detect::Game;
    use std::fs;

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(tag: &str) -> TempDir {
            let d = std::env::temp_dir().join(format!(
                "gametime-test-{}-tracker-{tag}",
                std::process::id()
            ));
            let _ = fs::remove_dir_all(&d);
            fs::create_dir_all(&d).unwrap();
            TempDir(d)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn game(name: &str, source: Source, dir: &str) -> Game {
        Game {
            name: name.into(),
            source,
            install_dir: PathBuf::from(dir),
        }
    }

    fn games() -> Vec<Game> {
        vec![
            game("Alpha", Source::Steam, "D:\\Lib\\Alpha"),
            game("Beta", Source::Epic, "D:\\Lib\\Beta"),
        ]
    }

    fn scan_fake(cfg: &Config) -> GameDb {
        GameDb::new(games(), &cfg.ignore)
    }

    const ALPHA: &str = "D:\\Lib\\Alpha\\bin\\alpha.exe";
    const BETA: &str = "D:\\Lib\\Beta\\beta.exe";
    const OTHER: &str = "C:\\Windows\\notepad.exe";

    fn exe(pid: u32, p: &str) -> Foreground {
        Foreground::Exe {
            pid,
            path: PathBuf::from(p),
        }
    }

    fn engine(d: &TempDir, free: bool) -> Engine {
        let cfg = Config::default();
        let db = scan_fake(&cfg);
        Engine::new(cfg, db, scan_fake, Store::open(&d.0).unwrap(), free)
    }

    fn records(d: &TempDir) -> Vec<SessionRecord> {
        Store::open(&d.0).unwrap().load_all().unwrap()
    }

    /// Feed one tick per second from `t0` over `n` seconds.
    fn run_for(e: &mut Engine, t0: u64, n: u64, fg: impl Fn() -> Foreground) {
        for t in t0..t0 + n {
            e.tick(t, fg());
        }
    }

    #[test]
    fn simple_session_is_recorded() {
        let d = TempDir::new("simple");
        let mut e = engine(&d, true);
        run_for(&mut e, 1000, 11, || exe(1, ALPHA));
        run_for(&mut e, 1011, 100, || Foreground::None);
        let r = records(&d);
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].game, "Alpha");
        assert_eq!(r[0].source, "steam");
        assert_eq!(r[0].exe, ALPHA);
        assert_eq!(r[0].start_utc, 1000);
        // Focus loss is first seen at the poll after the last focused one.
        assert_eq!(r[0].end_utc, 1011);
        assert_eq!(r[0].active_s, 11);
    }

    #[test]
    fn alt_tab_and_resume_merge() {
        let d = TempDir::new("resume");
        let mut e = engine(&d, true);
        run_for(&mut e, 1000, 11, || exe(1, ALPHA));
        run_for(&mut e, 1011, 20, || exe(2, OTHER)); // alt-tab, within merge gap
        run_for(&mut e, 1031, 10, || exe(1, ALPHA));
        assert!(records(&d).is_empty());
        e.shutdown();
        let r = records(&d);
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].start_utc, 1000);
        assert_eq!(r[0].exe, ALPHA);
        assert!(r[0].active_s >= 19);
    }

    #[test]
    fn switching_games_closes_first() {
        let d = TempDir::new("switch");
        let mut e = engine(&d, true);
        run_for(&mut e, 1000, 10, || exe(1, ALPHA));
        run_for(&mut e, 1010, 10, || exe(2, BETA));
        e.shutdown();
        let r = records(&d);
        assert_eq!(r.len(), 2);
        assert_eq!((r[0].game.as_str(), r[0].exe.as_str()), ("Alpha", ALPHA));
        assert_eq!(
            (r[1].game.as_str(), r[1].source.as_str(), r[1].exe.as_str()),
            ("Beta", "epic", BETA)
        );
    }

    #[test]
    fn exe_is_first_seen_for_session() {
        let d = TempDir::new("exe");
        let mut e = engine(&d, true);
        run_for(&mut e, 1000, 5, || exe(1, ALPHA));
        run_for(&mut e, 1005, 5, || exe(3, "D:\\Lib\\Alpha\\launcher2.exe"));
        e.shutdown();
        assert_eq!(records(&d)[0].exe, ALPHA);
    }

    #[test]
    fn shutdown_flushes_and_clears_checkpoint() {
        let d = TempDir::new("shutdown");
        let mut e = engine(&d, true);
        run_for(&mut e, 1000, 40, || exe(1, ALPHA));
        assert!(d.0.join("current.json").exists());
        e.shutdown();
        assert_eq!(records(&d).len(), 1);
        assert!(!d.0.join("current.json").exists());
    }

    #[test]
    fn checkpoint_every_30s_and_cleared_when_idle() {
        let d = TempDir::new("ckpt");
        let mut e = engine(&d, true);
        run_for(&mut e, 1000, 29, || exe(1, ALPHA));
        assert!(!d.0.join("current.json").exists());
        run_for(&mut e, 1029, 2, || exe(1, ALPHA));
        let s = Store::open(&d.0).unwrap();
        let c = s.load_checkpoint().expect("checkpoint written");
        assert_eq!(c.game, "Alpha");
        assert_eq!(c.exe, ALPHA);
        assert_eq!(c.start_utc, 1000);
        // Game closed by merge-gap expiry: close_session clears the file.
        run_for(&mut e, 1031, 120, || Foreground::None);
        assert!(!d.0.join("current.json").exists());
        assert_eq!(records(&d).len(), 1);
    }

    #[test]
    fn checkpoint_cleared_when_nothing_to_snapshot() {
        let d = TempDir::new("ckpt-idle");
        let mut e = engine(&d, true);
        run_for(&mut e, 1000, 31, || exe(1, ALPHA));
        assert!(d.0.join("current.json").exists());
        // Simulate an open session without credited time at the next tick.
        e.tracker.flush();
        run_for(&mut e, 1031, 31, || Foreground::None);
        assert!(!d.0.join("current.json").exists());
    }

    #[test]
    fn busy_slot_is_not_overwritten_and_recover_is_retried() {
        use std::os::windows::fs::OpenOptionsExt;
        let d = TempDir::new("busy");
        let s = Store::open(&d.0).unwrap();
        let old = SessionRecord {
            game: "Old".into(),
            source: "gog".into(),
            exe: "x".into(),
            start_utc: 5,
            end_utc: 50,
            active_s: 45,
        };
        s.write_checkpoint(&old).unwrap();
        // Readable but not deletable/renamable: recovery cannot free the slot.
        let h = fs::OpenOptions::new()
            .read(true)
            .share_mode(1)
            .open(d.0.join("current.json"))
            .unwrap();
        let mut e = engine(&d, false);
        run_for(&mut e, 1000, 65, || exe(1, ALPHA));
        assert_eq!(s.load_checkpoint().unwrap().game, "Old");
        drop(h);
        // Once released, a later checkpoint tick recovers and then writes.
        run_for(&mut e, 1065, 70, || exe(1, ALPHA));
        assert_eq!(s.load_checkpoint().unwrap().game, "Alpha");
        let names: Vec<_> = records(&d).iter().map(|r| r.game.clone()).collect();
        assert_eq!(names, vec!["Old"]);
    }

    #[test]
    fn leftover_checkpoint_is_recovered_not_overwritten() {
        let d = TempDir::new("leftover");
        let s = Store::open(&d.0).unwrap();
        let old = SessionRecord {
            game: "Old".into(),
            source: "gog".into(),
            exe: "x".into(),
            start_utc: 5,
            end_utc: 50,
            active_s: 45,
        };
        s.write_checkpoint(&old).unwrap();
        let mut e = engine(&d, false); // as if startup recovery failed to free it
        run_for(&mut e, 1000, 31, || exe(1, ALPHA));
        let names: Vec<_> = records(&d).iter().map(|r| r.game.clone()).collect();
        assert_eq!(names, vec!["Old"]);
        assert_eq!(s.load_checkpoint().unwrap().game, "Alpha");
    }

    #[test]
    fn denied_pid_logged_once_and_set_is_bounded() {
        let d = TempDir::new("denied");
        let mut e = engine(&d, true);
        for t in 0..5 {
            e.tick(1000 + t, Foreground::Denied(77));
        }
        assert_eq!(e.denied.len(), 1);
        for pid in 0..(MAX_DENIED_PIDS as u32 * 2) {
            e.tick(2000 + pid as u64, Foreground::Denied(pid));
        }
        assert!(e.denied.len() <= MAX_DENIED_PIDS);
        assert!(records(&d).is_empty());
    }

    #[test]
    fn cache_avoids_rematching() {
        let d = TempDir::new("cache");
        let mut e = engine(&d, true);
        run_for(&mut e, 1000, 20, || exe(1, ALPHA));
        assert_eq!(e.match_calls, 1);
        run_for(&mut e, 1020, 5, || exe(2, BETA));
        assert_eq!(e.match_calls, 2);
        run_for(&mut e, 1025, 5, || exe(2, BETA));
        assert_eq!(e.match_calls, 2);
        // Same pid, different path (pid reuse) is matched again.
        e.tick(1030, exe(2, OTHER));
        assert_eq!(e.match_calls, 3);
    }

    static SCANS: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

    fn scan_counting(cfg: &Config) -> GameDb {
        SCANS.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        scan_fake(cfg)
    }

    #[test]
    fn rescan_deferred_while_playing_then_runs_when_idle() {
        use std::sync::atomic::Ordering::SeqCst;
        let d = TempDir::new("rescan");
        let cfg = Config::default();
        let db = scan_fake(&cfg);
        let mut e = Engine::new(cfg, db, scan_counting, Store::open(&d.0).unwrap(), true);
        let before = SCANS.load(SeqCst);
        run_for(&mut e, 1000, RESCAN_EVERY_S + 20, || exe(1, ALPHA));
        assert_eq!(SCANS.load(SeqCst), before, "deferred while playing");
        assert!(records(&d).is_empty(), "session must stay open");
        // Focus lost; once the merge gap expires the tracker is idle and the
        // deferred rescan runs. The session is still intact.
        run_for(&mut e, 1000 + RESCAN_EVERY_S + 20, 100, || Foreground::None);
        assert_eq!(SCANS.load(SeqCst), before + 1);
        let r = records(&d);
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].start_utc, 1000);
    }

    #[test]
    fn foreign_checkpoint_survives_direct_switch_and_is_recovered() {
        use std::os::windows::fs::OpenOptionsExt;
        let d = TempDir::new("foreign");
        let s = Store::open(&d.0).unwrap();
        let old = SessionRecord {
            game: "Old".into(),
            source: "gog".into(),
            exe: "x".into(),
            start_utc: 5,
            end_utc: 50,
            active_s: 45,
        };
        s.write_checkpoint(&old).unwrap();
        let h = fs::OpenOptions::new()
            .read(true)
            .share_mode(1)
            .open(d.0.join("current.json"))
            .unwrap();
        let mut e = engine(&d, false);
        run_for(&mut e, 1000, 10, || exe(1, ALPHA));
        run_for(&mut e, 1010, 5, || exe(2, BETA)); // direct switch closes Alpha
        assert_eq!(s.load_checkpoint().unwrap().game, "Old");
        assert!(records(&d).iter().any(|r| r.game == "Alpha"));
        drop(h);
        run_for(&mut e, 1015, 200, || exe(2, BETA));
        let names: Vec<_> = records(&d).iter().map(|r| r.game.clone()).collect();
        assert!(names.contains(&"Old".to_string()), "{names:?}");
        assert!(names.contains(&"Alpha".to_string()));
    }

    #[test]
    fn commit_without_ownership_never_touches_checkpoint() {
        let d = TempDir::new("own");
        let s = Store::open(&d.0).unwrap();
        let prefix = SessionRecord {
            game: "Alpha".into(),
            source: "steam".into(),
            exe: "x".into(),
            start_utc: 1000,
            end_utc: 1030,
            active_s: 30,
        };
        s.write_checkpoint(&prefix).unwrap();
        let mut e = engine(&d, false); // S1 failed earlier: slot not ours
        let sess = Session {
            game: GameKey {
                name: "Beta".into(),
                source: Source::Epic,
            },
            start: 1200,
            end: 1210,
            active_s: 10,
        };
        e.commit(sess, "b.exe".into(), 1210);
        assert_eq!(s.load_checkpoint().unwrap(), prefix);
        assert_eq!(records(&d).len(), 1);
        // Even with slot_ok set, a non-matching checkpoint owner is not cleared.
        e.slot_ok = true;
        e.ckpt = Some((
            GameKey {
                name: "Alpha".into(),
                source: Source::Steam,
            },
            1000,
        ));
        let sess2 = Session {
            game: GameKey {
                name: "Beta".into(),
                source: Source::Epic,
            },
            start: 1300,
            end: 1310,
            active_s: 10,
        };
        e.commit(sess2, "b.exe".into(), 1310);
        assert_eq!(s.load_checkpoint().unwrap(), prefix);
        assert_eq!(records(&d).len(), 2);
    }

    #[test]
    fn failed_session_prefix_is_recoverable_after_retries_give_up() {
        let d = TempDir::new("prefix");
        let mut e = engine(&d, true);
        fs::create_dir(d.0.join("sessions.jsonl")).unwrap();
        run_for(&mut e, 1000, 40, || exe(1, ALPHA)); // checkpoint at 1030
        run_for(&mut e, 1040, 130, || Foreground::None); // S1 closes, append fails
        fs::remove_dir(d.0.join("sessions.jsonl")).unwrap();
        // Retries may already have given up; either way S2 must not delete S1's
        // prefix, and recover() must absorb it.
        run_for(&mut e, 1170, 10, || exe(2, BETA));
        run_for(&mut e, 1180, 200, || Foreground::None);
        let names: Vec<_> = records(&d).iter().map(|r| r.game.clone()).collect();
        assert!(names.contains(&"Alpha".to_string()), "{names:?}");
        assert!(names.contains(&"Beta".to_string()), "{names:?}");
    }

    #[test]
    fn recover_backs_off_while_slot_busy_and_resets() {
        use std::os::windows::fs::OpenOptionsExt;
        let d = TempDir::new("backoff");
        let s = Store::open(&d.0).unwrap();
        let old = SessionRecord {
            game: "Old".into(),
            source: "gog".into(),
            exe: "x".into(),
            start_utc: 5,
            end_utc: 50,
            active_s: 45,
        };
        s.write_checkpoint(&old).unwrap();
        let h = fs::OpenOptions::new()
            .read(true)
            .share_mode(1)
            .open(d.0.join("current.json"))
            .unwrap();
        let mut e = engine(&d, false);
        run_for(&mut e, 1000, 301, || Foreground::None);
        // Attempts at 1030, 1060, 1120, 1240 instead of every 30 s.
        assert_eq!(e.recover_calls, 4);
        assert_eq!(e.recover_wait, 480);
        drop(h);
        run_for(&mut e, 1301, 250, || Foreground::None);
        assert_eq!(e.recover_wait, RECOVER_WAIT_MIN_S);
        assert!(records(&d).iter().any(|r| r.game == "Old"));
    }

    #[test]
    fn retry_timer_recovers_from_backwards_clock() {
        let d = TempDir::new("retry-clock");
        let mut e = engine(&d, true);
        let rec = SessionRecord {
            game: "A".into(),
            source: "steam".into(),
            exe: String::new(),
            start_utc: 1,
            end_utc: 5,
            active_s: 4,
        };
        e.retry.push(PendingAppend {
            rec,
            attempts: 1,
            next_at: 1_000_000,
        });
        e.tick(100, Foreground::None);
        assert_eq!(e.retry.len(), 1);
        assert_eq!(e.retry[0].next_at, 100 + RETRY_EVERY_S);
        e.tick(100 + RETRY_EVERY_S, Foreground::None);
        assert!(e.retry.is_empty());
        assert_eq!(records(&d).len(), 1);
    }

    #[test]
    fn exe_is_cleared_when_idle() {
        let d = TempDir::new("execlear");
        let mut e = engine(&d, true);
        run_for(&mut e, 1000, 10, || exe(1, ALPHA));
        assert!(e.exe.is_some());
        run_for(&mut e, 1010, 100, || Foreground::None);
        assert!(e.exe.is_none());
    }

    #[test]
    fn unknown_is_not_added_to_denied_set() {
        let d = TempDir::new("unknown");
        let mut e = engine(&d, true);
        e.tick(1000, Foreground::Unknown(5));
        assert!(e.denied.is_empty());
    }

    #[test]
    fn append_failure_is_retried_and_clear_failure_is_not_reappended() {
        let d = TempDir::new("append");
        let mut e = engine(&d, true);
        // Block appends: a directory where sessions.jsonl should be.
        fs::create_dir(d.0.join("sessions.jsonl")).unwrap();
        run_for(&mut e, 1000, 10, || exe(1, ALPHA));
        run_for(&mut e, 1010, 70, || Foreground::None);
        assert_eq!(e.retry.len(), 1);
        fs::remove_dir(d.0.join("sessions.jsonl")).unwrap();
        run_for(&mut e, 1080, 15, || Foreground::None);
        assert!(e.retry.is_empty());
        assert_eq!(records(&d).len(), 1);
    }

    #[test]
    fn append_retries_are_capped() {
        let d = TempDir::new("append-cap");
        let mut e = engine(&d, true);
        fs::create_dir(d.0.join("sessions.jsonl")).unwrap();
        run_for(&mut e, 1000, 10, || exe(1, ALPHA));
        run_for(&mut e, 1010, 100, || Foreground::None);
        run_for(&mut e, 1080, 100, || Foreground::None);
        assert!(e.retry.is_empty()); // given up (logged), not retried forever
    }

    #[test]
    fn clear_failure_does_not_duplicate() {
        let d = TempDir::new("clear");
        let mut e = engine(&d, true);
        run_for(&mut e, 1000, 31, || exe(1, ALPHA)); // checkpoint exists
                                                     // Hold current.json open without share-delete to make removal fail.
        let _h = {
            use std::os::windows::fs::OpenOptionsExt;
            fs::OpenOptions::new()
                .read(true)
                .share_mode(1) // FILE_SHARE_READ only
                .open(d.0.join("current.json"))
                .unwrap()
        };
        run_for(&mut e, 1031, 100, || Foreground::None);
        assert_eq!(records(&d).len(), 1);
        assert!(e.retry.is_empty());
        drop(_h);
        run_for(&mut e, 1131, 40, || Foreground::None);
        assert_eq!(records(&d).len(), 1);
        assert!(!d.0.join("current.json").exists());
    }

    #[test]
    fn backwards_clock_does_not_panic() {
        let d = TempDir::new("clock");
        let mut e = engine(&d, true);
        run_for(&mut e, 5000, 10, || exe(1, ALPHA));
        run_for(&mut e, 100, 10, || exe(1, ALPHA));
        e.shutdown();
        assert!(!records(&d).is_empty());
    }

    #[test]
    fn game_key_identity() {
        let a = GameKey {
            name: "A".into(),
            source: Source::Steam,
        };
        assert_eq!(
            a,
            GameKey {
                name: "A".into(),
                source: Source::Steam
            }
        );
        assert_ne!(
            a,
            GameKey {
                name: "A".into(),
                source: Source::Epic
            }
        );
    }
}
