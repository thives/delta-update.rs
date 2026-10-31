mod support;

use std::time::Duration;
use std::time::Instant;

use delta_update_generator::SearchConfig;
use delta_update_generator::evaluate::Problem;
use delta_update_generator::search::open_session;
use delta_update_payload::profile::HOST;

fn noise(len: usize, seed: u64) -> Vec<u8> {
    let mut x = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    return (0..len)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            (x >> 24) as u8
        })
        .collect();
}

fn images() -> (Vec<u8>, Vec<u8>) {
    if let (Ok(a), Ok(b)) = (
        std::env::var("OTA_BENCH_OLD"),
        std::env::var("OTA_BENCH_NEW"),
    ) {
        return (std::fs::read(a).unwrap(), std::fs::read(b).unwrap());
    }
    let old = noise(4 << 20, 1);
    let mut new = old.clone();
    new[1 << 20..(1 << 20) + 300_000].copy_from_slice(&old[3 << 20..(3 << 20) + 300_000]);
    new[2 << 20..(2 << 20) + 50_000].fill(0);
    return (old, new);
}

fn open(old: &[u8], new: &[u8]) -> (delta_update_generator::gpu::Session, Duration) {
    let problem =
        Problem::new(old, new, 4096, "A/B", &HOST, 256).unwrap_or_else(|e| panic!("{}", e.msg));
    let config = SearchConfig {
        device: support::device(),
        ..SearchConfig::quick()
    };
    let started = Instant::now();
    let session = open_session(&problem, &config).unwrap_or_else(|e| panic!("{}", e.msg));
    return (session, started.elapsed());
}

