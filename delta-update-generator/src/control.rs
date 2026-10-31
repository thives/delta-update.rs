//! The search's control: one object that owns the clock, the deadline, the generation cap,
//! the rolling progress history, and the reason the search stopped.
//!
//! Every bounded step of the search asks the control whether it may go on
//! ([`Control::checkpoint`]). A step that the deadline cuts short returns [`Halt::Deadline`]
//! and no result, so an interrupted scan, mapping or compression can never be mistaken for a
//! finished one. The search then returns the last incumbent it fully evaluated.
//!
//! The clock is a trait, so tests can drive the deadline and the progress history with a
//! clock they control, and check the search's timing without waiting for it.
//!
//! **Progress.** [`Progress`] keeps the best size at each improvement, with the time of that
//! improvement. A stagnation check looks back over a full window: several small gains inside
//! the window add up, and the window slides with the search. A population restart opens a new
//! window at its own time, so the search gets a full window to make progress after it.

use std::time::Instant;

use delta_update_payload::error::PayloadError;

/// Seconds since the start of a search. The search reads time only through this trait.
pub trait Clock {
    fn seconds(&self) -> f64;
}

/// The wall clock, read from the instant the search started.
pub struct SystemClock {
    started: Instant,
}

impl SystemClock {
    /// A clock whose zero is now.
    pub fn started_now() -> SystemClock {
        return SystemClock {
            started: Instant::now(),
        };
    }
}

impl Clock for SystemClock {
    fn seconds(&self) -> f64 {
        return self.started.elapsed().as_secs_f64();
    }
}

struct NoDeadline;

impl Clock for NoDeadline {
    fn seconds(&self) -> f64 {
        return 0.0;
    }
}

static NO_DEADLINE: NoDeadline = NoDeadline;

/// Why the search stopped.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stop {
    /// The images are equal, so the empty program is the answer and nothing was searched.
    Unchanged,
    /// The clock reached `max_search_seconds`.
    Deadline,
    /// A full stagnation window passed without enough progress.
    Stagnation,
    /// `max_generations` generations ran.
    Generations,
}

/// A bounded step that did not finish.
#[derive(Debug)]
pub enum Halt {
    /// The deadline passed before the step finished. Its partial work is discarded.
    Deadline,
    /// The step failed for a reason of its own.
    Failed(PayloadError),
}

impl From<PayloadError> for Halt {
    fn from(e: PayloadError) -> Halt {
        return Halt::Failed(e);
    }
}

/// The result of a bounded step: its value, or why it did not produce one.
pub type Step<T> = Result<T, Halt>;

/// The best size at each improvement, with the time it was reached, and the time of the last restart.
pub struct Progress {
    window: f64,
    min_gain: u64,
    history: Vec<(f64, u64)>,
    restarted: Option<f64>,
}

impl Progress {
    /// A tracker for windows of `window` seconds and gains of at least `min_gain` bytes.
    pub fn new(window: f64, min_gain: u64) -> Progress {
        return Progress {
            window: window,
            min_gain: min_gain,
            history: Vec::new(),
            restarted: None,
        };
    }

    /// Record the best size at `at` seconds. Only an improvement is kept.
    pub fn record(&mut self, at: f64, size: u64) {
        let improved = match self.history.last() {
            None => true,
            Some((_, best)) => size < *best,
        };
        if improved {
            self.history.push((at, size));
        }
    }

    /// The best size as of `at` seconds, if anything was recorded by then.
    pub fn best_at(&self, at: f64) -> Option<u64> {
        for (t, size) in self.history.iter().rev() {
            if *t <= at {
                return Some(*size);
            }
        }
        return None;
    }

    /// The improvements recorded so far, in time order.
    pub fn history(&self) -> &[(f64, u64)] {
        return &self.history;
    }

    /// Open a stagnation window at `at` seconds, after a restart whose best size is `best`. The
    /// restart's best is recorded like any other improvement, so the history keeps it.
    pub fn restart(&mut self, at: f64, best: u64) {
        self.restarted = Some(at);
        self.record(at, best);
    }

    /// Whether the search should stop at `now` seconds, with `best` as its best size. It
    /// stops once a full window has elapsed and the window reduced the best size by less
    /// than `min_gain`. A `min_gain` of zero disables this rule.
    ///
    /// A restart opens a window of its own: nothing before the last restart counts, so no check
    /// can stop the search until a full window has elapsed since that restart.
    pub fn should_stop(&self, now: f64, best: u64) -> bool {
        if self.min_gain == 0 {
            return false;
        }
        let opened = self.restarted.unwrap_or(0.0);
        if now < opened + self.window {
            return false;
        }
        let base = match self.best_at(now - self.window) {
            None => return false,
            Some(size) => size,
        };
        return base.saturating_sub(best) < self.min_gain;
    }
}

