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
//! the window add up, and the window slides with the search.

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

/// The best size at each improvement, with the time it was reached.
pub struct Progress {
    window: f64,
    min_gain: u64,
    history: Vec<(f64, u64)>,
}

impl Progress {
    /// A tracker for windows of `window` seconds and gains of at least `min_gain` bytes.
    pub fn new(window: f64, min_gain: u64) -> Progress {
        return Progress {
            window: window,
            min_gain: min_gain,
            history: Vec::new(),
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

    /// Whether the search should stop at `now` seconds, with `best` as its best size. It
    /// stops once a full window has elapsed and the window reduced the best size by less
    /// than `min_gain`. A `min_gain` of zero disables this rule.
    pub fn should_stop(&self, now: f64, best: u64) -> bool {
        if self.min_gain == 0 || now < self.window {
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

    /// Whether the stagnation rule stops the search now, with `best` as its best size.
    pub fn stagnant(&self, best: u64) -> bool {
        return self.progress.should_stop(self.now(), best);
    }

    /// Whether the generation cap has been reached.
    pub fn generation_cap_reached(&self) -> bool {
        return self
            .generation_cap
            .map_or(false, |cap| self.generations >= cap);
    }

    /// Count one more generation.
    pub fn begin_generation(&mut self) {
        self.generations += 1;
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
}
