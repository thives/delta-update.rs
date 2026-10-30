use delta_update_payload::error::Code;
use delta_update_payload::profile::EMBEDDED;
use delta_update_payload::rsa;

use crate::assemble;
use crate::candidate_programs;
use crate::compress;
use crate::control::Control;
use crate::control::Halt;
use crate::control::Stop;
use crate::control::test_clocks::Frozen;
use crate::control::test_clocks::Ticking;
use crate::evaluate::Problem;
use crate::evaluate::evaluate;
use crate::gpu::DevicePreference;
use crate::gpu_layout::NCAND;
use crate::mapper::Mapping;
use crate::memory::Images;
use crate::program::Kind;
use crate::program::Op;
use crate::program::Program;
use crate::program::Span;
use crate::search::SearchConfig;
use crate::search::open_session;
use crate::search::run_with;

fn test_device() -> DevicePreference {
    return match std::env::var("OTA_TEST_DEVICE").as_deref() {
        Ok("auto") => DevicePreference::Auto,
        _ => DevicePreference::Software,
    };
}

const PRIV: &[u8] = include_bytes!("../../delta-update-payload/tests/fixtures/private.pem");

fn noise(len: usize, seed: u64) -> Vec<u8> {
    let mut x = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    let mut out = Vec::with_capacity(len);
    for _ in 0..len {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        out.push((x >> 24) as u8);
    }
    return out;
}

fn span(start: u64, len: u64) -> Span {
    return Span {
        start: start,
        len: len,
    };
}

fn key() -> rsa::PrivKey {
    return match rsa::parse_privkey_pem(PRIV) {
        Err(e) => panic!("{}", e.msg),
        Ok(k) => k,
    };
}

fn problem_for<'a>(old: &'a [u8], new: &'a [u8], chunk: usize) -> Problem<'a> {
    return match Problem::new(old, new, chunk, "A/B", &EMBEDDED, key().size) {
        Err(e) => panic!("{}", e.msg),
        Ok(p) => p,
    };
}

fn bounded(seed: u64, gens: u64) -> SearchConfig {
    return SearchConfig {
        max_search_seconds: 1.0e9,
        stagnation_seconds: 1.0e9,
        min_improvement_bytes: 0,
        seed: seed,
        max_generations: Some(gens),
        device: test_device(),
        readable_target: true,
        release_version: 0,
    };
}

fn flat_replay(program: &[Op], old: &[u8], new: &[u8]) -> Vec<u8> {
    let mut target = old.to_vec();
    for op in program {
        let stream: Vec<u8> = match op.kind {
            Kind::Zero => vec![0u8; op.dst.iter().map(|s| s.len as usize).sum()],
            Kind::SourceCopy => gather(&op.src, old),
            Kind::TargetCopy => gather(&op.src, &target),
            Kind::Replace | Kind::Frames | Kind::Sparse => gather(&op.dst, new),
        };
        scatter(&op.dst, &stream, &mut target);
    }
    return target;
}

fn gather(spans: &[Span], image: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    for s in spans {
        out.extend_from_slice(&image[s.start as usize..s.end() as usize]);
    }
    return out;
}

fn scatter(spans: &[Span], bytes: &[u8], target: &mut [u8]) {
    let mut at = 0usize;
    for s in spans {
        target[s.start as usize..s.end() as usize].copy_from_slice(&bytes[at..at + s.len as usize]);
        at += s.len as usize;
    }
}

fn single_ops(len: u64) -> Vec<Op> {
    let mut out = Vec::new();
    for start in 0..len {
        for n in 1..=(len - start) {
            let d = vec![span(start, n)];
            for kind in [Kind::Replace, Kind::Frames, Kind::Sparse, Kind::Zero] {
                out.push(Op::new(kind, d.clone()));
            }
            for src_start in 0..=(len - n) {
                let s = vec![span(src_start, n)];
                out.push(Op::copy(Kind::SourceCopy, d.clone(), s.clone()));
                out.push(Op::copy(Kind::TargetCopy, d.clone(), s));
            }
        }
    }
    return out;
}

fn overlaps(op: &Op) -> bool {
    if op.kind != Kind::TargetCopy {
        return false;
    }
    return op.src.iter().any(|s| {
        op.dst
            .iter()
            .any(|d| s.start < d.end() && d.start < s.end())
    });
}