/// The search's clock, deadline, generation cap, progress history and stop reason.
pub struct Control<'a> {
    clock: &'a dyn Clock,
    deadline: f64,
    generation_cap: Option<u64>,
    generations: u64,
    progress: Progress,
    /// Set by a population restart, and cleared once a generation has run on the restarted population.
    restart_unrun: bool,
}

impl<'a> Control<'a> {
    /// A control that stops at `max_seconds` on `clock`, after `generation_cap` generations,
    /// and that measures progress over `window` seconds with gains of `min_gain` bytes.
    pub fn new(
        clock: &'a dyn Clock,
        max_seconds: f64,
        window: f64,
        min_gain: u64,
        generation_cap: Option<u64>,
    ) -> Control<'a> {
        return Control {
            clock: clock,
            deadline: max_seconds,
            generation_cap: generation_cap,
            generations: 0,
            progress: Progress::new(window, min_gain),
            restart_unrun: false,
        };
    }

    /// A control with no deadline, for the steps that must finish: the raw baseline and the
    /// final assembly of the chosen program.
    pub fn unbounded() -> Control<'static> {
        return Control::new(&NO_DEADLINE, f64::INFINITY, 0.0, 0, None);
    }

    /// The time now, in seconds from the start of the search.
    pub fn now(&self) -> f64 {
        return self.clock.seconds();
    }

    /// `Err(Halt::Deadline)` once the deadline has passed. Bounded steps call this between
    /// their units of work.
    pub fn checkpoint(&self) -> Step<()> {
        if self.clock.seconds() >= self.deadline {
            return Err(Halt::Deadline);
        }
        return Ok(());
    }

    /// Record an accepted size at the current time.
    pub fn record(&mut self, size: u64) {
        let now = self.now();
        self.progress.record(now, size);
    }

    /// Open a new stagnation window at the current time, after a population restart whose best size
    /// is `best`. The improvement history is kept, and the restart's best joins it if it improved.
    /// The restarted population must then run a generation before the stagnation rule can stop the
    /// search (see [`Control::stagnant`]).
    pub fn reset_stagnation(&mut self, best: u64) {
        let now = self.now();
        self.progress.restart(now, best);
        self.restart_unrun = true;
    }

    /// Whether the stagnation rule stops the search now, with `best` as its best size. It does not
    /// while a restarted population has yet to run a generation, whatever the clock says: a window
    /// that expires between two reads of the clock still gives the restart its generation. The
    /// deadline and the generation cap are checked separately, and are not deferred by this.
    pub fn stagnant(&self, best: u64) -> bool {
        return !self.restart_unrun && self.progress.should_stop(self.now(), best);
    }

    /// Whether the generation cap has been reached.
    pub fn generation_cap_reached(&self) -> bool {
        return self
            .generation_cap
            .map_or(false, |cap| self.generations >= cap);
    }

    /// Count one more generation. It is the first generation after a restart that lets the stagnation
    /// rule apply to the restarted population again.
    pub fn begin_generation(&mut self) {
        self.generations += 1;
        self.restart_unrun = false;
    }

    /// Generations begun so far.
    pub fn generations(&self) -> u64 {
        return self.generations;
    }

    /// The improvements recorded so far.
    pub fn history(&self) -> &[(f64, u64)] {
        return self.progress.history();
    }
}

#[cfg(test)]
pub(crate) mod test_clocks {
    use std::cell::Cell;

    use super::Clock;

    pub struct Frozen;

    impl Clock for Frozen {
        fn seconds(&self) -> f64 {
            return 0.0;
        }
    }

    pub struct Ticking {
        pub reads: Cell<u64>,
        step: f64,
    }

    impl Ticking {
        pub fn new(step: f64) -> Ticking {
            return Ticking {
                reads: Cell::new(0),
                step: step,
            };
        }
    }

