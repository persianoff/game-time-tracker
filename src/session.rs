//! Pure session state machine: no Win32, no I/O, no clock reads.
//!
//! The caller polls (about once per second) and calls [`Tracker::step`] with the
//! current UTC epoch seconds and the foreground game, if any.
//!
//! Attribution is sample-and-hold: the interval `[last_poll, now]` is credited
//! to whatever was focused at `last_poll`, provided `0 <= now - last_poll <=
//! sleep_gap_s`. Consequently:
//! - a game focused for a single poll followed by focus loss gets one poll
//!   interval;
//! - on focus loss (or a direct switch) `end` is the poll at which the change
//!   was first observed, and that is also the pause start;
//! - on resume within the merge gap, the paused gap is credited to nothing;
//! - after a sleep gap or a backwards clock the gap is not credited and the open
//!   session is closed at its last credited time.

/// A closed (or snapshotted) play session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Session<G> {
    pub game: G,
    /// UTC epoch seconds of the poll that first saw the game focused.
    pub start: u64,
    /// UTC epoch seconds up to which time was credited.
    pub end: u64,
    /// Seconds the game was credited as being in the foreground.
    pub active_s: u64,
}

/// Tunable thresholds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Params {
    /// A paused session resumes if the same game regains focus within this many
    /// seconds of the pause start (inclusive).
    pub merge_gap_s: u64,
    /// A gap between polls larger than this (or a clock going backwards) is
    /// treated as sleep/hibernate/clock jump: the open session is closed at its
    /// last credited time and the gap is not credited.
    pub sleep_gap_s: u64,
}

impl Default for Params {
    fn default() -> Self {
        Params {
            merge_gap_s: 60,
            sleep_gap_s: 10,
        }
    }
}

#[derive(Debug, Clone)]
struct Open<G> {
    game: G,
    start: u64,
    /// Last credited time (pause start when `!focused`).
    end: u64,
    active_s: u64,
    focused: bool,
}

impl<G> Open<G> {
    /// Sessions with no credited time are dropped.
    fn into_session(self) -> Option<Session<G>> {
        (self.active_s > 0).then(|| Session {
            game: self.game,
            start: self.start,
            end: self.end,
            active_s: self.active_s,
        })
    }
}

/// The state machine. Idle when `open` is `None`; Active when `open.focused`;
/// Paused when `open` is set and not focused.
#[derive(Debug, Clone)]
pub struct Tracker<G: Clone + PartialEq> {
    params: Params,
    open: Option<Open<G>>,
    last_poll: Option<u64>,
}

impl<G: Clone + PartialEq> Tracker<G> {
    pub fn new(params: Params) -> Self {
        Tracker {
            params,
            open: None,
            last_poll: None,
        }
    }

    fn close_open(&mut self) -> Option<Session<G>> {
        self.open.take().and_then(Open::into_session)
    }

    fn open_new(now: u64, game: &G) -> Open<G> {
        Open {
            game: game.clone(),
            start: now,
            end: now,
            active_s: 0,
            focused: true,
        }
    }

    /// Process one poll. Returns the session that closed during this step, if any.
    ///
    /// At most one session can close per step: a close (sleep gap, merge-gap
    /// expiry, or a switch to another game) always leaves the tracker idle
    /// before a new session is opened, so the return type is `Option` and the
    /// common path never allocates.
    pub fn step(&mut self, now: u64, fg: Option<&G>) -> Option<Session<G>> {
        let mut closed = None;

        let lp = self.last_poll.replace(now);
        let slept = matches!(lp, Some(l) if now < l || now - l > self.params.sleep_gap_s);
        if slept {
            // Close at the last credited time; the gap is not credited.
            closed = self.close_open();
        } else if let (Some(l), Some(o)) = (lp, self.open.as_mut()) {
            if o.focused {
                // Sample-and-hold: credit the interval to the game focused at `l`.
                o.active_s += now - l;
                o.end = now;
            } else if now - o.end > self.params.merge_gap_s {
                // Paused too long: close at pause start.
                closed = self.close_open();
            }
        }

        match (&mut self.open, fg) {
            (None, Some(g)) => self.open = Some(Self::open_new(now, g)),
            (None, None) => {}
            (Some(o), Some(g)) if *g == o.game => {
                // Resume (if paused) or continue; a paused gap is not credited.
                o.focused = true;
                o.end = now;
            }
            (Some(_), Some(g)) => {
                debug_assert!(closed.is_none());
                closed = self.close_open();
                self.open = Some(Self::open_new(now, g));
            }
            // Focus lost: `end` is already `now` if it was credited; the pause
            // starts here. If already paused this is a no-op.
            (Some(o), None) => o.focused = false,
        }
        closed
    }