#[test]
fn every_finalist_is_a_valid_payload_of_its_exact_raw_size() {
    let mut cases: Vec<(Vec<u8>, Vec<u8>, usize)> = Vec::new();
    let old: Vec<u8> = (0..73u8).map(|k| k.wrapping_mul(3)).collect();
    let mut new = old.clone();
    new[40..73].fill(0x11);
    new[5..12].copy_from_slice(&old[30..37]);
    cases.push((old.clone(), new, 16));
    let old: Vec<u8> = (0..200u32).map(|k| (k * 7 % 251) as u8).collect();
    let mut new = old.clone();
    new[13..61].fill(0);
    new[100..117].copy_from_slice(&old[3..20]);
    let carried: Vec<u8> = new[120..130].to_vec();
    new[150..160].copy_from_slice(&carried);
    cases.push((old, new, 7));
    let old = noise(37, 5);
    let mut new = old.clone();
    new[3..9].copy_from_slice(&old[20..26]);
    new[30..37].fill(0);
    cases.push((old, new, 4));

    let key = key();
    let mut total_checked = 0usize;
    for (old, new, chunk) in &cases {
        for seed in 1..=2u64 {
            let problem = problem_for(old, new, *chunk);
            let found = match run_with(&problem, &bounded(seed, 8), &Frozen, None) {
                Err(e) => panic!("{}", e.msg),
                Ok(f) => f,
            };
            assert_eq!(found.stop, Stop::Generations);
            assert!(!found.finalists.is_empty(), "the search kept no finalist");
            for f in &found.finalists {
                let evaluated = match evaluate(&problem, f.program.clone(), &Control::unbounded()) {
                    Ok(Some(e)) => e,
                    _ => panic!("the CPU refuses a finalist: {:?}", f.program),
                };
                assert_eq!(
                    evaluated.size, f.raw_size,
                    "the device's raw size of {:?}",
                    f.program
                );
                assert_eq!(
                    evaluated.program, f.program,
                    "a finalist is stored complete, with its repair"
                );
                match assemble(&problem, &f.program, &key, old, new) {
                    Err(err) => panic!("a finalist refused by the client rules: {}", err.msg),
                    Ok(payload) => assert_eq!(
                        payload.len() as u64,
                        f.raw_size,
                        "raw size must equal the emitted length"
                    ),
                }
                total_checked += 1;
            }
        }
    }
    assert!(
        total_checked >= 12,
        "only {} finalists were checked",
        total_checked
    );
}

#[test]
fn the_run_length_model_agrees_with_a_flat_replay_on_every_small_program() {
    let pairs: Vec<(Vec<u8>, Vec<u8>)> = vec![
        (vec![1, 2, 3, 4], vec![1, 9, 3, 9]),
        (vec![0, 0, 0, 0], vec![0, 5, 5, 0]),
        (vec![7, 7, 7, 7], vec![7, 7, 7, 7]),
    ];
    let key = key();
    let mut complete_programs = 0usize;
    let mut refused_programs = 0usize;
    for (old, new) in &pairs {
        let ops = single_ops(old.len() as u64);
        let mut programs: Vec<Program> = vec![Vec::new()];
        programs.extend(ops.iter().map(|o| vec![o.clone()]));
        for a in &ops {
            for b in &ops {
                programs.push(vec![a.clone(), b.clone()]);
            }
        }
        for chunk in [1usize, 2, 3] {
            let mapping = Mapping::new(Images { old, new }, chunk);
            let problem = problem_for(old, new, chunk);
            for program in &programs {
                let structurally_invalid = program.iter().any(overlaps);
                let model = mapping.map(program, &Control::unbounded(), false);
                match model {
                    Err(Halt::Failed(_)) => {
                        assert!(
                            structurally_invalid,
                            "the mapper refused a valid program: {:?}",
                            program
                        );
                        refused_programs += 1;
                        continue;
                    }
                    Err(Halt::Deadline) => panic!("an unbounded mapping cannot time out"),
                    Ok(mapped) => {
                        assert!(
                            !structurally_invalid,
                            "the mapper admitted an overlapping target copy"
                        );
                        let flat = flat_replay(program, old, new);
                        let complete = mapped.mismatches.is_empty();
                        assert_eq!(complete, flat == *new, "validity differs for {:?}", program);
                        if complete {
                            complete_programs += 1;
                            let evaluated =
                                match evaluate(&problem, program.clone(), &Control::unbounded()) {
                                    Ok(Some(e)) => e,
                                    _ => {
                                        panic!("a complete program was not admitted: {:?}", program)
                                    }
                                };
                            assert_eq!(
                                evaluated.program, *program,
                                "a complete program is not repaired"
                            );
                            if complete_programs % 97 == 0 {
                                let emitted = match assemble(&problem, program, &key, old, new) {
                                    Err(e) => panic!("{} for {:?}", e.msg, program),
                                    Ok(p) => p,
                                };
                                assert_eq!(
                                    emitted.len() as u64,
                                    evaluated.size,
                                    "exact cost of {:?}",
                                    program
                                );
                            }
                        }
                    }
                }
            }
        }
    }
    assert!(
        complete_programs > 1000,
        "only {} complete programs were replayed",
        complete_programs
    );
    assert!(
        refused_programs > 0,
        "the oracle must see refused target copies"
    );
}

