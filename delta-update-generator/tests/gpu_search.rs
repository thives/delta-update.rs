mod support;

use std::cell::Cell;

use delta_update_generator::SearchConfig;
use delta_update_generator::control::Clock;
use delta_update_generator::control::Control;
use delta_update_generator::control::Stop;
use delta_update_generator::evaluate::Problem;
use delta_update_generator::evaluate::evaluate;
use delta_update_generator::gpu::Evaluation;
use delta_update_generator::gpu::Seed;
use delta_update_generator::gpu::Session;
use delta_update_generator::gpu_layout::*;
use delta_update_generator::program::Kind;
use delta_update_generator::program::Op;
use delta_update_generator::program::Span;
use delta_update_generator::search::Found;
use delta_update_generator::search::open_session;
use delta_update_generator::search::run_with;
use delta_update_payload::profile::HOST;
use delta_update_payload::profile::ResourceProfile;

struct Frozen;

impl Clock for Frozen {
    fn seconds(&self) -> f64 {
        return 0.0;
    }
}

fn span(start: u64, len: u64) -> Span {
    return Span {
        start: start,
        len: len,
    };
}

fn problem<'a>(old: &'a [u8], new: &'a [u8]) -> Problem<'a> {
    return Problem::new(old, new, 64, "A/B", &HOST, 256).unwrap_or_else(|e| panic!("{}", e.msg));
}

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

fn by_generations(seed: u64, generations: u64) -> SearchConfig {
    return SearchConfig {
        max_search_seconds: 600.0,
        stagnation_seconds: 3600.0,
        min_improvement_bytes: 1,
        seed: seed,
        max_generations: Some(generations),
        device: support::device(),
        release_version: 0,
        readable_target: true,
    };
}

fn search(old: &[u8], new: &[u8], seed: u64, generations: u64, batch: Option<u32>) -> Found {
    let p = problem(old, new);
    return run_with(&p, &by_generations(seed, generations), &Frozen, batch)
        .unwrap_or_else(|e| panic!("{}", e.msg));
}

fn relocated() -> (Vec<u8>, Vec<u8>) {
    let old = noise(8192, 1);
    let mut new = old.clone();
    new[4001..7001].copy_from_slice(&old[13..3013]);
    new[100..400].fill(0);
    return (old, new);
}

#[test]
fn every_finalist_is_a_complete_program_whose_raw_size_the_cpu_confirms() {
    let (old, new) = relocated();
    let p = problem(&old, &new);
    let found = search(&old, &new, 2, 30, None);
    assert_eq!(found.stop, Stop::Generations);
    assert_eq!(found.generations, 30);
    assert!(!found.finalists.is_empty());
    for f in &found.finalists {
        let e = evaluate(&p, f.program.clone(), &Control::unbounded())
            .unwrap_or_else(|_| panic!("deadline"));
        let e = e.unwrap_or_else(|| panic!("the CPU refuses a finalist: {:?}", f.program));
        assert_eq!(e.size, f.raw_size, "raw size of {:?}", f.program);
        assert_eq!(e.program, f.program, "a finalist is stored with its repair");
    }
    let best = found.finalists[0].raw_size;
    assert!(best < 8192 - 2000, "the best raw payload is {} bytes", best);
    assert!(
        found
            .finalists
            .iter()
            .any(|f| f.program.iter().any(|o| o.kind == Kind::SourceCopy))
    );
    assert!(
        found
            .finalists
            .windows(2)
            .all(|w| w[0].raw_size <= w[1].raw_size),
        "finalists are best first"
    );
}

#[test]
fn a_seed_and_a_generation_cap_reproduce_the_same_search() {
    let (old, new) = relocated();
    let a = search(&old, &new, 5, 12, None);
    let b = search(&old, &new, 5, 12, None);
    assert_eq!(a.finalists, b.finalists);
    assert_eq!(a.counters, b.counters);
    let c = search(&old, &new, 6, 12, None);
    assert!(
        a.finalists != c.finalists || a.counters != c.counters,
        "another seed must search differently"
    );
}

#[test]
fn splitting_a_run_into_batches_does_not_change_its_result() {
    let (old, new) = relocated();
    let whole = search(&old, &new, 9, 12, Some(12));
    for batch in [Some(1u32), Some(5), Some(64), None] {
        let split = search(&old, &new, 9, 12, batch);
        assert_eq!(split.finalists, whole.finalists, "batch {:?}", batch);
        assert_eq!(split.counters, whole.counters, "batch {:?}", batch);
        assert_eq!(split.generations, 12);
    }
}

#[test]
fn mutation_statistics_match_forced_proposals_and_reset() {
    let old = noise(512, 12);
    let mut new = old.clone();
    new[80..240].fill(0);
    new[300..450].copy_from_slice(&old[20..170]);
    let p = problem(&old, &new);
    let mut session =
        open_session(&p, &by_generations(7, 1)).unwrap_or_else(|e| panic!("{}", e.msg));
    let mut evaluator =
        open_session(&p, &by_generations(7, 1)).unwrap_or_else(|e| panic!("{}", e.msg));
    let seeded = session.prepare().unwrap_or_else(|e| panic!("{}", e.msg));
    assert_eq!(seeded.moves, [MoveCounters::default(); MV_COUNT]);
    evaluator.prepare().unwrap_or_else(|e| panic!("{}", e.msg));
    let parent = vec![Op::new(Kind::Replace, vec![span(0, 512)])];
    for mv in 0..MV_COUNT {
        let before = session
            .load_population(&[parent.clone()])
            .unwrap_or_else(|e| panic!("{}", e.msg));
        assert_eq!(before.moves, [MoveCounters::default(); MV_COUNT]);
        let children = session
            .breed_only(session.next_generation(), 3, mv as u32 + 1)
            .unwrap_or_else(|e| panic!("{}", e.msg));
        assert_eq!(session.counters().unwrap().moves, before.moves);
        let skipped = children.iter().filter(|c| c.is_none()).count() as u32;
        let programs: Vec<_> = children.into_iter().flatten().collect();
        let results = evaluator
            .evaluate(&programs)
            .unwrap_or_else(|e| panic!("{}", e.msg));
        let admitted = results.iter().filter(|r| r.size.is_some()).count() as u32;
        let best = results.iter().filter_map(|r| r.size).min();
        let after = session
            .run_forced_generation(3, mv as u32 + 1)
            .unwrap_or_else(|e| panic!("{}", e.msg));
        for id in 0..MV_COUNT {
            let expected = if id == mv {
                MoveCounters {
                    proposed: CHILDREN as u32,
                    skipped: skipped,
                    admitted: admitted,
                    new_best: u32::from(best.is_some_and(|b| b < before.best as u64)),
                }
            } else {
                MoveCounters::default()
            };
            assert_eq!(after.moves[id], expected, "forced move {mv}, counter {id}");
        }
        let immigrant = session.run_forced_generation(1, mv as u32 + 1).unwrap();
        assert_eq!(
            immigrant.moves, after.moves,
            "immigrants must not inherit mutation tags"
        );
        let restarted = session.restart_from_archive().unwrap();
        assert_eq!(
            restarted.moves, after.moves,
            "restart must preserve counters"
        );
    }
    let two = session
        .load_population(&[parent.clone(), Vec::new()])
        .unwrap();
    assert!(two.members >= 2);
    session.breed_only(session.next_generation(), 3, 1).unwrap();
    let crossover = session.run_forced_generation(2, 1).unwrap();
    assert_eq!(
        crossover.moves, two.moves,
        "crossovers must not inherit mutation tags"
    );
    let one = session.load_population(&[parent]).unwrap();
    assert_eq!(one.members, 1);
    let fallback_mutation = session.run_forced_generation(2, 1).unwrap();
    assert_eq!(fallback_mutation.moves[0].proposed, CHILDREN as u32);
    let cumulative = session.run_forced_generation(3, 1).unwrap();
    assert_eq!(cumulative.moves[0].proposed, 2 * CHILDREN as u32);
    session.evaluate(&[Vec::new()]).unwrap();
    assert_eq!(
        session.counters().unwrap().moves,
        [MoveCounters::default(); MV_COUNT]
    );
    assert!(session.run_forced_generation(4, 0).is_err());
    assert!(
        session
            .run_forced_generation(3, MV_COUNT as u32 + 1)
            .is_err()
    );
    let empty = session.load_population(&[]).unwrap();
    assert_eq!(empty.moves, [MoveCounters::default(); MV_COUNT]);
    let fallback = session.run_forced_generation(3, 1).unwrap();
    assert_eq!(
        fallback.moves, empty.moves,
        "an empty population breeds immigrants"
    );
    let reset = session.prepare().unwrap();
    assert_eq!(reset.moves, [MoveCounters::default(); MV_COUNT]);
}

