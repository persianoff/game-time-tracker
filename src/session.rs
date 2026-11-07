//! Pure session state machine: no Win32, no I/O, no clock reads.
//!
//! The caller polls (about once per second) and calls [`Tracker::step`] with the
//! current UTC epoch seconds and the foreground game, if any.
//!
//! Attribution rule (error at most one poll): a poll interval `[last_poll, now]`
//! is credited to `active_s` only if the game was in the foreground at *both*
//! ends. When focus is lost, the session's `end` is the last poll at which the
//! game was still focused; the interval up to the poll that noticed the loss is
//! not counted. The same applies on resume: the gap (including the resume poll
//! interval) is not counted.

/// A closed (or snapshotted) play session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Session<G> {
    pub game: G,
    /// UTC epoch seconds of the first focused poll.
    pub start: u64,
    /// UTC epoch seconds of the last focused poll.
    pub end: u64,
    /// Seconds the game was actually in the foreground.
    pub active_s: u64,
}

/// Tunable thresholds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Params {
    /// A paused session resumes if the same game regains focus within this many
    /// seconds of the last focused poll (inclusive).
    pub merge_gap_s: u64,
    /// A gap between polls larger than this (or a clock going backwards) is
    /// treated as sleep/hibernate/clock jump: the open session is closed at its
    /// last focused poll.
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
    /// Last focused poll (pause start when `!focused`).
    end: u64,
    active_s: u64,
    focused: bool,
}

impl<G> Open<G> {
    /// Sessions with no accrued active time are dropped.
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

        let slept = matches!(
            self.last_poll,
            Some(lp) if now < lp || now - lp > self.params.sleep_gap_s
        );
        self.last_poll = Some(now);
        if slept {
            closed = self.close_open();
        } else if let Some(o) = &self.open {
            // Paused too long: close at pause start.
            if !o.focused && now.saturating_sub(o.end) > self.params.merge_gap_s {
                closed = self.close_open();
            }
        }