    /// True if there is no open or paused session.
    pub fn is_idle(&self) -> bool {
        self.open.is_none()
    }

    /// Snapshot of the open or paused session (for checkpointing), with `end` at
    /// the last credited time. It is always a prefix of the session that will
    /// eventually close. `None` if idle or nothing has been credited yet.
    /// Start time of the session [`Tracker::current`] would return (no allocation).
    pub fn current_start(&self) -> Option<u64> {
        self.open
            .as_ref()
            .filter(|o| o.active_s > 0)
            .map(|o| o.start)
    }

    pub fn current(&self) -> Option<Session<G>> {
        self.open
            .as_ref()
            .filter(|o| o.active_s > 0)
            .map(|o| Session {
                game: o.game.clone(),
                start: o.start,
                end: o.end,
                active_s: o.active_s,
            })
    }

    /// Close everything (stop/shutdown). Resets the tracker to idle.
    pub fn flush(&mut self) -> Option<Session<G>> {
        self.last_poll = None;
        self.close_open()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    type T = Tracker<char>;

    fn tr() -> T {
        Tracker::new(Params::default())
    }

    fn feed(t: &mut T, seq: &[(u64, Option<char>)]) -> Vec<Session<char>> {
        seq.iter()
            .filter_map(|(now, fg)| t.step(*now, fg.as_ref()))
            .collect()
    }

    fn s(game: char, start: u64, end: u64, active_s: u64) -> Session<char> {
        Session {
            game,
            start,
            end,
            active_s,
        }
    }

    /// Poll every second over `[from, to]` with a constant fg.
    fn run(from: u64, to: u64, fg: Option<char>) -> Vec<(u64, Option<char>)> {
        (from..=to).map(|t| (t, fg)).collect()
    }

    #[test]
    fn continuous_play() {
        let mut t = tr();
        assert!(feed(&mut t, &run(100, 200, Some('a'))).is_empty());
        assert_eq!(t.flush(), Some(s('a', 100, 200, 100)));
        assert_eq!(t.flush(), None);
    }

    #[test]
    fn alt_tab_within_merge_gap_is_one_session() {
        let mut t = tr();
        let mut seq = run(0, 10, Some('a'));
        seq.extend(run(11, 40, None));
        seq.extend(run(41, 50, Some('a')));
        assert!(feed(&mut t, &seq).is_empty());
        // 0..11 credited (11 s, loss seen at 11); gap 11..41 not credited; 41..50 = 9 s.
        assert_eq!(t.flush(), Some(s('a', 0, 50, 20)));
    }

    #[test]
    fn merge_gap_boundary_inclusive() {
        let p = Params {
            merge_gap_s: 60,
            sleep_gap_s: 1000,
        };
        // Pause starts at 11; back at 71 (gap exactly 60): merged.
        let mut t = Tracker::new(p);
        let mut seq = run(0, 10, Some('a'));
        seq.extend(run(11, 70, None));
        seq.extend([(71, Some('a')), (72, Some('a'))]);
        assert!(feed(&mut t, &seq).is_empty());
        assert_eq!(t.flush(), Some(s('a', 0, 72, 12)));
        // Back at 72 (gap 61): separate sessions.
        let mut t = Tracker::new(p);
        let mut seq = run(0, 10, Some('a'));
        seq.extend(run(11, 71, None));
        seq.extend([(72, Some('a')), (73, Some('a'))]);
        assert_eq!(feed(&mut t, &seq), vec![s('a', 0, 11, 11)]);
        assert_eq!(t.flush(), Some(s('a', 72, 73, 1)));
    }

    #[test]
    fn alt_tab_beyond_merge_gap_closes_at_pause_start() {
        let mut t = tr();
        let mut seq = run(0, 10, Some('a'));
        seq.extend(run(11, 80, None));
        let closed = feed(&mut t, &seq);
        assert_eq!(closed, vec![s('a', 0, 11, 11)]);
        // Returning later starts a fresh session.
        let closed = feed(&mut t, &run(81, 90, Some('a')));
        assert!(closed.is_empty());
        assert_eq!(t.flush(), Some(s('a', 81, 90, 9)));
    }

    #[test]
    fn direct_switch_between_games() {
        let mut t = tr();
        let mut seq = run(0, 10, Some('a'));
        seq.extend(run(11, 20, Some('b')));
        let closed = feed(&mut t, &seq);
        assert_eq!(closed, vec![s('a', 0, 11, 11)]);
        assert_eq!(t.flush(), Some(s('b', 11, 20, 9)));
    }

    #[test]
    fn switch_while_paused() {
        let mut t = tr();
        let mut seq = run(0, 10, Some('a'));
        seq.extend(run(11, 20, None));
        seq.extend(run(21, 30, Some('b')));
        let closed = feed(&mut t, &seq);
        assert_eq!(closed, vec![s('a', 0, 11, 11)]);
        assert_eq!(t.flush(), Some(s('b', 21, 30, 9)));
    }

    #[test]
    fn sleep_gap_closes_at_last_poll() {
        let mut t = tr();
        let mut seq = run(0, 10, Some('a'));
        seq.push((500, Some('a'))); // woke from sleep still on game
        seq.push((501, Some('a')));
        let closed = feed(&mut t, &seq);
        assert_eq!(closed, vec![s('a', 0, 10, 10)]);
        assert_eq!(t.flush(), Some(s('a', 500, 501, 1)));
    }

    #[test]
    fn sleep_gap_while_paused() {
        let mut t = tr();
        let mut seq = run(0, 10, Some('a'));
        seq.extend(run(11, 15, None));
        seq.push((1000, None));
        let closed = feed(&mut t, &seq);
        assert_eq!(closed, vec![s('a', 0, 11, 11)]);
        assert_eq!(t.current(), None);
    }

    #[test]
    fn clock_going_backwards_closes() {
        let mut t = tr();
        let mut seq = run(100, 110, Some('a'));
        seq.push((50, Some('a')));
        seq.push((51, Some('a')));
        let closed = feed(&mut t, &seq);
        assert_eq!(closed, vec![s('a', 100, 110, 10)]);
        assert_eq!(t.flush(), Some(s('a', 50, 51, 1)));
    }

    #[test]
    fn flush_mid_session_and_while_paused() {
        let mut t = tr();
        feed(&mut t, &run(0, 5, Some('a')));
        assert_eq!(t.flush(), Some(s('a', 0, 5, 5)));

        feed(&mut t, &run(10, 15, Some('a')));
        feed(&mut t, &run(16, 20, None));
        assert_eq!(t.flush(), Some(s('a', 10, 16, 6)));
        assert_eq!(t.current(), None);
    }

    #[test]
    fn current_snapshots() {
        let mut t = tr();
        assert_eq!(t.current(), None);
        t.step(0, Some(&'a'));
        assert_eq!(t.current(), None); // nothing credited yet
        t.step(1, Some(&'a'));
        t.step(2, Some(&'a'));
        assert_eq!(t.current(), Some(s('a', 0, 2, 2)));
        t.step(3, None);
        t.step(4, None);
        assert_eq!(t.current(), Some(s('a', 0, 3, 3))); // paused: end = loss poll
        t.step(5, Some(&'a'));
        assert_eq!(t.current(), Some(s('a', 0, 5, 3))); // gap not credited
        t.step(6, Some(&'a'));
        assert_eq!(t.current(), Some(s('a', 0, 6, 4)));
    }

    #[test]
    fn single_poll_focus_gets_one_interval() {
        let mut t = tr();
        let mut seq = vec![(0, Some('a'))];
        seq.extend(run(1, 100, None));
        assert_eq!(feed(&mut t, &seq), vec![s('a', 0, 1, 1)]);
        assert_eq!(t.flush(), None);

        // Single-poll game between two stretches of another game.
        let mut seq = run(200, 210, Some('a'));
        seq.push((211, Some('b')));
        seq.extend(run(212, 220, Some('a')));
        let closed = feed(&mut t, &seq);
        assert_eq!(closed, vec![s('a', 200, 211, 11), s('b', 211, 212, 1)]);
        assert_eq!(t.flush(), Some(s('a', 212, 220, 8)));
    }

    #[test]
    fn zero_credit_sessions_dropped() {
        let mut t = tr();
        t.step(0, Some(&'a'));
        assert_eq!(t.flush(), None);
        // Duplicate-time switch: a gets 0 s and is dropped.
        t.step(5, Some(&'a'));
        assert_eq!(t.step(5, Some(&'b')), None);
        t.step(6, Some(&'b'));
        assert_eq!(t.flush(), Some(s('b', 5, 6, 1)));
    }

    #[test]
    fn six_hours_of_one_second_polls() {
        let mut t = tr();
        let n = 6 * 3600;
        assert!(feed(&mut t, &run(1_000_000, 1_000_000 + n, Some('a'))).is_empty());
        assert_eq!(t.flush(), Some(s('a', 1_000_000, 1_000_000 + n, n)));
    }

    /// Deterministic LCG, no external crates.
    struct Lcg(u64);
    impl Lcg {
        fn next(&mut self) -> u64 {
            self.0 = self
                .0
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            self.0 >> 33
        }
    }

    /// `a` is a checkpoint of `b`: same game and start, end and credit not larger.
    fn is_prefix(a: &Session<char>, b: &Session<char>) -> bool {
        a.game == b.game && a.start == b.start && a.end <= b.end && a.active_s <= b.active_s
    }

    fn gi(c: char) -> usize {
        (c as u8 - b'a') as usize
    }

    /// Irregular polling (0..=3 s steps incl. duplicates, rare sleep gaps, rarer
    /// backwards clock) over random foreground changes, checked against a
    /// per-game oracle and structural invariants.
    #[test]
    fn property_random_foreground() {
        let params = Params::default();
        for seed in 0..20u64 {
            let mut rng = Lcg(seed + 1);
            let mut t = Tracker::new(params);
            // (session, epoch); the epoch increments on every backwards clock.
            let mut sessions: Vec<(Session<char>, u32)> = Vec::new();
            // (time, epoch, fg) per poll, chronological by index.
            let mut polls: Vec<(u64, u32, Option<char>)> = Vec::new();
            // Forward sleep events: (prev poll time, poll time, epoch).
            let mut sleeps: Vec<(u64, u64, u32)> = Vec::new();
            let mut oracle = [0u64; 3];
            let mut epoch = 0u32;
            let mut fg: Option<char> = None;
            let mut now = 10_000u64;
            let mut prev: Option<(u64, Option<char>)> = None;
            let mut last_snap: Option<Session<char>> = None;

            for i in 0..10_000 {
                if i > 0 {
                    let r = rng.next() % 1000;
                    now = if r < 5 {
                        now.saturating_sub(1 + rng.next() % 50)
                    } else if r < 20 {
                        now + 11 + rng.next() % 300
                    } else {
                        now + rng.next() % 4
                    };
                }
                if rng.next().is_multiple_of(50) {
                    fg = match rng.next() % 4 {
                        0 => None,
                        1 => Some('a'),
                        2 => Some('b'),
                        _ => Some('c'),
                    };
                }
                // A session returned by this step belongs to the old epoch.
                let tag = epoch;
                if let Some((pn, pf)) = prev {
                    if now < pn {
                        epoch += 1;
                    } else if now - pn > params.sleep_gap_s {
                        sleeps.push((pn, now, epoch));
                    } else if let Some(g) = pf {
                        oracle[gi(g)] += now - pn;
                    }
                }
                polls.push((now, epoch, fg));
                let closed = t.step(now, fg.as_ref());
                if let Some(x) = &closed {
                    // The snapshot taken before this step is a prefix of the
                    // session that closes now.
                    if let Some(snap) = &last_snap {
                        assert!(is_prefix(snap, x), "seed {seed}: {snap:?} vs {x:?}");
                    }
                    sessions.push((x.clone(), tag));
                }
                let snap = t.current();
                if closed.is_none() {
                    if let Some(old) = &last_snap {
                        let new = snap.as_ref().expect("snapshot vanished without a close");
                        assert!(is_prefix(old, new), "seed {seed}: {old:?} vs {new:?}");
                    }
                }
                last_snap = snap;
                prev = Some((now, fg));
            }
            if let Some(x) = t.flush() {
                if let Some(snap) = &last_snap {
                    assert!(is_prefix(snap, &x), "seed {seed}: {snap:?} vs {x:?}");
                }
                sessions.push((x, epoch));
            }

            let mut sums = [0u64; 3];
            let mut last_in_order: Option<&(Session<char>, u32)> = None;
            let mut last_of_game: [Option<&(Session<char>, u32)>; 3] = [None; 3];
            for item in &sessions {
                let (x, ex) = item;
                assert!(x.start <= x.end, "seed {seed}: {x:?}");
                assert!(x.active_s > 0, "seed {seed}: {x:?}");
                assert!(x.active_s <= x.end - x.start, "seed {seed}: {x:?}");
                sums[gi(x.game)] += x.active_s;

                // Sessions of one epoch don't overlap (adjacent ones may touch,
                // e.g. a direct switch closes and opens at the same poll).
                if let Some((p, ep)) = last_in_order {
                    if ep == ex {
                        assert!(x.start >= p.end, "seed {seed}: overlap {p:?} {x:?}");
                    }
                }
                last_in_order = Some(item);

                // No session spans a sleep gap.
                for &(a, b, e) in &sleeps {
                    if e == *ex {
                        assert!(
                            !(x.start <= a && x.end >= b),
                            "seed {seed}: {x:?} spans sleep"
                        );
                    }
                }

                // No session spans a poll of a different game, nor an
                // unfocused run longer than the merge gap.
                let mut pause_start: Option<u64> = None;
                for &(tm, e, f) in &polls {
                    if e != *ex || tm < x.start || tm > x.end {
                        continue;
                    }
                    if tm > x.start && tm < x.end {
                        assert!(
                            !matches!(f, Some(h) if h != x.game),
                            "seed {seed}: {x:?} spans other game at {tm}"
                        );
                    }
                    if f == Some(x.game) {
                        if let Some(ps) = pause_start.take() {
                            assert!(
                                tm - ps <= params.merge_gap_s,
                                "seed {seed}: {x:?} spans unfocused run {ps}..{tm}"
                            );
                        }
                    } else if pause_start.is_none() && tm > x.start {
                        pause_start = Some(tm);
                    }
                }

                // Same-game neighbours are split only for a legitimate reason.
                let g = gi(x.game);
                if let Some((p, ep)) = last_of_game[g] {
                    if ep == ex {
                        let big_gap = x.start.saturating_sub(p.end) > params.merge_gap_s;
                        let other = || {
                            polls.iter().any(|&(tm, e, f)| {
                                e == *ex
                                    && tm >= p.end
                                    && tm <= x.start
                                    && matches!(f, Some(h) if h != x.game)
                            })
                        };
                        let slept = || {
                            sleeps
                                .iter()
                                .any(|&(a, b, e)| e == *ex && a >= p.end && b <= x.start)
                        };
                        assert!(
                            big_gap || other() || slept(),
                            "seed {seed}: unjustified split {p:?} {x:?}"
                        );
                    }
                }
                last_of_game[g] = Some(item);
            }
            // Per-game oracle, tolerance 0.
            assert_eq!(sums, oracle, "seed {seed}");
        }
    }
}