#[test]
fn a_zero_generation_cap_still_seeds_and_evaluates_a_population() {
    let (old, new) = relocated();
    let found = search(&old, &new, 1, 0, None);
    assert_eq!(found.stop, Stop::Generations);
    assert_eq!(found.generations, 0);
    assert!(
        !found.finalists.is_empty(),
        "the seeding generation is evaluated"
    );
    assert_eq!(found.counters.evaluated as usize, NCAND);
    assert!(found.finalists.iter().any(|f| f.raw_size >= 8192));
}

fn children(
    old: &[u8],
    new: &[u8],
    parents: &[Vec<Op>],
    how: u32,
    mv: usize,
) -> (Vec<Vec<Op>>, Vec<Vec<Op>>) {
    let p = problem(old, new);
    let mut session =
        open_session(&p, &by_generations(1, 1)).unwrap_or_else(|e| panic!("{}", e.msg));
    session.prepare().unwrap_or_else(|e| panic!("{}", e.msg));
    let counters = session
        .load_population(parents)
        .unwrap_or_else(|e| panic!("{}", e.msg));
    assert!(counters.members as usize >= 1, "no parent was admitted");
    let held: Vec<Vec<Op>> = session
        .population()
        .unwrap_or_else(|e| panic!("{}", e.msg))
        .into_iter()
        .map(|f| f.program)
        .collect();
    let mut out: Vec<Vec<Op>> = Vec::new();
    for generation in 1..=12u32 {
        let bred = session
            .breed_only(
                generation,
                how,
                if mv == usize::MAX { 0 } else { mv as u32 + 1 },
            )
            .unwrap_or_else(|e| panic!("{}", e.msg));
        for child in bred.into_iter().flatten() {
            out.push(child);
        }
    }
    return (held, out);
}

fn assert_well_formed(program: &[Op], old_len: u64, new_len: u64) {
    for op in program {
        assert!(
            !op.dst.is_empty(),
            "an operation with no destination: {:?}",
            program
        );
        for d in &op.dst {
            assert!(
                d.len > 0 && d.start + d.len <= new_len,
                "destination {:?} of {:?}",
                d,
                program
            );
        }
        if !op.kind.is_copy() {
            assert!(op.src.is_empty(), "source extents on {:?}", op);
            continue;
        }
        let bound = if op.kind == Kind::SourceCopy {
            old_len
        } else {
            new_len
        };
        for s in &op.src {
            assert!(
                s.len > 0 && s.start + s.len <= bound,
                "source {:?} of {:?}",
                s,
                op
            );
        }
        let dst: u64 = op.dst.iter().map(|d| d.len).sum();
        let src: u64 = op.src.iter().map(|s| s.len).sum();
        assert_eq!(dst, src, "the streams of {:?} differ", op);
        if op.kind == Kind::TargetCopy {
            for s in &op.src {
                for d in &op.dst {
                    assert!(
                        s.start >= d.start + d.len || d.start >= s.start + s.len,
                        "{:?} reads its own destination",
                        op
                    );
                }
            }
        }
    }
}

fn gather(spans: &[Span], image: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    for s in spans {
        out.extend_from_slice(&image[s.start as usize..(s.start + s.len) as usize]);
    }
    return out;
}

fn assert_copies_are_exact(program: &[Op], old: &[u8], new: &[u8]) {
    for op in program {
        let want = gather(&op.dst, new);
        match op.kind {
            Kind::SourceCopy => assert_eq!(
                gather(&op.src, old),
                want,
                "a source copy that does not copy: {:?}",
                op
            ),
            Kind::TargetCopy => assert_eq!(
                gather(&op.src, new),
                want,
                "a target copy that does not copy: {:?}",
                op
            ),
            _ => {}
        }
    }
}

fn covered(program: &[Op]) -> Vec<(u64, u64)> {
    let mut v: Vec<(u64, u64)> = program
        .iter()
        .flat_map(|o| o.dst.iter().map(|s| (s.start, s.len)))
        .collect();
    v.sort();
    return v;
}

fn whole(len: u64) -> Vec<Op> {
    return vec![Op::new(Kind::Replace, vec![span(0, len)])];
}

#[test]
fn a_guided_copy_is_found_at_an_odd_offset_and_is_exact() {
    let (old, new) = relocated();
    let (_, kids) = children(&old, &new, &[whole(8192)], 3, MV_COPY_CREATE);
    assert!(!kids.is_empty());
    let mut found_odd = false;
    for kid in &kids {
        assert_well_formed(kid, 8192, 8192);
        assert_copies_are_exact(kid, &old, &new);
        for op in kid.iter().filter(|o| o.kind == Kind::SourceCopy) {
            let d = op.dst[0];
            let s = op.src[0];
            if d.start >= 4001 && d.start + d.len <= 7001 && s.start + 3988 == d.start {
                found_odd = true;
            }
        }
    }
    assert!(
        found_odd,
        "no guided copy of the relocated region at its true source"
    );
}

fn colliding_windows() -> (Vec<u8>, Vec<u8>) {
    let hash = |w: &[u8]| {
        w.iter().fold(0u32, |h, b| {
            h.wrapping_mul(16777619).wrapping_add(*b as u32)
        })
    };
    let mut seen: std::collections::HashMap<u32, Vec<u8>> = std::collections::HashMap::new();
    for k in 0..1_000_000u64 {
        let w = noise(64, 1000 + k);
        let h = hash(&w);
        if let Some(other) = seen.get(&h) {
            if *other != w {
                assert_eq!(hash(other), hash(&w));
                return (other.clone(), w);
            }
        }
        seen.insert(h, w);
    }
    panic!("no hash collision found");
}

#[test]
fn a_guided_copy_never_uses_a_hash_collision() {
    let (a, b) = colliding_windows();
    let build = |held: &[u8]| {
        let mut old = noise(4096, 3);
        old[1024..1088].copy_from_slice(held);
        let mut new = old.clone();
        for k in 0..8usize {
            new[1536 + k * 64..1536 + (k + 1) * 64].copy_from_slice(&b);
        }
        return (old, new);
    };
    let from_1024 = |kids: &[Vec<Op>]| {
        kids.iter()
            .flatten()
            .filter(|o| {
                o.kind == Kind::SourceCopy && o.src[0].start >= 1024 && o.src[0].start < 1088
            })
            .count()
    };
    let (old, new) = build(&b);
    let (_, kids) = children(&old, &new, &[whole(4096)], 3, MV_COPY_CREATE);
    for kid in &kids {
        assert_copies_are_exact(kid, &old, &new);
    }
    assert!(
        from_1024(&kids) > 0,
        "the true window was never proposed as a source: the probe does not reach it"
    );
    let (old, new) = build(&a);
    let (_, kids) = children(&old, &new, &[whole(4096)], 3, MV_COPY_CREATE);
    for kid in &kids {
        assert_copies_are_exact(kid, &old, &new);
    }
    assert_eq!(
        from_1024(&kids),
        0,
        "a copy was proposed through a hash collision"
    );
}