        match (&mut self.open, fg) {
            (None, Some(g)) => self.open = Some(Self::open_new(now, g)),
            (None, None) => {}
            (Some(o), Some(g)) if *g == o.game => {
                if o.focused {
                    o.active_s += now - o.end;
                }
                // On resume the gap is not counted.
                o.focused = true;
                o.end = now;
            }
            (Some(_), Some(g)) => {
                debug_assert!(closed.is_none());
                closed = self.close_open();
                self.open = Some(Self::open_new(now, g));
            }
            (Some(o), None) => o.focused = false,
        }
        closed
    }

    /// Snapshot of the open or paused session (for checkpointing), with `end` at
    /// the last focused poll. `None` if idle or nothing has been accrued yet.
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
        // 10 s before pause; resume poll at 41 not counted; 41..50 = 9 s.
        assert_eq!(t.flush(), Some(s('a', 0, 50, 19)));
    }

    #[test]
    fn merge_gap_boundary_inclusive() {
        let p = Params {
            merge_gap_s: 60,
            sleep_gap_s: 1000,
        };
        // Pause starts at 10; back at 70 (gap exactly 60): merged.
        let mut t = Tracker::new(p);
        let mut seq = run(0, 10, Some('a'));
        seq.extend(run(11, 69, None));
        seq.extend([(70, Some('a')), (71, Some('a'))]);
        assert!(feed(&mut t, &seq).is_empty());
        assert_eq!(t.flush(), Some(s('a', 0, 71, 11)));
        // Back at 71 (gap 61): separate sessions.
        let mut t = Tracker::new(p);
        let mut seq = run(0, 10, Some('a'));
        seq.extend(run(11, 70, None));
        seq.extend([(71, Some('a')), (72, Some('a'))]);
        assert_eq!(feed(&mut t, &seq), vec![s('a', 0, 10, 10)]);
        assert_eq!(t.flush(), Some(s('a', 71, 72, 1)));
    }

    #[test]
    fn alt_tab_beyond_merge_gap_closes_at_pause_start() {
        let mut t = tr();
        let mut seq = run(0, 10, Some('a'));
        seq.extend(run(11, 80, None));
        let closed = feed(&mut t, &seq);
        assert_eq!(closed, vec![s('a', 0, 10, 10)]);
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
        assert_eq!(closed, vec![s('a', 0, 10, 10)]);
        assert_eq!(t.flush(), Some(s('b', 11, 20, 9)));
    }

    #[test]
    fn switch_while_paused() {
        let mut t = tr();
        let mut seq = run(0, 10, Some('a'));
        seq.extend(run(11, 20, None));
        seq.extend(run(21, 30, Some('b')));
        let closed = feed(&mut t, &seq);
        assert_eq!(closed, vec![s('a', 0, 10, 10)]);
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
        assert_eq!(closed, vec![s('a', 0, 10, 10)]);
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
        assert_eq!(t.flush(), Some(s('a', 10, 15, 5)));
        assert_eq!(t.current(), None);
    }

    #[test]
    fn current_snapshots() {
        let mut t = tr();
        assert_eq!(t.current(), None);
        t.step(0, Some(&'a'));
        assert_eq!(t.current(), None); // nothing accrued yet
        t.step(1, Some(&'a'));
        t.step(2, Some(&'a'));
        assert_eq!(t.current(), Some(s('a', 0, 2, 2)));
        t.step(3, None);
        t.step(4, None);
        assert_eq!(t.current(), Some(s('a', 0, 2, 2))); // paused: end = last focus
        t.step(5, Some(&'a'));
        t.step(6, Some(&'a'));
        assert_eq!(t.current(), Some(s('a', 0, 6, 3)));
    }

    #[test]
    fn single_poll_focus_dropped() {
        let mut t = tr();
        let mut seq = vec![(0, Some('a'))];
        seq.extend(run(1, 100, None));
        assert!(feed(&mut t, &seq).is_empty());
        assert_eq!(t.flush(), None);

        // Single-poll game between two real stretches of another game.
        let mut seq = run(200, 210, Some('a'));
        seq.push((211, Some('b')));
        seq.extend(run(212, 220, Some('a')));
        let closed = feed(&mut t, &seq);
        assert_eq!(closed, vec![s('a', 200, 210, 10)]);
        assert_eq!(t.flush(), Some(s('a', 212, 220, 8)));
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

    #[test]
    fn property_random_foreground() {
        for seed in 0..20u64 {
            let mut rng = Lcg(seed + 1);
            let mut t = tr();
            let mut sessions = Vec::new();
            let mut prev: Option<char> = None;
            let mut truth = 0u64;
            let mut fg: Option<char> = None;
            let mut now = 1_000u64;
            let end_t = now + 20_000;
            while now < end_t {
                // Change focus with ~2% probability per poll.
                if rng.next().is_multiple_of(50) {
                    fg = match rng.next() % 4 {
                        0 => None,
                        1 => Some('a'),
                        2 => Some('b'),
                        _ => Some('c'),
                    };
                }
                if fg.is_some() && fg == prev {
                    truth += 1;
                }
                prev = fg;
                sessions.extend(t.step(now, fg.as_ref()));
                now += 1;
            }
            sessions.extend(t.flush());

            let mut last_end = 0;
            let mut total = 0;
            for x in &sessions {
                assert!(x.start <= x.end, "seed {seed}: {x:?}");
                assert!(x.active_s > 0);
                assert!(x.active_s <= x.end - x.start, "seed {seed}: {x:?}");
                assert!(x.start >= last_end, "seed {seed}: overlap at {x:?}");
                last_end = x.end;
                total += x.active_s;
            }
            // With 1 s polls and no sleep gaps, credited time equals the number
            // of poll intervals focused on the same game at both ends: tolerance 0.
            // (Versus continuous foreground time the error is at most one poll
            // per focus run, by the attribution rule in the module docs.)
            assert_eq!(total, truth, "seed {seed}");
        }
    }
}