fn device_ops(len: u64) -> Vec<Op> {
    let mut out = Vec::new();
    for start in 0..len {
        for n in 1..=(len - start) {
            let d = vec![span(start, n)];
            out.push(Op::new(Kind::Replace, d.clone()));
            out.push(Op::new(Kind::Zero, d.clone()));
            for src_start in 0..=(len - n) {
                let s = vec![span(src_start, n)];
                out.push(Op::copy(Kind::SourceCopy, d.clone(), s.clone()));
                out.push(Op::copy(Kind::TargetCopy, d.clone(), s));
            }
        }
    }
    return out;
}

#[test]
fn the_device_replay_agrees_with_a_flat_replay_on_every_small_program() {
    let pairs: Vec<(Vec<u8>, Vec<u8>)> = vec![
        (vec![1, 2, 3, 4], vec![1, 9, 3, 9]),
        (vec![0, 0, 0, 0], vec![0, 5, 5, 0]),
        (vec![7, 7, 7, 7], vec![7, 7, 7, 7]),
    ];
    let mut complete = 0usize;
    let mut refused = 0usize;
    for (old, new) in &pairs {
        let ops = device_ops(old.len() as u64);
        let mut programs: Vec<Program> = vec![Vec::new()];
        programs.extend(ops.iter().map(|o| vec![o.clone()]));
        for a in &ops {
            for b in &ops {
                programs.push(vec![a.clone(), b.clone()]);
            }
        }
        let problem = problem_for(old, new, 2);
        let config = SearchConfig {
            device: test_device(),
            ..SearchConfig::quick()
        };
        let mut session = match open_session(&problem, &config) {
            Err(e) => panic!("{}", e.msg),
            Ok(s) => s,
        };
        for chunk in programs.chunks(NCAND) {
            let got = match session.evaluate(chunk) {
                Err(e) => panic!("{}", e.msg),
                Ok(g) => g,
            };
            for (program, result) in chunk.iter().zip(&got) {
                let structurally_invalid = program.iter().any(overlaps);
                if structurally_invalid {
                    assert!(
                        result.size.is_none(),
                        "the device admitted an overlapping target copy: {:?}",
                        program
                    );
                    refused += 1;
                    continue;
                }
                let flat = flat_replay(program, old, new);
                let is_complete = flat == *new;
                assert_eq!(
                    result.mismatch_spans == 0,
                    is_complete,
                    "completeness differs for {:?}",
                    program
                );
                let want = match evaluate(&problem, program.clone(), &Control::unbounded()) {
                    Ok(Some(e)) => e,
                    _ => panic!("the CPU refused a valid program: {:?}", program),
                };
                assert_eq!(result.size, Some(want.size), "size of {:?}", program);
                assert_eq!(result.program, want.program, "repair of {:?}", program);
                if is_complete {
                    complete += 1;
                }
            }
        }
    }
    assert!(
        complete > 1000,
        "only {} complete programs were replayed",
        complete
    );
    assert!(refused > 0, "the oracle must see refused target copies");
}

#[test]
fn a_deadline_before_the_search_starts_leaves_the_raw_baseline_as_the_answer() {
    let old = noise(300, 11);
    let mut new = old.clone();
    new[40..90].fill(0);
    new[200..260].copy_from_slice(&old[10..70]);
    let key = key();
    let problem = problem_for(&old, &new, 16);
    let config = SearchConfig {
        max_search_seconds: 50.0,
        ..bounded(1, 1000)
    };
    let found = match run_with(&problem, &config, &Ticking::new(50.0), None) {
        Err(e) => panic!("{}", e.msg),
        Ok(f) => f,
    };
    assert_eq!(found.stop, Stop::Deadline);
    assert_eq!(found.generations, 0);
    assert!(found.device.is_none() && found.finalists.is_empty());
    let candidates = candidate_programs(&problem, &found);
    let outcome = match compress::optimize(&problem, candidates, &Control::unbounded()) {
        Ok(Some(o)) => o,
        _ => panic!("the raw baseline must be admitted"),
    };
    assert!(assemble(&problem, &outcome.best.program, &key, &old, &new).is_ok());
}