#[test]
fn a_general_copy_shorter_than_a_probe_is_reachable_and_exact() {
    let old = noise(4096, 41);
    let mut new = old.clone();
    for i in 0..20u64 {
        let d = (i * 200 + 10) as usize;
        let x = (i * 97 + 7) as usize;
        new[d..d + 63].copy_from_slice(&old[x..x + 63]);
    }
    let (_, kids) = children(&old, &new, &[whole(4096)], 3, MV_SHORT_COPY);
    let mut short = false;
    for kid in &kids {
        assert_well_formed(kid, 4096, 4096);
        assert_copies_are_exact(kid, &old, &new);
        short |= kid
            .iter()
            .any(|o| o.kind == Kind::SourceCopy && o.src[0].len < 64);
    }
    assert!(short, "no source copy shorter than a probe was proposed");
}

#[test]
fn a_target_copy_proposal_copies_the_new_image_from_elsewhere_and_never_overlaps() {
    let old = noise(4096, 12);
    let mut new = noise(4096, 13);
    let block = noise(200, 14);
    new[1000..1200].copy_from_slice(&block);
    new[3000..3200].copy_from_slice(&block);
    for mv in [MV_TARGET_CREATE, MV_SHORT_COPY] {
        let (_, kids) = children(&old, &new, &[whole(4096)], 3, mv);
        let mut target = false;
        for kid in &kids {
            assert_well_formed(kid, 4096, 4096);
            assert_copies_are_exact(kid, &old, &new);
            target |= kid.iter().any(|o| o.kind == Kind::TargetCopy);
        }
        assert!(target, "move {} proposed no target copy", mv);
    }
}

#[test]
fn a_zero_scan_finds_zero_bytes_inside_a_changed_cell() {
    let mut new = noise(1024, 5);
    new[200..210].fill(0);
    let old = vec![9u8; 1024];
    let (_, kids) = children(&old, &new, &[whole(1024)], 3, MV_ZERO_CREATE);
    let mut hit = false;
    for kid in &kids {
        assert_well_formed(kid, 1024, 1024);
        for op in kid.iter().filter(|o| o.kind == Kind::Zero) {
            let zeros = |d: &Span| {
                new[d.start as usize..(d.start + d.len) as usize]
                    .iter()
                    .all(|b| *b == 0)
            };
            assert!(
                op.dst.iter().all(zeros),
                "a zero over non-zero bytes: {:?}",
                op
            );
            hit |= op.dst[0].start >= 200 && op.dst[0].start + op.dst[0].len <= 210;
        }
    }
    assert!(hit, "a zero run inside a changed cell was not proposed");
}

#[test]
fn a_copy_shift_moves_a_copy_to_another_place_that_holds_the_same_bytes() {
    let mut old = noise(8192, 21);
    let block = noise(500, 22);
    old[1000..1500].copy_from_slice(&block);
    old[5000..5500].copy_from_slice(&block);
    let mut new = old.clone();
    new[2000..2500].copy_from_slice(&block);
    let parent = vec![
        Op::copy(
            Kind::SourceCopy,
            vec![span(2000, 500)],
            vec![span(1000, 500)],
        ),
        Op::new(Kind::Replace, vec![span(0, 2000)]),
        Op::new(Kind::Replace, vec![span(2500, 5692)]),
    ];
    let (held, kids) = children(&old, &new, &[parent.clone()], 3, MV_COPY_SHIFT);
    assert_eq!(
        held[0], parent,
        "the parent is complete and stored as it is"
    );
    let mut moved = false;
    for kid in &kids {
        assert_well_formed(kid, 8192, 8192);
        assert_copies_are_exact(kid, &old, &new);
        moved |= kid
            .iter()
            .any(|o| o.kind == Kind::SourceCopy && o.src[0].start == 5000);
    }
    assert!(moved, "the copy was never shifted to its other source");
}

#[test]
fn the_structural_moves_do_what_they_are_for() {
    let (old, new) = relocated();
    let a = vec![
        Op::new(Kind::Replace, vec![span(0, 3000)]),
        Op::new(Kind::Replace, vec![span(3000, 5192)]),
    ];
    let wide = vec![Op::new(
        Kind::Replace,
        vec![
            span(0, 1000),
            span(1000, 2000),
            span(5000, 3192),
            span(3000, 2000),
        ],
    )];
    let regroupable = vec![
        Op::new(Kind::Replace, vec![span(0, 1000), span(4000, 4192)]),
        Op::new(Kind::Replace, vec![span(1000, 3000)]),
    ];
    let copies = vec![
        Op::copy(
            Kind::SourceCopy,
            vec![span(4001, 1000)],
            vec![span(13, 1000)],
        ),
        Op::copy(
            Kind::SourceCopy,
            vec![span(5001, 1000)],
            vec![span(1013, 1000)],
        ),
        Op::copy(
            Kind::SourceCopy,
            vec![span(6001, 1000)],
            vec![span(2013, 1000)],
        ),
        Op::new(Kind::Replace, vec![span(0, 4001)]),
        Op::new(Kind::Replace, vec![span(7001, 1191)]),
    ];
    let one = whole(8192);

    let (_, kids) = children(&old, &new, &[a.clone()], 3, MV_SPLIT);
    assert!(
        kids.iter().any(|k| k.len() == a.len() + 1),
        "split made no extra operation"
    );
    kids.iter().for_each(|k| assert_well_formed(k, 8192, 8192));

    let (_, kids) = children(&old, &new, &[a.clone()], 3, MV_MERGE);
    assert!(
        kids.iter().any(|k| k == &whole(8192)),
        "merge did not join adjacent raw operations: {:?}",
        kids.first()
    );

    let (_, kids) = children(&old, &new, &[a.clone()], 3, MV_SHIFT);
    assert!(kids.iter().any(|k| k != &a), "shift changed nothing");
    kids.iter().for_each(|k| assert_well_formed(k, 8192, 8192));
    let (_, kids) = children(&old, &new, &[a.clone()], 3, MV_SHIFT_START);
    assert!(
        kids.iter()
            .any(|k| k != &a && k[0].dst[0].len == a[0].dst[0].len),
        "shift start changed nothing"
    );

    let (_, kids) = children(&old, &new, &[a.clone()], 3, MV_INSERT_ZERO);
    assert!(
        kids.iter()
            .any(|k| k.len() == a.len() + 1 && k.iter().any(|o| o.kind == Kind::Zero))
    );
    let (_, kids) = children(&old, &new, &[a.clone()], 3, MV_REMOVE);
    assert!(kids.iter().any(|k| k.len() == a.len() - 1));
    let (_, kids) = children(&old, &new, &[a.clone()], 3, MV_SWAP);
    assert!(
        kids.iter().any(|k| k[0] == a[1] && k[1] == a[0]),
        "swap did not exchange the two operations"
    );

    let (_, kids) = children(&old, &new, &[wide.clone()], 3, MV_REORDER);
    assert!(kids.iter().any(|k| k[0].dst != wide[0].dst));
    for k in &kids {
        let mut got = k[0].dst.clone();
        let mut want = wide[0].dst.clone();
        got.sort_by_key(|s| (s.start, s.len));
        want.sort_by_key(|s| (s.start, s.len));
        assert_eq!(got, want, "reorder changed what the operation writes");
    }

    let (held, kids) = children(&old, &new, &[regroupable.clone()], 3, MV_REGROUP);
    assert!(
        kids.iter()
            .any(|k| k != &held[0] && covered(k) == covered(&held[0])),
        "regroup moved no extent"
    );

    let (held, kids) = children(&old, &new, &[copies.clone()], 3, MV_COMPACT);
    let copy_ops = |p: &[Op]| p.iter().filter(|o| o.kind == Kind::SourceCopy).count();
    assert!(copy_ops(&held[0]) >= 3);
    assert!(
        kids.iter().any(|k| copy_ops(k) == 1),
        "compact left several source copies"
    );
    for k in &kids {
        assert_well_formed(k, 8192, 8192);
        assert_copies_are_exact(k, &old, &new);
    }
    let joined = kids.iter().find(|k| copy_ops(k) == 1).map(|k| k[0].clone());
    if let Some(op) = joined {
        assert_eq!(
            op.dst,
            vec![span(4001, 3000)],
            "the three contiguous copies are one extent"
        );
        assert_eq!(op.src, vec![span(13, 3000)]);
    }

    let (_, kids) = children(&old, &new, &[one.clone()], 3, MV_SWEEP);
    assert!(
        kids.iter()
            .any(|k| k.iter().any(|o| o.kind == Kind::SourceCopy)),
        "sweep made no copy"
    );
    for k in &kids {
        assert_well_formed(k, 8192, 8192);
        assert_copies_are_exact(k, &old, &new);
        assert!(
            k.iter().filter(|o| o.kind == Kind::SourceCopy).count() <= 1,
            "sweep leaves its copies joined"
        );
    }
}

