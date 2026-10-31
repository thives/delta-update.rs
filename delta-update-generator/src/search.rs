//! The search over programs: a genetic algorithm that runs entirely on the GPU.
//!
//! See `docs/generator.md`. This module is the host's side of it: the configuration, the numbers
//! the shaders score with, and the loop that submits batches of generations and decides when to
//! stop. The host does not select parents, mutate, repair or evaluate any candidate. It reads one
//! small record of counters between batches, and the finalist programs when the search ends.

use std::time::Duration;
use std::time::Instant;

use delta_update_payload::error::Code;
use delta_update_payload::error::PayloadError;
use delta_update_payload::error::err;
use delta_update_payload::layout::HEADER_SIZE;
use delta_update_payload::prescan::scan_manifest;
use delta_update_payload::profile::ResourceProfile;
use prost::Message;

use crate::control::Clock;
use crate::control::Control;
use crate::control::Halt;
use crate::control::Stop;
use crate::control::SystemClock;
use crate::evaluate::Problem;
use crate::evaluate::manifest_for;
use crate::gpu::DevicePreference;
use crate::gpu::Finalist;
use crate::gpu::Gpu;
use crate::gpu::Session;
use crate::gpu::SessionConfig;
use crate::gpu::SessionStats;
use crate::gpu::SizeModel;
use crate::gpu_layout::Counters;
use crate::gpu_layout::DEFAULT_CROSSOVER_PERCENT;
use crate::gpu_layout::DEFAULT_ELITES;
use crate::gpu_layout::DEFAULT_IMMIGRANT_PERCENT;
use crate::gpu_layout::DEFAULT_TOURNAMENT;
use crate::gpu_layout::FITNESS_INVALID;
use crate::memory::bytes_equal;

/// The stop criteria and reproducibility controls of one search.
///
/// Times are seconds. `max_search_seconds` may be zero, which returns the raw seed alone, with no
/// comparison of the images and no device. It may not be infinite: a search with no clock is
/// bounded by `max_generations`. `min_improvement_bytes` of zero disables the stagnation rule, so
/// only the clock or the generation cap stops such a search. Progress is measured on the raw,
/// uncompressed size the device scores, not on the final compressed payload.
#[derive(Clone, Debug, PartialEq)]
pub struct SearchConfig {
    /// Longest elapsed time the search may run, from its entry.
    pub max_search_seconds: f64,
    /// Length of the window in which progress is measured. Must be positive and finite.
    pub stagnation_seconds: f64,
    /// Smallest raw size reduction that counts as progress within a full window. Zero disables
    /// the stagnation rule.
    pub min_improvement_bytes: u64,
    /// The seed of the search's random choices.
    pub seed: u64,
    /// Stop after this many generations, whatever the clock says. A generation cap
    /// makes a run reproducible from its seed alone.
    pub max_generations: Option<u64>,
    /// The Vulkan device the search runs on.
    pub device: DevicePreference,
    /// Whether the device can read its target. A target copy reads the target, and the client
    /// refuses one on write-only storage, so `false` makes the search propose no target copies.
    pub readable_target: bool,
    /// The signed release version of the payload. The client refuses a release below the
    /// device's floor, so this is the generator's half of the rollback check.
    pub release_version: u64,
}

impl SearchConfig {
    /// A short budget for tests and small images: at most three seconds, stopping after half a
    /// second without a one-byte gain. The payload it produces is verified like any other, but
    /// it is not a search to convergence, so its size is not a benchmark.
    pub fn quick() -> SearchConfig {
        return SearchConfig {
            max_search_seconds: 3.0,
            stagnation_seconds: 0.5,
            min_improvement_bytes: 1,
            seed: 1,
            max_generations: None,
            device: DevicePreference::Auto,
            readable_target: true,
            release_version: 0,
        };
    }

    /// Refuse settings the search cannot use: a clock that is NaN, infinite or negative, and a
    /// stagnation window that is not a positive finite time.
    pub fn validate(&self) -> Result<(), PayloadError> {
        if !self.max_search_seconds.is_finite() || self.max_search_seconds < 0.0 {
            return Err(err(
                Code::unsupported_field,
                "max_search_seconds must be a finite, non-negative number of seconds",
            ));
        }
        if !self.stagnation_seconds.is_finite() || self.stagnation_seconds <= 0.0 {
            return Err(err(
                Code::unsupported_field,
                "stagnation_seconds must be a finite, positive number of seconds",
            ));
        }
        return Ok(());
    }
}

impl Default for SearchConfig {
    fn default() -> SearchConfig {
        return SearchConfig {
            max_search_seconds: 30.0,
            stagnation_seconds: 5.0,
            min_improvement_bytes: 1,
            seed: 1,
            max_generations: None,
            device: DevicePreference::Auto,
            readable_target: true,
            release_version: 0,
        };
    }
}