#[test]
#[ignore]
fn per_stage_times_of_one_generation() {
    let (old, new) = images();
    let (mut session, upload) = open(&old, &new);
    let prepared = Instant::now();
    let counters = session.prepare().unwrap_or_else(|e| panic!("{}", e.msg));
    eprintln!("device {}", session.device_name());
    eprintln!("images {} + {} bytes", old.len(), new.len());
    eprintln!(
        "open and upload {:.3} s, tables + first population {:.3} s",
        upload.as_secs_f64(),
        prepared.elapsed().as_secs_f64()
    );
    eprintln!(
        "after seeding: best raw {} members {} refused {:?}",
        counters.best, counters.members, counters.refused
    );
    let warmup: u32 = std::env::var("OTA_BENCH_WARMUP")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    if warmup > 0 {
        let started = Instant::now();
        let counters = session
            .run_generations(warmup)
            .unwrap_or_else(|e| panic!("{}", e.msg));
        eprintln!(
            "warm-up of {} generations took {:.2} s; best raw {}",
            warmup,
            started.elapsed().as_secs_f64(),
            counters.best
        );
    }
    let rounds = 12u32;
    let mut totals: Vec<(&'static str, f64, f64)> = Vec::new();
    for _ in 0..rounds {
        let times = session
            .profile_generation()
            .unwrap_or_else(|e| panic!("{}", e.msg));
        for (k, (name, took)) in times.into_iter().enumerate() {
            let ms = took.as_secs_f64() * 1000.0;
            if totals.len() <= k {
                totals.push((name, 0.0, 0.0));
            }
            totals[k].1 += ms;
            totals[k].2 = totals[k].2.max(ms);
        }
    }
    for (name, total, worst) in &totals {
        eprintln!(
            "  {:8} mean {:8.2} ms  worst {:8.2} ms",
            name,
            total / rounds as f64,
            worst
        );
    }
    let generations = 20;
    let started = Instant::now();
    let counters = session
        .run_generations(generations)
        .unwrap_or_else(|e| panic!("{}", e.msg));
    let took = started.elapsed().as_secs_f64();
    eprintln!(
        "{} generations in {:.3} s: {:.1} generations/s, {:.0} candidates/s; best raw {}",
        generations,
        took,
        generations as f64 / took,
        generations as f64 * delta_update_generator::gpu_layout::CHILDREN as f64 / took,
        counters.best
    );
    let population = session.population().unwrap_or_else(|e| panic!("{}", e.msg));
    let shape: Vec<(u64, usize, usize)> = population
        .iter()
        .map(|f| {
            (
                f.raw_size,
                f.program.len(),
                f.program.iter().map(|o| o.dst.len() + o.src.len()).sum(),
            )
        })
        .collect();
    eprintln!("population (raw size, operations, extents): {:?}", shape);
    let stats = session.stats();
    eprintln!(
        "device memory {} bytes, host memory {} bytes, uploaded {} bytes, read back {} bytes",
        stats.device_bytes, stats.host_bytes, stats.uploaded, stats.read_back
    );
}

fn dead_data_bytes(program: &[delta_update_generator::program::Op]) -> (u64, u64) {
    let mut data = 0u64;
    let mut dead = 0u64;
    for (index, op) in program.iter().enumerate() {
        if !op.kind.carries_data() {
            continue;
        }
        for span in &op.dst {
            data += span.len;
            let mut overlaps: Vec<(u64, u64)> = program[index + 1..]
                .iter()
                .flat_map(|later| later.dst.iter())
                .filter_map(|later| {
                    let start = span.start.max(later.start);
                    let end = span.end().min(later.end());
                    (start < end).then_some((start, end))
                })
                .collect();
            overlaps.sort_unstable();
            let mut covered: u64 = 0;
            let mut current: Option<(u64, u64)> = None;
            for (start, end) in overlaps {
                match current {
                    Some((a, b)) if start <= b => current = Some((a, b.max(end))),
                    Some((a, b)) => {
                        covered += b - a;
                        current = Some((start, end));
                    }
                    None => current = Some((start, end)),
                }
            }
            if let Some((a, b)) = current {
                covered += b - a;
            }
            dead += covered;
        }
    }
    (data, dead)
}

#[test]
#[ignore]
fn mutation_stats() {
    let (old, new) = images();
    let (mut session, _) = open(&old, &new);
    session.prepare().unwrap_or_else(|e| panic!("{}", e.msg));
    eprintln!(
        "device {} images {} + {}",
        session.device_name(),
        old.len(),
        new.len()
    );
    for batch in [20, 80] {
        let counters = session
            .run_generations(batch)
            .unwrap_or_else(|e| panic!("{}", e.msg));
        eprintln!("generation {} best {}", counters.generations, counters.best);
        for (id, stats) in counters.moves.iter().enumerate() {
            eprintln!(
                "move {} proposed {} skipped {} admitted {} new_best {}",
                id, stats.proposed, stats.skipped, stats.admitted, stats.new_best
            );
        }
    }
}

#[test]
#[ignore]
fn operator_yield_and_repair_inheritance() {
    use delta_update_generator::gpu_layout::CHILDREN;

    let (old, new) = images();
    let (mut session, _) = open(&old, &new);
    session.prepare().unwrap_or_else(|e| panic!("{}", e.msg));
    let warmup: u32 = std::env::var("OTA_BENCH_GENERATIONS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(20);
    session
        .run_generations(warmup)
        .unwrap_or_else(|e| panic!("{}", e.msg));
    let converged = session.population().unwrap_or_else(|e| panic!("{}", e.msg));
    let parents: Vec<Vec<_>> = converged.iter().map(|f| f.program.clone()).collect();
    let _ = session
        .load_population(&parents)
        .unwrap_or_else(|e| panic!("{}", e.msg));
    let population = session.population().unwrap_or_else(|e| panic!("{}", e.msg));
    let worst_kept = population.iter().map(|f| f.raw_size).max().unwrap_or(0);
    let assessed = session
        .evaluate(&parents)
        .unwrap_or_else(|e| panic!("{}", e.msg));
    let repair_candidates = population
        .iter()
        .filter(|member| {
            member
                .program
                .last()
                .is_some_and(|op| op.kind == delta_update_generator::program::Kind::Replace)
        })
        .count();
    let repaired = assessed.iter().filter(|e| e.mismatch_spans > 0).count();
    let (data_bytes, dead_bytes) = assessed.iter().fold((0u64, 0u64), |(data, dead), e| {
        let (member_data, member_dead) = dead_data_bytes(&e.program);
        (data + member_data, dead + member_dead)
    });
    eprintln!(
        "device {} images {} + {} generations {}",
        session.device_name(),
        old.len(),
        new.len(),
        warmup
    );
    eprintln!(
        "reloaded population {} worst_kept {} trailing_replace_repair_candidates {} newly_repaired_on_evaluation {} dead_raw_data_bytes {}/{} ({:.3})",
        population.len(),
        worst_kept,
        repair_candidates,
        repaired,
        dead_bytes,
        data_bytes,
        dead_bytes as f64 / data_bytes.max(1) as f64
    );

    for (label, how, mv) in std::iter::once(("immigrant", 1u32, 0u32))
        .chain(std::iter::once(("crossover", 2u32, 0u32)))
        .chain((1..=16).map(|mv| ("mutation", 3u32, mv)))
    {
        let mut totals = [0u64; 3];
        let mut refusals = std::collections::BTreeMap::<String, u64>::new();
        let mut valid = 0u64;
        let mut improvements = 0u64;
        for generation in 1..=12 {
            // evaluate() replaces candidate slots and clears the population control words.
            let loaded = session
                .load_population(&parents)
                .unwrap_or_else(|e| panic!("{}", e.msg));
            assert!(
                loaded.members > 0,
                "forced breeding requires a populated session"
            );
            let proposals = session
                .breed_only(generation, how, mv)
                .unwrap_or_else(|e| panic!("{}", e.msg));
            let skipped = proposals.iter().filter(|p| p.is_none()).count() as u64;
            let programs: Vec<Vec<_>> = proposals.into_iter().flatten().collect();
            let evaluations = session
                .evaluate(&programs)
                .unwrap_or_else(|e| panic!("{}", e.msg));
            for result in evaluations {
                if let Some(size) = result.size {
                    valid += 1;
                    if size < worst_kept {
                        improvements += 1;
                    }
                } else {
                    let name = delta_update_generator::gpu_layout::STATUS_NAMES
                        .get(result.status as usize)
                        .copied()
                        .unwrap_or("unknown")
                        .to_string();
                    *refusals.entry(name).or_default() += 1;
                }
            }
            totals[0] += CHILDREN as u64;
            totals[1] += skipped;
            totals[2] += programs.len() as u64;
        }
        eprintln!(
            "operator {} mv {} proposed {} skipped {} evaluated {} valid {} refusals {:?} beat_worst {} ({:.3})",
            label,
            mv,
            totals[0],
            totals[1],
            totals[2],
            valid,
            refusals,
            improvements,
            improvements as f64 / valid.max(1) as f64
        );
    }
}

#[test]
#[ignore]
fn first_use_of_each_stage_includes_the_driver_compiling_it() {
    let (old, new) = images();
    let (mut session, opened) = open(&old, &new);
    eprintln!("open {:.2} s", opened.as_secs_f64());
    let started = Instant::now();
    session.prepare().unwrap_or_else(|e| panic!("{}", e.msg));
    eprintln!(
        "prepare (first use of the table, seed, replay and select stages) {:.2} s",
        started.elapsed().as_secs_f64()
    );
    let started = Instant::now();
    session
        .run_generations(1)
        .unwrap_or_else(|e| panic!("{}", e.msg));
    eprintln!(
        "first generation (first use of breed) {:.2} s",
        started.elapsed().as_secs_f64()
    );
    let started = Instant::now();
    session
        .run_generations(1)
        .unwrap_or_else(|e| panic!("{}", e.msg));
    eprintln!("second generation {:.3} s", started.elapsed().as_secs_f64());
}

#[test]
#[ignore]
fn compressor_setup_per_chunk() {
    use miniz_oxide::deflate::core::CompressorOxide;
    use miniz_oxide::deflate::core::TDEFLFlush;
    use miniz_oxide::deflate::core::compress;
    use miniz_oxide::deflate::core::create_comp_flags_from_zip_params;
    let data: Vec<u8> = (0..65536usize).map(|k| (k % 251) as u8).collect();
    let flags = create_comp_flags_from_zip_params(6, 1, 0);
    let run = |reuse: bool| -> f64 {
        let started = Instant::now();
        let mut comp = CompressorOxide::new(flags);
        let mut out = vec![0u8; 1 << 14];
        let mut total = 0usize;
        for chunk in data.chunks(1) {
            if !reuse {
                comp = CompressorOxide::new(flags);
            } else {
                comp.reset();
            }
            let (_, _, produced) = compress(&mut comp, chunk, &mut out, TDEFLFlush::None);
            total += produced;
            let (_, _, produced) = compress(&mut comp, &[], &mut out, TDEFLFlush::Finish);
            total += produced;
        }
        assert!(total > 0);
        return started.elapsed().as_secs_f64();
    };
    eprintln!(
        "65536 one-byte chunks: new each time {:.3} s, reset and reuse {:.3} s",
        run(false),
        run(true)
    );
}

fn text(len: usize) -> Vec<u8> {
    return (0..len).map(|k| b"the quick brown fox "[k % 20]).collect();
}

fn periodic(len: usize, period: usize) -> Vec<u8> {
    return (0..len).map(|k| (k % period) as u8 + 1).collect();
}

struct Workload {
    name: &'static str,
    old: Vec<u8>,
    new: Vec<u8>,
    block: usize,
}

fn matrix(size: usize) -> Vec<Workload> {
    let old = noise(size, 1);
    let edited = |old: &[u8], edits: usize, width: usize| -> Vec<u8> {
        let mut new = old.to_vec();
        for k in 0..edits {
            let at = k * (old.len() / (edits + 1)) + 13;
            new[at..at + width].copy_from_slice(&noise(width, 100 + k as u64));
        }
        return new;
    };
    let mut relocated = old.clone();
    let region = size / 256;
    for k in 0..40 {
        let dst = k * (size / 41);
        let src = (dst + size / 2) % (size - region);
        relocated[dst..dst + region].copy_from_slice(&old[src..src + region]);
    }
    let mut short = old.clone();
    let mut x: u64 = 0x2545_F491_4F6C_DD1D;
    for _ in 0..2000 {
        x = x
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        let width = 8 + (x >> 60) as usize * 3;
        let at = ((x >> 16) as usize) % (size - width);
        short[at..at + width].copy_from_slice(&noise(width, x));
    }
    let mut repetitive = periodic(size, 37);
    for k in 0..4 {
        let at = k * (size / 5) + 7;
        repetitive[at..at + 512].copy_from_slice(&noise(512, 200 + k as u64));
    }
    let mut grown = old.clone();
    grown.extend_from_slice(&noise(size / 8 + 777, 9));
    let shrunk = edited(&old[..size - size / 8 - 333], 6, 300);
    return vec![
        Workload {
            name: "sparse-edits",
            new: edited(&old, 16, 64),
            old: old.clone(),
            block: 4096,
        },
        Workload {
            name: "relocated",
            new: relocated,
            old: old.clone(),
            block: 4096,
        },
        Workload {
            name: "short-extents",
            new: short,
            old: old.clone(),
            block: 4096,
        },
        Workload {
            name: "compressible",
            old: vec![0u8; size],
            new: text(size),
            block: 4096,
        },
        Workload {
            name: "incompressible",
            old: vec![0u8; size],
            new: noise(size, 7),
            block: 4096,
        },
        Workload {
            name: "repetitive",
            old: periodic(size, 37),
            new: repetitive,
            block: 4096,
        },
        Workload {
            name: "growth",
            old: old.clone(),
            new: grown,
            block: 4096,
        },
        Workload {
            name: "shrink",
            old: old.clone(),
            new: shrunk,
            block: 4096,
        },
        Workload {
            name: "one-byte-chunks",
            old: noise(size / 16, 1),
            new: edited(&noise(size / 16, 1), 16, 64),
            block: 1,
        },
    ];
}

fn hex(bytes: &[u8]) -> String {
    return bytes.iter().map(|b| format!("{:02x}", b)).collect();
}

#[test]
#[ignore]
fn workload_matrix() {
    use delta_update_generator::SearchConfig;
    use delta_update_payload::profile::EMBEDDED;
    use delta_update_payload::profile::HOST;
    let size: usize = std::env::var("OTA_MATRIX_SIZE")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(1 << 20);
    let generations: u64 = std::env::var("OTA_MATRIX_GENERATIONS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(20);
    let seeds: Vec<u64> = std::env::var("OTA_MATRIX_SEEDS")
        .unwrap_or_else(|_| "1".to_string())
        .split(',')
        .filter_map(|s| s.trim().parse().ok())
        .collect();
    let priv_pem: &[u8] = include_bytes!("../../delta-update-payload/tests/fixtures/private.pem");
    eprintln!(
        "workload\tprofile\tseed\tblock\tpayload\tdigest\traw_fitness\tgenerations\tevaluated\ttrials\tmappings\t\
         upload_s\tsearch_s\tencode_s\tfinalize_s\tdevice_bytes\tuploaded\tread_back"
    );
    for workload in matrix(size) {
        for (profile_name, profile) in [("host", &HOST), ("embedded", &EMBEDDED)] {
            for &seed in &seeds {
                let config = SearchConfig {
                    max_search_seconds: 600.0,
                    stagnation_seconds: 5.0,
                    min_improvement_bytes: 0,
                    seed: seed,
                    max_generations: Some(generations),
                    device: support::device(),
                    readable_target: true,
                    release_version: 0,
                };
                let built = match delta_update_generator::build_report(
                    &workload.old,
                    &workload.new,
                    b"A/B",
                    priv_pem,
                    workload.block,
                    profile,
                    &config,
                ) {
                    Err(e) => {
                        eprintln!(
                            "{}\t{}\t{}\tREFUSED {}",
                            workload.name, profile_name, seed, e.msg
                        );
                        continue;
                    }
                    Ok(b) => b,
                };
                let digest = hex(&delta_update_payload::sha256::hash_bytes(&built.payload)[..8]);
                eprintln!(
                    "{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{:.3}\t{:.3}\t{:.3}\t{:.3}\t{}\t{}\t{}",
                    workload.name,
                    profile_name,
                    seed,
                    workload.block,
                    built.payload.len(),
                    digest,
                    built.raw_fitness.unwrap_or(0),
                    built.generations,
                    built.evaluated,
                    built.trials,
                    built.encoding_mappings,
                    built.upload_seconds,
                    built.search_seconds,
                    built.compress_seconds,
                    built.finalize_seconds,
                    built.gpu.device_bytes,
                    built.gpu.uploaded,
                    built.gpu.read_back,
                );
            }
        }
    }
}