#[test]
fn crossover_joins_a_prefix_of_one_parent_and_a_suffix_of_another() {
    let (old, new) = relocated();
    let a = vec![
        Op::new(Kind::Replace, vec![span(0, 3000)]),
        Op::new(Kind::Replace, vec![span(3000, 5192)]),
    ];
    let b = vec![
        Op::new(Kind::Replace, vec![span(0, 8192)]),
        Op::new(Kind::Zero, vec![span(100, 300)]),
    ];
    let (held, kids) = children(&old, &new, &[a, b], 2, usize::MAX);
    assert_eq!(held.len(), 2);
    assert!(!kids.is_empty());
    let pool: Vec<&Op> = held.iter().flatten().collect();
    for kid in &kids {
        assert!(
            kid.iter().all(|o| pool.contains(&o)),
            "a crossover invented an operation: {:?}",
            kid
        );
    }
    assert!(
        kids.iter().any(|k| k != &held[0] && k != &held[1]),
        "no crossover mixed the parents"
    );
}

#[test]
fn an_immigrant_is_a_random_cover_of_the_image_with_valid_extents() {
    let (old, new) = relocated();
    let (_, kids) = children(&old, &new, &[whole(8192)], 1, usize::MAX);
    assert!(kids.len() > 100);
    for kid in &kids {
        let mut at = 0u64;
        for (start, len) in covered(kid) {
            assert_eq!(start, at, "immigrant pieces tile the image: {:?}", kid);
            at = start + len;
        }
        assert_eq!(at, 8192);
        for op in kid {
            assert!(op.dst.iter().all(|d| d.len > 0 && d.start + d.len <= 8192));
            for s in &op.src {
                assert!(s.len > 0 && s.start + s.len <= 8192);
            }
        }
    }
    let kinds: std::collections::HashSet<_> = kids.iter().flatten().map(|o| o.kind).collect();
    assert!(kinds.contains(&Kind::Replace) && kinds.contains(&Kind::Zero));
    assert!(kinds.contains(&Kind::SourceCopy) && kinds.contains(&Kind::TargetCopy));
}

#[test]
fn no_target_copy_is_proposed_when_the_target_is_not_readable() {
    let (old, new) = relocated();
    let p = problem(&old, &new);
    let config = SearchConfig {
        readable_target: false,
        ..by_generations(3, 25)
    };
    let found = run_with(&p, &config, &Frozen, None).unwrap_or_else(|e| panic!("{}", e.msg));
    assert!(!found.finalists.is_empty());
    for f in &found.finalists {
        assert!(
            f.program.iter().all(|o| o.kind != Kind::TargetCopy),
            "{:?}",
            f.program
        );
    }
}

#[test]
fn a_repetitive_image_with_many_equal_sources_still_searches_and_every_finalist_is_valid() {
    for (old, new) in [
        (vec![0u8; 8192], {
            let mut v = vec![0u8; 8192];
            v[1000..1100].fill(7);
            v[5000..5300].fill(9);
            v
        }),
        (
            (0..8192usize).map(|k| (k % 7) as u8).collect::<Vec<u8>>(),
            {
                let mut v: Vec<u8> = (0..8192usize).map(|k| (k % 7) as u8).collect();
                v.rotate_left(3);
                v[2000..2100].fill(1);
                v
            },
        ),
    ] {
        let p = problem(&old, &new);
        let found = search(&old, &new, 4, 15, None);
        assert!(!found.finalists.is_empty());
        for f in &found.finalists {
            let e = evaluate(&p, f.program.clone(), &Control::unbounded())
                .unwrap_or_else(|_| panic!("deadline"));
            assert_eq!(e.map(|e| e.size), Some(f.raw_size), "{:?}", f.program);
        }
    }
}

#[test]
fn the_search_counts_what_it_refuses_and_still_returns_valid_programs() {
    let len = 4 * EXT_CAP;
    let old = vec![0u8; len];
    let new: Vec<u8> = (0..len).map(|k| if k % 2 == 0 { 1 } else { 0 }).collect();
    let found = search(&old, &new, 1, 5, None);
    assert!(
        found.counters.refused_for(S_EXTCAP) > 0,
        "{:?}",
        found.counters
    );
    assert!(
        !found.finalists.is_empty(),
        "the raw replacement is always admitted"
    );
    let p = problem(&old, &new);
    for f in &found.finalists {
        let e = evaluate(&p, f.program.clone(), &Control::unbounded())
            .unwrap_or_else(|_| panic!("deadline"));
        assert_eq!(e.map(|e| e.size), Some(f.raw_size));
    }
}

#[test]
fn the_images_go_up_once_and_only_small_records_come_back_while_the_search_runs() {
    let old = noise(1 << 16, 31);
    let mut new = old.clone();
    new[20_000..30_000].copy_from_slice(&old[40_000..50_000]);
    new[5000..5500].fill(0);
    let short = search(&old, &new, 3, 2, Some(1));
    let long = search(&old, &new, 3, 60, Some(1));
    let images = (old.len() + new.len()) as u64;
    for found in [&short, &long] {
        assert!(
            found.stats.uploaded < images + 100_000,
            "uploaded {} bytes for images of {}",
            found.stats.uploaded,
            images
        );
        assert!(
            found.stats.device_bytes > images,
            "the images are resident on the device"
        );
    }
    assert!(
        long.stats.read_back < 1_000_000,
        "read back {} bytes",
        long.stats.read_back
    );
    assert!(
        long.stats.read_back - short.stats.read_back < 58 * (CTL_WORDS as u64 * 4 + 44) + 200_000
    );
}

#[test]
fn an_image_the_search_cannot_address_is_refused_with_the_limit_that_failed() {
    let big = vec![0u8; MAX_IMAGE];
    let small = vec![0u8; 16];
    let p = problem(&small, &big);
    let refused = open_session(&p, &by_generations(1, 1));
    assert_eq!(
        refused.err().map(|e| e.msg),
        Some("an image is larger than the GPU search supports (it must be smaller than 1 GiB)")
    );
    let p = problem(&big, &small);
    assert!(open_session(&p, &by_generations(1, 1)).is_err());
}

#[test]
fn images_that_exceed_the_devices_storage_buffer_range_are_refused_not_streamed() {
    let config = SearchConfig {
        device: delta_update_generator::DevicePreference::Software,
        ..by_generations(1, 1)
    };
    let old = vec![1u8; 70 << 20];
    let new = vec![2u8; 70 << 20];
    let p = problem(&old, &new);
    match open_session(&p, &config) {
        Err(e) => assert_eq!(
            e.msg,
            "the two images together exceed the device's maximum storage buffer size"
        ),
        Ok(_) => panic!("a device with a 128 MiB storage buffer range held 140 MiB of images"),
    }
}