#[test]
fn deadlines_at_every_depth_of_the_search_return_valid_programs() {
    let old = noise(600, 12);
    let mut new = old.clone();
    new[100..180].fill(1);
    new[400..470].copy_from_slice(&old[50..120]);
    let key = key();
    let problem = problem_for(&old, &new, 32);
    for step in [2.0, 0.5, 0.05, 0.005, 0.0005] {
        let config = SearchConfig {
            max_search_seconds: 1.0,
            ..bounded(3, 10_000)
        };
        let found = match run_with(&problem, &config, &Ticking::new(step), Some(1)) {
            Err(e) => panic!("{}", e.msg),
            Ok(f) => f,
        };
        for f in &found.finalists {
            assert!(
                assemble(&problem, &f.program, &key, &old, &new).is_ok(),
                "step {}",
                step
            );
        }
        let candidates = candidate_programs(&problem, &found);
        let outcome = match compress::optimize(&problem, candidates, &Control::unbounded()) {
            Ok(Some(o)) => o,
            _ => panic!("step {}: no admissible candidate", step),
        };
        assert!(
            assemble(&problem, &outcome.best.program, &key, &old, &new).is_ok(),
            "step {}",
            step
        );
    }
}

#[test]
fn compression_yields_between_pieces_of_one_large_chunk() {
    let old = noise(1 << 20, 21);
    let new: Vec<u8> = old.iter().map(|b| b / 16).collect();
    let mapping = Mapping::new(
        Images {
            old: &old,
            new: &new,
        },
        1 << 20,
    );
    let mut frames = Op::new(Kind::Frames, vec![span(0, 1 << 20)]);
    frames.level = Some(6);
    let program = vec![frames];
    let clock = Ticking::new(1.0);
    let control = Control::new(&clock, 6.5, 1.0, 0, None);
    match mapping.map(&program, &control, false) {
        Err(Halt::Deadline) => {}
        other => panic!("expected a deadline, got {:?}", other.is_ok()),
    }
    assert!(
        clock.reads.get() < 64,
        "compression ran {} checks without yielding",
        clock.reads.get()
    );
}

#[test]
fn the_search_stops_for_the_reason_it_met() {
    let old = noise(200, 4);
    let mut new = old.clone();
    new[10..20].fill(5);
    let problem = problem_for(&old, &new, 8);
    let by_cap = match run_with(&problem, &bounded(1, 2), &Frozen, None) {
        Err(e) => panic!("{}", e.msg),
        Ok(f) => f,
    };
    assert_eq!(by_cap.stop, Stop::Generations);
    assert_eq!(by_cap.generations, 2);
    let stagnant = SearchConfig {
        max_search_seconds: 1.0e9,
        stagnation_seconds: 1.0,
        min_improvement_bytes: 1,
        ..bounded(2, 10_000)
    };
    let by_stagnation = match run_with(&problem, &stagnant, &Ticking::new(1.0), Some(1)) {
        Err(e) => panic!("{}", e.msg),
        Ok(f) => f,
    };
    assert!(matches!(
        by_stagnation.stop,
        Stop::Stagnation | Stop::Deadline
    ));
    let equal = problem_for(&old, &old, 8);
    let unchanged = match run_with(&equal, &bounded(1, 5), &Frozen, None) {
        Err(e) => panic!("{}", e.msg),
        Ok(f) => f,
    };
    assert_eq!(unchanged.stop, Stop::Unchanged);
}

#[test]
fn progress_records_improvements_at_their_own_times() {
    let old = noise(400, 14);
    let mut new = old.clone();
    new[50..150].fill(0);
    new[300..340].copy_from_slice(&old[0..40]);
    let problem = problem_for(&old, &new, 16);
    let found = match run_with(&problem, &bounded(4, 3), &Ticking::new(1.0), Some(1)) {
        Err(e) => panic!("{}", e.msg),
        Ok(f) => f,
    };
    let history = found.history;
    assert!(!history.is_empty());
    assert!(
        history[0].0 > 0.0,
        "the baseline is timestamped at its own time"
    );
    assert!(
        history
            .windows(2)
            .all(|w| w[0].0 < w[1].0 && w[0].1 > w[1].1),
        "{:?}",
        history
    );
    assert_eq!(
        history.last().map(|h| h.1),
        found.finalists.first().map(|f| f.raw_size)
    );
}

#[test]
fn invalid_time_settings_are_refused_before_the_search() {
    let old = vec![0u8; 16];
    let mut new = old.clone();
    new[4..8].fill(1);
    for (max, stagnation) in [
        (f64::NAN, 1.0),
        (f64::INFINITY, 1.0),
        (-1.0, 1.0),
        (1.0, 0.0),
        (1.0, f64::NAN),
    ] {
        let config = SearchConfig {
            max_search_seconds: max,
            stagnation_seconds: stagnation,
            ..bounded(1, 1)
        };
        let refused = crate::build_payload_with(&old, &new, b"A/B", PRIV, 4, &EMBEDDED, &config);
        assert_eq!(
            refused.err().map(|e| e.code),
            Some(Code::unsupported_field),
            "{} / {}",
            max,
            stagnation
        );
    }
}