fn word(n: usize) -> u32 {
    return u32::try_from(n).unwrap_or(u32::MAX);
}

/// The numbers the shaders score a program with, computed with the real encoder and the profile's
/// own scanner, so the device's raw size is the size of the payload the mapper would build.
///
/// The fixed parts are the encoded sizes of a partition and a manifest with no operations. The
/// decode charge is what the profile's scanner models for that empty manifest, to which the shaders
/// add each operation. The scan is made against unbounded manifest and decode budgets, because the
/// limits are the shaders' to enforce, program by program, and a profile that refuses even the
/// empty manifest is reported by the search, not here.
pub fn size_model(problem: &Problem) -> Result<SizeModel, PayloadError> {
    let empty = manifest_for(problem, Vec::new(), 0);
    let partition_fixed = match empty.partitions.first() {
        None => return Err(err(Code::device_io, "the empty manifest has no partition")),
        Some(p) => p.encoded_len(),
    };
    let mut without = empty.clone();
    without.partitions = Vec::new();
    without.signatures_offset = None;
    let manifest_fixed = without.encoded_len();
    let unbounded = ResourceProfile {
        max_decoded_bytes: usize::MAX,
        max_encoded_manifest: usize::MAX,
        max_partitions: std::cmp::max(problem.profile.max_partitions, 1),
        ..*problem.profile
    };
    let usage = match scan_manifest(&empty.encode_to_vec(), &unbounded) {
        Err(e) => return Err(e),
        Ok(u) => u,
    };
    return Ok(SizeModel {
        signatures: word(2 * problem.sig_msg_size),
        partition_fixed: word(partition_fixed),
        manifest_fixed: word(manifest_fixed),
        header: word(HEADER_SIZE),
        max_manifest: word(problem.profile.max_encoded_manifest),
        decode_slots: word(usage.slot_bytes),
        decode_heap: word(usage.heap_bytes),
        decode_transient: word(usage.transient_bytes),
        max_decoded: word(problem.profile.max_decoded_bytes),
    });
}

/// The session configuration for `problem` under `config`: the profile's count limits, which are
/// exact on the device, and the size model.
pub fn session_config(
    problem: &Problem,
    config: &SearchConfig,
) -> Result<SessionConfig, PayloadError> {
    let model = match size_model(problem) {
        Err(e) => return Err(e),
        Ok(m) => m,
    };
    return Ok(SessionConfig {
        seed: config.seed,
        readable_target: config.readable_target,
        max_operations: word(problem.profile.max_operations),
        max_extents_per_op: word(problem.profile.max_extents_per_op),
        max_total_extents: word(problem.profile.max_total_extents),
        model: model,
        tournament: DEFAULT_TOURNAMENT as u32,
        immigrant_percent: DEFAULT_IMMIGRANT_PERCENT as u32,
        crossover_percent: DEFAULT_CROSSOVER_PERCENT as u32,
        elites: DEFAULT_ELITES as u32,
    });
}

/// Open the device `config` selects and a session on it for `problem`: both images are uploaded
/// once, and the search parameters are written. No generation has run.
pub fn open_session(problem: &Problem, config: &SearchConfig) -> Result<Session, PayloadError> {
    let session = match session_config(problem, config) {
        Err(e) => return Err(e),
        Ok(c) => c,
    };
    let gpu = match Gpu::open(config.device) {
        Err(e) => return Err(e),
        Ok(g) => g,
    };
    let images = problem.mapping.images();
    return Session::open(gpu, images.old, images.new, session);
}

const BATCH_TARGET: Duration = Duration::from_millis(60);
const BATCH_MAX: u32 = 64;
const MAX_STAGNATION_RESTARTS: u32 = 3;

/// What the device search returns: the finalist programs, why it stopped, and what it cost.
#[derive(Clone, Debug)]
pub struct Found {
    /// The best programs the device admitted, one per distinct raw-written set, best raw size
    /// first. Empty when the budget allowed no device search, or the images are equal.
    pub finalists: Vec<Finalist>,
    pub stop: Stop,
    /// Generations the device ran after seeding.
    pub generations: u64,
    /// Population restarts after a stagnant window.
    pub restarts: u32,
    /// The device's counters at the end: candidates evaluated, and why others were refused.
    pub counters: Counters,
    /// `(seconds, raw size)` at each improvement of the best raw size the host saw.
    pub history: Vec<(f64, u64)>,
    /// The device's memory and transfer figures. Zero when no device was opened.
    pub stats: SessionStats,
    /// The name of the device that searched, if one did.
    pub device: Option<String>,
    /// Time from entering the search until the images were resident and the first population
    /// was evaluated, and time spent in generations after that.
    pub upload_time: Duration,
    pub search_time: Duration,
}