#[test]
fn the_software_and_hardware_searches_return_valid_finalists() {
    let (old, new) = relocated();
    let p = problem(&old, &new);
    let run = |device| {
        let config = SearchConfig {
            device: device,
            ..by_generations(7, 20)
        };
        return run_with(&p, &config, &Frozen, None).unwrap_or_else(|e| panic!("{}", e.msg));
    };
    let software = run(delta_update_generator::DevicePreference::Software);
    let automatic = run(delta_update_generator::DevicePreference::Auto);
    if automatic.device == software.device {
        return;
    }
    for found in [&software, &automatic] {
        assert!(
            !found.finalists.is_empty(),
            "device {:?} returned no finalists",
            found.device
        );
        for finalist in &found.finalists {
            let checked = evaluate(&p, finalist.program.clone(), &Control::unbounded())
                .unwrap_or_else(|_| panic!("deadline"));
            let checked = checked.unwrap_or_else(|| {
                panic!(
                    "device {:?} returned an invalid finalist: {:?}",
                    found.device, finalist.program
                )
            });
            assert_eq!(checked.size, finalist.raw_size, "device {:?}", found.device);
            assert_eq!(
                checked.program, finalist.program,
                "device {:?}",
                found.device
            );
        }
    }
}

#[test]
fn compaction_never_moves_a_copy_ahead_of_an_earlier_write_to_the_same_bytes() {
    let (old, new) = relocated();
    let parent = vec![
        Op::new(Kind::Replace, vec![span(0, 8192)]),
        Op::copy(
            Kind::SourceCopy,
            vec![span(4001, 1500)],
            vec![span(13, 1500)],
        ),
        Op::copy(
            Kind::SourceCopy,
            vec![span(5501, 1500)],
            vec![span(1513, 1500)],
        ),
    ];
    let (held, kids) = children(&old, &new, &[parent.clone()], 3, MV_COMPACT);
    assert_eq!(
        held[0], parent,
        "the parent is complete and stored as it is"
    );
    assert!(!kids.is_empty());
    for kid in &kids {
        let write = kid
            .iter()
            .position(|o| o.kind == Kind::Replace)
            .unwrap_or(usize::MAX);
        for (at, op) in kid.iter().enumerate() {
            assert!(
                op.kind != Kind::SourceCopy || at > write,
                "a copy moved ahead of the raw write: {:?}",
                kid
            );
        }
        assert_copies_are_exact(kid, &old, &new);
    }
}

#[test]
fn compaction_moves_a_copy_that_lies_between_the_extents_of_an_earlier_write() {
    let (old, new) = relocated();
    let parent = vec![
        Op::new(Kind::Replace, vec![span(0, 2048), span(6144, 2048)]),
        Op::copy(
            Kind::SourceCopy,
            vec![span(4001, 1500)],
            vec![span(13, 1500)],
        ),
        Op::copy(
            Kind::SourceCopy,
            vec![span(5501, 600)],
            vec![span(1513, 600)],
        ),
    ];
    let (_, kids) = children(&old, &new, &[parent], 3, MV_COMPACT);
    assert!(!kids.is_empty());
    for kid in &kids {
        let write = kid
            .iter()
            .position(|o| o.kind == Kind::Replace)
            .unwrap_or(usize::MAX);
        assert!(
            kid.iter()
                .position(|o| o.kind == Kind::SourceCopy)
                .is_some_and(|c| c < write),
            "no copy moved ahead of the raw write: {:?}",
            kid
        );
        assert_copies_are_exact(kid, &old, &new);
    }
}

fn prepared(old: &[u8], new: &[u8]) -> Session {
    let p = problem(old, new);
    let mut session =
        open_session(&p, &by_generations(1, 1)).unwrap_or_else(|e| panic!("{}", e.msg));
    session.prepare().unwrap_or_else(|e| panic!("{}", e.msg));
    return session;
}

/// The greedy seed, which is candidate 3 of the seeding generation.
fn greedy_seed(session: &mut Session) -> Seed {
    let seeds = session.seeds().unwrap_or_else(|e| panic!("{}", e.msg));
    return seeds[3].clone();
}

/// Evaluate `program` through the search's own stages. It must be admitted, and its evaluation,
/// repair included, is returned.
fn assert_evaluates(session: &mut Session, program: &[Op]) -> Evaluation {
    let evaluated = session
        .evaluate(&[program.to_vec()])
        .unwrap_or_else(|e| panic!("{}", e.msg));
    let first = evaluated.into_iter().next().unwrap();
    assert_eq!(first.status, S_OK as u32, "refused with status {}", first.status);
    assert!(first.size.is_some(), "no size for {:?}", program);
    return first;
}

#[test]
fn consecutive_restarts_propose_different_immigrants() {
    let (old, new) = relocated();
    let mut session = prepared(&old, &new);
    let first = session
        .restart_proposals()
        .unwrap_or_else(|e| panic!("{}", e.msg));
    let second = session
        .restart_proposals()
        .unwrap_or_else(|e| panic!("{}", e.msg));
    assert!(first.iter().flatten().count() > 0, "no immigrant was proposed");
    assert_ne!(first, second, "two restarts proposed the same immigrants");
}

#[test]
fn a_fresh_session_repeats_the_restart_sequence_whatever_its_generations() {
    let (old, new) = relocated();
    let mut first = prepared(&old, &new);
    let first_restart = first
        .restart_proposals()
        .unwrap_or_else(|e| panic!("{}", e.msg));
    let second_restart = first
        .restart_proposals()
        .unwrap_or_else(|e| panic!("{}", e.msg));

    let mut again = prepared(&old, &new);
    again
        .run_generations(3)
        .unwrap_or_else(|e| panic!("{}", e.msg));
    let repeated = again
        .restart_proposals()
        .unwrap_or_else(|e| panic!("{}", e.msg));
    assert_eq!(repeated, first_restart, "the first restart changed with the generations");
    let repeated_second = again
        .restart_proposals()
        .unwrap_or_else(|e| panic!("{}", e.msg));
    assert_eq!(repeated_second, second_restart);
}

#[test]
fn the_greedy_seed_copies_a_relocated_range_from_its_source() {
    let (old, new) = relocated();
    let mut session = prepared(&old, &new);
    let seed = greedy_seed(&mut session);
    assert_eq!(seed.status, S_OK as u32, "the seed was refused with status {}", seed.status);
    assert_copies_are_exact(&seed.program, &old, &new);
    let moved = seed.program.iter().any(|op| {
        op.kind == Kind::SourceCopy
            && op.dst[0].start >= 4001
            && op.dst[0].start + op.dst[0].len <= 7001
            && op.src[0].start + 3988 == op.dst[0].start
    });
    assert!(moved, "no seed copy of the relocated range at its source: {:?}", seed.program);
    assert_evaluates(&mut session, &seed.program);
}

#[test]
fn a_run_of_changed_cells_is_one_operation_in_the_greedy_seed() {
    // 600 changed cells, more than one program may hold as separate operations.
    let old = noise(600 * 512 + 512, 41);
    let new = noise(600 * 512, 42);
    let mut session = prepared(&old, &new);
    let seed = greedy_seed(&mut session);
    assert_eq!(seed.status, S_OK as u32, "the seed was refused with status {}", seed.status);
    assert_eq!(
        seed.program.len(),
        1,
        "the changed cells did not join: {} operations",
        seed.program.len()
    );
    assert_eq!(seed.program[0].kind, Kind::Replace);
    assert_eq!(seed.program[0].dst, vec![span(0, 600 * 512)]);
    let evaluated = assert_evaluates(&mut session, &seed.program);
    assert_eq!(evaluated.mismatch_spans, 0, "the seed leaves changed bytes for the repair");
}