    impl Clock for Ticking {
        fn seconds(&self) -> f64 {
            let r = self.reads.get() + 1;
            self.reads.set(r);
            return r as f64 * self.step;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    struct Manual(Cell<f64>);

    impl Clock for Manual {
        fn seconds(&self) -> f64 {
            return self.0.get();
        }
    }

    #[test]
    fn progress_counts_small_gains_that_add_up_in_a_window() {
        let mut p = Progress::new(10.0, 25);
        p.record(0.0, 1000);
        p.record(4.0, 990);
        p.record(7.0, 980);
        p.record(9.0, 970);
        assert!(!p.should_stop(10.0, 970), "window 0..10 gained 30 bytes");
        assert!(p.should_stop(14.0, 970), "window 4..14 gained 20 bytes");
    }

    #[test]
    fn progress_does_not_stop_before_a_full_window_has_elapsed() {
        let mut p = Progress::new(10.0, 1);
        p.record(0.0, 500);
        assert!(!p.should_stop(9.9, 500));
        assert!(p.should_stop(10.0, 500));
    }

    #[test]
    fn progress_records_only_improvements() {
        let mut p = Progress::new(5.0, 1);
        p.record(0.0, 100);
        p.record(1.0, 100);
        p.record(2.0, 120);
        p.record(3.0, 90);
        assert_eq!(p.history(), &[(0.0, 100), (3.0, 90)]);
        assert_eq!(p.best_at(2.5), Some(100));
        assert_eq!(p.best_at(3.0), Some(90));
    }

    #[test]
    fn a_zero_minimum_gain_never_stops_on_stagnation() {
        let mut p = Progress::new(1.0, 0);
        p.record(0.0, 10);
        assert!(!p.should_stop(100.0, 10));
    }

    #[test]
    fn the_deadline_halts_a_checkpoint_at_exactly_the_limit() {
        let clock = Manual(Cell::new(0.0));
        let control = Control::new(&clock, 2.0, 1.0, 1, None);
        assert!(control.checkpoint().is_ok());
        clock.0.set(2.0);
        assert!(matches!(control.checkpoint(), Err(Halt::Deadline)));
    }

    #[test]
    fn an_unbounded_control_never_halts() {
        let control = Control::unbounded();
        assert!(control.checkpoint().is_ok());
        assert!(!control.stagnant(0));
    }

    #[test]
    fn improvements_are_timestamped_when_they_are_recorded() {
        let clock = Manual(Cell::new(0.0));
        let mut control = Control::new(&clock, 100.0, 50.0, 1, None);
        clock.0.set(0.7);
        control.record(900);
        clock.0.set(1.9);
        control.record(850);
        assert_eq!(control.history(), &[(0.7, 900), (1.9, 850)]);
    }

    #[test]
    fn a_restart_is_not_stagnant_until_a_full_window_has_elapsed_since_it() {
        let clock = Manual(Cell::new(0.0));
        let mut control = Control::new(&clock, 1000.0, 10.0, 1, None);
        control.record(500);
        clock.0.set(50.0);
        control.reset_stagnation(500);
        control.begin_generation();
        assert!(!control.stagnant(500), "checked at the restart itself");
        clock.0.set(55.0);
        assert!(
            !control.stagnant(500),
            "checked half a window after the restart, when the stale window [45, 55] had no gain"
        );
        clock.0.set(59.9);
        assert!(!control.stagnant(500), "checked just before the window expires");
        clock.0.set(60.0);
        assert!(control.stagnant(500), "a full window with no gain since the restart");
    }

    #[test]
    fn each_restart_opens_its_own_window() {
        let clock = Manual(Cell::new(0.0));
        let mut control = Control::new(&clock, 1000.0, 10.0, 1, None);
        control.record(500);
        clock.0.set(50.0);
        control.reset_stagnation(500);
        control.begin_generation();
        clock.0.set(70.0);
        control.reset_stagnation(500);
        control.begin_generation();
        clock.0.set(79.9);
        assert!(!control.stagnant(500), "the second restart's window is still open");
        clock.0.set(80.0);
        assert!(control.stagnant(500));
    }

    #[test]
    fn a_window_after_a_restart_slides_with_the_gains_made_after_it() {
        let clock = Manual(Cell::new(0.0));
        let mut control = Control::new(&clock, 1000.0, 10.0, 1, None);
        control.record(500);
        clock.0.set(50.0);
        control.reset_stagnation(500);
        control.begin_generation();
        clock.0.set(63.0);
        control.record(490);
        clock.0.set(70.0);
        assert!(
            !control.stagnant(490),
            "[60, 70] holds the gain of 10 bytes made at 63"
        );
        clock.0.set(73.0);
        assert!(
            control.stagnant(490),
            "[63, 73] holds no gain, so the window slid past the improvement"
        );
    }

    #[test]
    fn a_restarted_population_must_run_a_generation_before_it_can_be_stagnant() {
        let clock = Manual(Cell::new(0.0));
        let mut control = Control::new(&clock, 1000.0, 10.0, 1, None);
        control.record(500);
        clock.0.set(50.0);
        control.reset_stagnation(500);
        clock.0.set(60.0);
        assert!(
            !control.stagnant(500),
            "a full window has elapsed since the restart, but no generation has run on it"
        );
        control.begin_generation();
        assert!(control.stagnant(500), "a generation has run and the window has elapsed");
    }

    #[test]
    fn a_restart_keeps_the_improvement_history_and_records_its_own_best() {
        let clock = Manual(Cell::new(0.0));
        let mut control = Control::new(&clock, 1000.0, 10.0, 1, None);
        control.record(500);
        clock.0.set(50.0);
        control.reset_stagnation(480);
        assert_eq!(control.history(), &[(0.0, 500), (50.0, 480)]);
        clock.0.set(60.0);
        control.reset_stagnation(480);
        assert_eq!(
            control.history(),
            &[(0.0, 500), (50.0, 480)],
            "a restart without an improvement adds nothing to the history"
        );
    }
}