impl Found {
    fn seed_only(stop: Stop) -> Found {
        return Found {
            finalists: Vec::new(),
            stop: stop,
            generations: 0,
            restarts: 0,
            counters: Counters::default(),
            history: Vec::new(),
            stats: SessionStats::default(),
            device: None,
            upload_time: Duration::ZERO,
            search_time: Duration::ZERO,
        };
    }
}

/// Search for the smallest raw programs, timed by the system clock from this call.
pub fn run(problem: &Problem, config: &SearchConfig) -> Result<Found, PayloadError> {
    let clock = SystemClock::started_now();
    return run_with(problem, config, &clock, None);
}

/// [`run`] on `clock`, which is how tests control the time the search sees. `batch` fixes the
/// generations per submission; `None` sizes batches by their measured time. Every random choice
/// is a function of the seed and the generation, so the batch size changes timing and nothing else.
pub fn run_with(
    problem: &Problem,
    config: &SearchConfig,
    clock: &dyn Clock,
    batch: Option<u32>,
) -> Result<Found, PayloadError> {
    config.validate()?;
    let mut ctl = Control::new(
        clock,
        config.max_search_seconds,
        config.stagnation_seconds,
        config.min_improvement_bytes,
        config.max_generations,
    );
    if ctl.checkpoint().is_err() {
        return Ok(Found::seed_only(Stop::Deadline));
    }
    match bytes_equal(
        problem.mapping.images().old,
        problem.mapping.images().new,
        &ctl,
    ) {
        Ok(true) => return Ok(Found::seed_only(Stop::Unchanged)),
        Ok(false) => {}
        Err(Halt::Deadline) => return Ok(Found::seed_only(Stop::Deadline)),
        Err(Halt::Failed(e)) => return Err(e),
    }
    let opened = Instant::now();
    let mut session = match open_session(problem, config) {
        Err(e) => return Err(e),
        Ok(s) => s,
    };
    let mut counters = match session.prepare() {
        Err(e) => return Err(e),
        Ok(c) => c,
    };
    let upload_time = opened.elapsed();
    let searching = Instant::now();
    let record = |ctl: &mut Control, c: &Counters| {
        if c.best != FITNESS_INVALID {
            ctl.record(c.best as u64);
        }
    };
    record(&mut ctl, &counters);
    let mut per_generation = Duration::ZERO;
    let mut stagnation_restarts = 0u32;
    let mut restarts = 0u32;
    let stop = loop {
        if ctl.generation_cap_reached() {
            break Stop::Generations;
        }
        if ctl.checkpoint().is_err() {
            break Stop::Deadline;
        }
        if counters.best != FITNESS_INVALID && ctl.stagnant(counters.best as u64) {
            if stagnation_restarts >= MAX_STAGNATION_RESTARTS {
                break Stop::Stagnation;
            }
            let previous_best = counters.best;
            counters = match session.restart_from_archive() {
                Err(e) => return Err(e),
                Ok(c) => c,
            };
            if counters.best < previous_best {
                stagnation_restarts = 0;
            } else {
                stagnation_restarts += 1;
            }
            restarts += 1;
            // The restart opens a window, and its population must run a generation before the search
            // can call it stagnant again, whatever the clock says about the window.
            ctl.reset_stagnation(counters.best as u64);
            per_generation = Duration::ZERO;
            continue;
        }
        let mut n = match batch {
            Some(n) => std::cmp::max(n, 1),
            None if per_generation.is_zero() => 1,
            None => ((BATCH_TARGET.as_secs_f64() / per_generation.as_secs_f64()) as u32)
                .clamp(1, BATCH_MAX),
        };
        if let Some(cap) = config.max_generations {
            n = std::cmp::min(n as u64, cap - ctl.generations()) as u32;
        }
        let previous_best = counters.best;
        let started = Instant::now();
        counters = match session.run_generations(n) {
            Err(e) => return Err(e),
            Ok(c) => c,
        };
        let took = started.elapsed() / n;
        per_generation = if per_generation.is_zero() {
            took
        } else {
            (per_generation + took) / 2
        };
        for _ in 0..n {
            ctl.begin_generation();
        }
        if counters.best < previous_best {
            stagnation_restarts = 0;
        }
        record(&mut ctl, &counters);
    };
    let search_time = searching.elapsed();
    let finalists = match session.finalists() {
        Err(e) => return Err(e),
        Ok(f) => f,
    };
    return Ok(Found {
        finalists: finalists,
        stop: stop,
        generations: ctl.generations(),
        restarts: restarts,
        counters: counters,
        history: ctl.history().to_vec(),
        stats: session.stats(),
        device: Some(session.device_name().to_string()),
        upload_time: upload_time,
        search_time: search_time,
    });
}