#[test]
fn the_greedy_seed_stops_at_its_operation_bound_and_leaves_the_rest_to_the_repair() {
    // Every other cell changes, so the 550 changed cells are separate operations. The seed has room
    // for 511 of them, since one operation stays back for the repair, and it writes nothing past them.
    let old = noise(1100 * 512, 51);
    let mut new = old.clone();
    for cell in (0..1100).step_by(2) {
        new[cell * 512..(cell + 1) * 512].copy_from_slice(&noise(512, 60 + cell as u64));
    }
    let mut session = prepared(&old, &new);
    let seed = greedy_seed(&mut session);
    assert_eq!(
        seed.status,
        S_OK as u32,
        "the seed ran out of operations with status {}",
        seed.status
    );
    assert_eq!(seed.program.len(), OP_CAP - 1);
    let last = seed.program.last().unwrap().dst[0];
    assert_eq!(last.start + last.len, 1021 * 512, "the seed wrote past its bound");
    // Each of the 39 changed cells past the bound is at least one mismatch span: unchanged cells lie
    // between them, so no span joins two. A cell can hold more, where its new bytes equal the old.
    let evaluated = assert_evaluates(&mut session, &seed.program);
    assert!(
        evaluated.mismatch_spans >= 39,
        "{} mismatch spans for the 39 changed cells past the bound",
        evaluated.mismatch_spans
    );
    assert_eq!(evaluated.program.len(), OP_CAP, "the repair is the last operation");
}

#[test]
fn the_greedy_seed_stops_copying_once_its_comparison_budget_is_spent() {
    // The new image is the old one shifted by a byte, so every cell has a copy from one byte on. The
    // copies would cover the whole image if the seed could compare as much as it liked.
    let old = noise(1100 * 512 + 1, 71);
    let new = old[1..].to_vec();
    let mut session = prepared(&old, &new);
    let seed = greedy_seed(&mut session);
    assert_eq!(seed.status, S_OK as u32, "the seed was refused with status {}", seed.status);
    let sum = |kind: Kind| -> u64 {
        return seed
            .program
            .iter()
            .filter(|o| o.kind == kind)
            .flat_map(|o| o.dst.iter())
            .map(|s| s.len)
            .sum();
    };
    let copied = sum(Kind::SourceCopy);
    assert!(copied > 0, "the seed made no copy before its budget ran out");
    assert!(
        copied <= MAX_COMPARE as u64,
        "{} bytes copied against a budget of {}",
        copied,
        MAX_COMPARE
    );
    assert_eq!(copied + sum(Kind::Replace), new.len() as u64, "the rest is not written raw");
    assert_copies_are_exact(&seed.program, &old, &new);
    assert_evaluates(&mut session, &seed.program);
}

#[test]
fn programs_that_write_the_same_bytes_raw_but_copy_them_differently_all_stay_in_the_population() {
    let mut old = noise(400, 33);
    let head = old[0..100].to_vec();
    old[200..300].copy_from_slice(&head);
    let mut new = head.clone();
    new.extend_from_slice(&noise(100, 34));
    let raw = Op::new(Kind::Replace, vec![span(100, 100)]);
    let from_front = vec![
        Op::copy(Kind::SourceCopy, vec![span(0, 100)], vec![span(0, 100)]),
        raw.clone(),
    ];
    let from_later = vec![
        Op::copy(Kind::SourceCopy, vec![span(0, 100)], vec![span(200, 100)]),
        raw.clone(),
    ];
    let in_halves = vec![
        Op::copy(Kind::SourceCopy, vec![span(0, 50)], vec![span(0, 50)]),
        Op::copy(Kind::SourceCopy, vec![span(50, 50)], vec![span(50, 50)]),
        raw.clone(),
    ];
    let programs = [from_front.clone(), from_later.clone(), in_halves.clone()];

    let mut session = prepared(&old, &new);
    let counters = session
        .load_population(&programs)
        .unwrap_or_else(|e| panic!("{}", e.msg));
    assert_eq!(counters.members, 3, "a structurally different program was dropped");
    let held: Vec<Vec<Op>> = session
        .population()
        .unwrap_or_else(|e| panic!("{}", e.msg))
        .into_iter()
        .map(|f| f.program)
        .collect();
    for program in &programs {
        assert!(held.contains(program), "{:?} is not in the population", program);
    }
    let archived = session.finalists().unwrap_or_else(|e| panic!("{}", e.msg));
    assert_eq!(archived.len(), 1, "the archive keeps one program per raw-written set");
}

#[test]
fn structurally_identical_programs_still_collapse_in_the_population() {
    let mut old = noise(400, 33);
    let head = old[0..100].to_vec();
    old[200..300].copy_from_slice(&head);
    let mut new = head.clone();
    new.extend_from_slice(&noise(100, 34));
    let program = vec![
        Op::copy(Kind::SourceCopy, vec![span(0, 100)], vec![span(0, 100)]),
        Op::new(Kind::Replace, vec![span(100, 100)]),
    ];
    let mut session = prepared(&old, &new);
    let counters = session
        .load_population(&[program.clone(), program.clone()])
        .unwrap_or_else(|e| panic!("{}", e.msg));
    assert_eq!(counters.members, 1);
}

/// A session for `p`, which carries its own profile.
fn prepared_under(p: &Problem) -> Session {
    let mut session =
        open_session(p, &by_generations(1, 1)).unwrap_or_else(|e| panic!("{}", e.msg));
    session.prepare().unwrap_or_else(|e| panic!("{}", e.msg));
    return session;
}

#[test]
fn the_greedy_seed_makes_no_probe_past_its_lookup_bound() {
    // Unrelated bytes match nothing, so each changed cell costs its probes and compares no bytes. The
    // first MAX_LOOKUPS / IDX_MIN_STRIDE cells spend every probe, so a copy that starts after them is not
    // found, although the same copy at the start of the image is.
    let cells = MAX_LOOKUPS / IDX_MIN_STRIDE + 64;
    let tail = 64;
    let old = noise((cells + tail) * 512, 110);
    let copy = old[1..1 + tail * 512].to_vec();
    let unrelated = noise(cells * 512, 111);
    let mut late = unrelated.clone();
    late.extend_from_slice(&copy);
    let mut early = copy.clone();
    early.extend_from_slice(&unrelated);

    let mut session = prepared(&old, &late);
    let seed = greedy_seed(&mut session);
    assert_eq!(seed.status, S_OK as u32, "the seed was refused with status {}", seed.status);
    assert!(
        seed.program.iter().all(|o| o.kind != Kind::SourceCopy),
        "a copy past the lookup bound was made: {:?}",
        seed.program.iter().map(|o| o.dst.clone()).collect::<Vec<_>>()
    );
    // A cell that finds no match leaves nothing behind, so the seed counts the cells it enters the matcher for.
    // Each of the first cells spends its 16 probes, so exactly MAX_LOOKUPS / IDX_MIN_STRIDE enter; the 128
    // changed cells after them are never hashed.
    assert_eq!(
        seed.searched as usize,
        MAX_LOOKUPS / IDX_MIN_STRIDE,
        "of {} changed cells, {} entered the matcher",
        cells + tail,
        seed.searched
    );
    assert_evaluates(&mut session, &seed.program);

    let mut session = prepared(&old, &early);
    let seed = greedy_seed(&mut session);
    assert_eq!(seed.status, S_OK as u32, "the seed was refused with status {}", seed.status);
    assert!(
        seed.program.iter().any(|o| o.kind == Kind::SourceCopy),
        "no copy at the start of the image, where the lookups are not spent"
    );
    assert_copies_are_exact(&seed.program, &old, &early);
    assert_evaluates(&mut session, &seed.program);
}

#[test]
fn a_complete_single_copy_seed_fits_one_operation_and_exactly_its_extents() {
    // The new image is the old one shifted by a byte, so every cell is one copy, and the joined seed is one
    // operation of two extents. Nothing is left for the repair, so the seed may use all of the profile's.
    let old = noise(2049, 81);
    let new = old[1..].to_vec();
    let profile = ResourceProfile {
        max_operations: 1,
        max_total_extents: 2,
        ..HOST
    };
    let p = Problem::new(&old, &new, 64, "A/B", &profile, 256)
        .unwrap_or_else(|e| panic!("{}", e.msg));
    let mut session = prepared_under(&p);
    let seed = greedy_seed(&mut session);
    assert_eq!(seed.status, S_OK as u32, "the seed was refused with status {}", seed.status);
    assert_eq!(seed.program.len(), 1, "{:?}", seed.program);
    assert_eq!(seed.program[0].kind, Kind::SourceCopy);
    assert_eq!(seed.program[0].dst, vec![span(0, 2048)]);
    assert_eq!(seed.program[0].src, vec![span(1, 2048)]);
    let evaluated = assert_evaluates(&mut session, &seed.program);
    assert_eq!(evaluated.program.len(), 1, "a complete seed needs no repair operation");
}

#[test]
fn a_copy_the_extents_cannot_admit_gives_way_to_the_cells_raw_bytes() {
    // The same image, with one extent in all: the copy's two extents do not fit, so the seed writes the
    // cells raw, and they join into one operation of one extent.
    let old = noise(2049, 81);
    let new = old[1..].to_vec();
    let profile = ResourceProfile {
        max_operations: 1,
        max_total_extents: 1,
        ..HOST
    };
    let p = Problem::new(&old, &new, 64, "A/B", &profile, 256)
        .unwrap_or_else(|e| panic!("{}", e.msg));
    let mut session = prepared_under(&p);
    let seed = greedy_seed(&mut session);
    assert_eq!(seed.status, S_OK as u32, "the seed was refused with status {}", seed.status);
    assert_eq!(seed.program.len(), 1, "{:?}", seed.program);
    assert_eq!(seed.program[0].kind, Kind::Replace);
    assert_eq!(seed.program[0].dst, vec![span(0, 2048)]);
    assert_evaluates(&mut session, &seed.program);
}

#[test]
fn a_partial_seed_leaves_one_operation_for_the_repair() {
    // Cells 0 and 1 are a shifted copy, cell 2 is unchanged, cells 3 and 5 are unrelated, and cell 4 is
    // unchanged. With two operations the seed writes the copy alone, so the repair can add its operation.
    let old = noise(6 * 512, 91);
    let mut new = old.clone();
    new[0..1024].copy_from_slice(&old[1..1025]);
    new[1536..2048].copy_from_slice(&noise(512, 92));
    new[2560..3072].copy_from_slice(&noise(512, 93));
    let profile = ResourceProfile {
        max_operations: 2,
        max_total_extents: 64,
        ..HOST
    };
    let p = Problem::new(&old, &new, 64, "A/B", &profile, 256)
        .unwrap_or_else(|e| panic!("{}", e.msg));
    let mut session = prepared_under(&p);
    let seed = greedy_seed(&mut session);
    assert_eq!(seed.status, S_OK as u32, "the seed was refused with status {}", seed.status);
    assert_eq!(seed.program.len(), 1, "the seed kept {:?}", seed.program);
    assert_eq!(seed.program[0].kind, Kind::SourceCopy);
    assert_eq!(seed.program[0].dst, vec![span(0, 1024)]);
    let evaluated = assert_evaluates(&mut session, &seed.program);
    assert_eq!(evaluated.program.len(), 2, "the repair is the second operation");
}

#[test]
fn a_short_copy_whose_rest_is_already_right_is_kept_and_needs_no_repair() {
    // One changed cell: its first 100 bytes are the old image's bytes from 200, and the rest of the cell is the
    // old image's own. The copy is short of its cell, but what it leaves is already right, so one operation is
    // enough: the copy is kept, and the evaluation has nothing to repair.
    let old = noise(4096, 121);
    let mut new = old.clone();
    new[1536..1636].copy_from_slice(&old[200..300]);
    assert_ne!(old[300], old[1636], "the copy would run on into the rest of the cell");

    let profile = ResourceProfile {
        max_operations: 1,
        ..HOST
    };
    let p = Problem::new(&old, &new, 64, "A/B", &profile, 256)
        .unwrap_or_else(|e| panic!("{}", e.msg));
    let mut session = prepared_under(&p);
    let seed = greedy_seed(&mut session);
    assert_eq!(seed.status, S_OK as u32, "the seed was refused with status {}", seed.status);
    assert_eq!(seed.program.len(), 1, "{:?}", seed.program);
    assert_eq!(seed.program[0].kind, Kind::SourceCopy);
    assert_eq!(seed.program[0].dst, vec![span(1536, 100)]);
    assert_eq!(seed.program[0].src, vec![span(200, 100)]);
    let evaluated = assert_evaluates(&mut session, &seed.program);
    assert_eq!(evaluated.program.len(), 1, "the copy leaves nothing to repair");
    assert_eq!(evaluated.mismatch_spans, 0, "the copy leaves changed bytes behind");
}

#[test]
fn a_short_copy_with_a_fragmented_rest_gives_way_to_its_whole_cell() {
    // The same copy, but the rest of the cell differs from the old image in three runs. The repair would be one
    // raw operation of three extents, which a profile of two extents per operation refuses. The seed does not
    // predict that: it writes the whole cell, one extent, and the seed is admitted with nothing to repair.
    let old = noise(4096, 121);
    let mut new = old.clone();
    new[1536..1636].copy_from_slice(&old[200..300]);
    new[1700..1710].copy_from_slice(&noise(10, 123));
    new[1800..1810].copy_from_slice(&noise(10, 124));
    new[1900..1910].copy_from_slice(&noise(10, 125));

    let profile = ResourceProfile {
        max_extents_per_op: 2,
        ..HOST
    };
    let p = Problem::new(&old, &new, 64, "A/B", &profile, 256)
        .unwrap_or_else(|e| panic!("{}", e.msg));
    let mut session = prepared_under(&p);
    let seed = greedy_seed(&mut session);
    assert_eq!(seed.status, S_OK as u32, "the seed was refused with status {}", seed.status);
    assert_eq!(seed.program.len(), 1, "{:?}", seed.program);
    assert_eq!(seed.program[0].kind, Kind::Replace);
    assert_eq!(seed.program[0].dst, vec![span(1536, 512)]);
    let evaluated = assert_evaluates(&mut session, &seed.program);
    assert_eq!(evaluated.program.len(), 1, "the whole cell is written, so nothing is repaired");
    assert_eq!(evaluated.mismatch_spans, 0);
}

#[test]
fn a_short_copy_whose_rest_runs_past_the_old_image_gives_way_to_its_whole_cell() {
    // The last cell holds 1836 - 1536 = 300 bytes of the old image and 212 bytes past its end. The copy covers
    // the first 100, the next 200 are the old image's own, and the 212 after them are not in the old image at all,
    // so nothing says they are right. The rest is taken as wrong, and the cell is written whole. The profile has
    // room for a repair, so it is the check that decides, not the room.
    let old = noise(1836, 131);
    let mut new = old.clone();
    new.extend_from_slice(&noise(212, 132));
    new[1536..1636].copy_from_slice(&old[200..300]);
    assert_ne!(old[300], old[1636], "the copy would run on into the rest of the cell");

    let mut session = prepared(&old, &new);
    let seed = greedy_seed(&mut session);
    assert_eq!(seed.status, S_OK as u32, "the seed was refused with status {}", seed.status);
    assert_eq!(seed.searched, 1, "the copy was not looked for");
    assert_eq!(seed.program.len(), 1, "{:?}", seed.program);
    assert_eq!(seed.program[0].kind, Kind::Replace);
    assert_eq!(seed.program[0].dst, vec![span(1536, 512)]);
    let evaluated = assert_evaluates(&mut session, &seed.program);
    assert_eq!(evaluated.program.len(), 1, "the whole cell is written, so nothing is repaired");
}

#[test]
fn a_short_copy_whose_rest_cannot_be_compared_within_the_budget_gives_way_to_its_whole_cell() {
    // A shifted cell costs the seed 514 bytes of its comparison budget: 498 to extend the probe's match, and 16 to
    // extend it back to the cell's start. The cells before the last are shifted cells, and they leave a little
    // over one cell of the budget. The last cell starts with 100 bytes of a block the old image holds at `block`,
    // and ends with the old image's own bytes, so its rest is right.
    //
    // With the block held once, finding the copy costs 101, and its rest of 412 bytes fits in what is left, so
    // the copy is kept. With the block held twice there are two candidates to verify, 202 in all, and the rest
    // no longer fits in what is left: it cannot be shown to be right, and the cell is written whole. The new
    // image is the same in both, and the old images differ only in the second copy of the block.
    let shifted = (MAX_COMPARE - 513) / 514;
    let left = MAX_COMPARE - 514 * shifted;
    assert!(
        (513..614).contains(&left),
        "{} bytes of budget are left after {} shifted cells",
        left,
        shifted
    );
    let pos = shifted * 512;
    let block = (shifted + 3) * 512;
    let twin = (shifted + 5) * 512;
    let once = noise((shifted + 8) * 512, 141);
    assert_ne!(once[block + 100], once[pos + 100], "the copy would run on into the rest of the cell");
    let mut new = once[1..1 + pos].to_vec();
    new.extend_from_slice(&once[block..block + 100]);
    new.extend_from_slice(&once[pos + 100..pos + 512]);
    let mut twice = once.clone();
    twice.copy_within(block..block + 100, twin);
    assert_ne!(twice[twin + 100], twice[pos + 100], "the copy would run on into the rest of the cell");

    let p = problem(&once, &new);
    let mut session = prepared_under(&p);
    let seed = greedy_seed(&mut session);
    assert_eq!(seed.status, S_OK as u32, "the seed was refused with status {}", seed.status);
    assert_eq!(seed.searched as usize, shifted + 1, "a cell was not searched");
    let last = seed.program.last().unwrap();
    assert_eq!(last.kind, Kind::SourceCopy, "{:?}", seed.program);
    assert_eq!(last.dst, vec![span(pos as u64, 100)]);
    assert_eq!(last.src, vec![span(block as u64, 100)]);
    let evaluated = assert_evaluates(&mut session, &seed.program);
    assert_eq!(evaluated.program.len(), 2, "the copy leaves nothing to repair");
    assert_eq!(evaluated.mismatch_spans, 0);

    let p = problem(&twice, &new);
    let mut session = prepared_under(&p);
    let seed = greedy_seed(&mut session);
    assert_eq!(seed.status, S_OK as u32, "the seed was refused with status {}", seed.status);
    assert_eq!(seed.searched as usize, shifted + 1, "the last cell was not searched");
    let last = seed.program.last().unwrap();
    assert_eq!(last.kind, Kind::Replace, "{:?}", seed.program);
    assert_eq!(last.dst, vec![span(pos as u64, 512)]);
    let evaluated = assert_evaluates(&mut session, &seed.program);
    assert_eq!(evaluated.program.len(), 2, "the whole cell is written, so nothing is repaired");
    assert_eq!(evaluated.mismatch_spans, 0);
}

#[test]
fn a_profile_with_one_extent_per_operation_gets_a_seed_of_raw_bytes() {
    // A copy needs two extents in one operation, which this profile refuses, so the seed makes none, and
    // the raw bytes it writes are admitted. The copy would have matched, as the unrestricted seed shows.
    let old = noise(4096, 101);
    let new = old[1..].to_vec();
    let mut host = prepared(&old, &new);
    let copied = greedy_seed(&mut host);
    assert!(copied.program.iter().any(|o| o.kind == Kind::SourceCopy), "no copy to refuse");

    let profile = ResourceProfile {
        max_extents_per_op: 1,
        ..HOST
    };
    let p = Problem::new(&old, &new, 64, "A/B", &profile, 256)
        .unwrap_or_else(|e| panic!("{}", e.msg));
    let mut session = prepared_under(&p);
    let seed = greedy_seed(&mut session);
    assert_eq!(seed.status, S_OK as u32, "the seed was refused with status {}", seed.status);
    assert!(
        seed.program.iter().all(|o| o.kind != Kind::SourceCopy),
        "a copy of two extents was made under one extent per operation: {:?}",
        seed.program
    );
    assert_evaluates(&mut session, &seed.program);
}

/// A clock that moves a whole second on every read, so a window shorter than a second has expired by
/// the next read.
struct Stepping {
    reads: Cell<u64>,
}

impl Clock for Stepping {
    fn seconds(&self) -> f64 {
        let r = self.reads.get() + 1;
        self.reads.set(r);
        return r as f64;
    }
}

#[test]
fn a_window_that_expires_between_reads_still_gives_each_restart_a_generation() {
    // Every window has expired by the next read of the clock, so only the generation a restart waits for
    // stops a restarted population from being called stagnant at once. Each restart is then followed by
    // a generation, so there are at least as many generations as restarts.
    let old = noise(2048, 12);
    let mut new = old.clone();
    new[100..140].fill(1);
    let config = SearchConfig {
        release_version: 0,
        max_search_seconds: 1.0e9,
        stagnation_seconds: 0.4,
        min_improvement_bytes: 1 << 20,
        seed: 2,
        max_generations: Some(500),
        device: support::device(),
        readable_target: true,
    };
    let p = problem(&old, &new);
    let clock = Stepping {
        reads: Cell::new(0),
    };
    let found = run_with(&p, &config, &clock, Some(1)).unwrap_or_else(|e| panic!("{}", e.msg));
    assert_eq!(found.stop, Stop::Stagnation);
    assert!(found.restarts >= 1, "the search never restarted");
    assert!(
        found.generations >= u64::from(found.restarts),
        "{} generations for {} restarts: a restart was called stagnant before a generation ran on it",
        found.generations,
        found.restarts
    );
}

#[test]
fn the_generation_cap_and_the_deadline_still_stop_a_search_that_restarts() {
    // The same clock and window as above. The stagnation rule is deferred for a restart, but neither the
    // generation cap nor the deadline waits for it, so each still ends the search.
    let old = noise(2048, 12);
    let mut new = old.clone();
    new[100..140].fill(1);
    let p = problem(&old, &new);
    let config = |max_search_seconds: f64, max_generations: Option<u64>| SearchConfig {
        release_version: 0,
        max_search_seconds: max_search_seconds,
        stagnation_seconds: 0.4,
        min_improvement_bytes: 1 << 20,
        seed: 2,
        max_generations: max_generations,
        device: support::device(),
        readable_target: true,
    };

    let clock = Stepping {
        reads: Cell::new(0),
    };
    let capped = run_with(&p, &config(1.0e9, Some(2)), &clock, Some(1))
        .unwrap_or_else(|e| panic!("{}", e.msg));
    assert_eq!(capped.stop, Stop::Generations);
    assert_eq!(capped.generations, 2);

    // On this clock a deadline of 7 seconds falls after the first restart and before the generation that
    // would follow it: 6 to 8 seconds all do. The search reads the clock for its first checkpoint (1), the
    // two checkpoints of the equality scan (2, 3), the first recorded size (4), the loop's checkpoint (5), the
    // stagnation rule (6) and the restart's window (7). The next checkpoint (8) is the first at or past the
    // deadline, so the search has restarted once and run no generation. The checkpoint at the top of the loop
    // must stop it before the restarted population runs, although the stagnation rule has just fired.
    let clock = Stepping {
        reads: Cell::new(0),
    };
    let timed = run_with(&p, &config(7.0, None), &clock, Some(1))
        .unwrap_or_else(|e| panic!("{}", e.msg));
    assert_eq!(timed.stop, Stop::Deadline);
    assert_eq!(timed.restarts, 1, "the deadline did not fall after the first restart");
    assert_eq!(timed.generations, 0, "a generation ran before the deadline");
}
