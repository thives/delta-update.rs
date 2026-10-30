
mod support;

use prost::Message;

use ota_generator::SearchConfig;
use ota_generator::control::Clock;
use ota_generator::control::Control;
use ota_generator::evaluate::Problem;
use ota_generator::evaluate::evaluate;
use ota_generator::evaluate::manifest_for;
use ota_generator::gpu::Evaluation;
use ota_generator::gpu::Session;
use ota_generator::gpu_layout::*;
use ota_generator::program::Kind;
use ota_generator::program::Op;
use ota_generator::program::Span;
use ota_generator::search::open_session;
use ota_payload::prescan::scan_manifest;
use ota_payload::profile::ALLOC_OVERHEAD;
use ota_payload::profile::EMBEDDED;
use ota_payload::profile::HOST;
use ota_payload::profile::ResourceProfile;
use ota_payload::profile::SPAN_COST;
use ota_payload::profile::Usage;

fn span(start: u64, len: u64) -> Span {
    return Span { start: start, len: len };
}

struct NoClock;

impl Clock for NoClock {
    fn seconds(&self) -> f64 {
        return 0.0;
    }
}

fn config() -> SearchConfig {
    return SearchConfig {
        device: support::device(),
        ..SearchConfig::quick()
    };
}

fn problem<'a>(old: &'a [u8], new: &'a [u8], profile: &'a ResourceProfile) -> Problem<'a> {
    return Problem::new(old, new, 64, "A/B", profile, 256).unwrap_or_else(|e| panic!("{}", e.msg));
}

fn session(problem: &Problem) -> Session {
    return open_session(problem, &config()).unwrap_or_else(|e| panic!("{}", e.msg));
}

struct Lcg(u64);

impl Lcg {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        return self.0 >> 33;
    }

    fn below(&mut self, n: u64) -> u64 {
        return if n == 0 { 0 } else { self.next() % n };
    }
}

fn noise(len: usize, seed: u64) -> Vec<u8> {
    let mut r = Lcg(seed);
    return (0..len).map(|_| r.next() as u8).collect();
}

fn cpu(problem: &Problem, program: &[Op]) -> Option<(Vec<Op>, u64)> {
    return match evaluate(problem, program.to_vec(), &Control::unbounded()) {
        Ok(Some(e)) => Some((e.program, e.size)),
        Ok(None) => None,
        Err(_) => panic!("an unbounded evaluation cannot time out"),
    };
}

fn assert_agree(problem: &Problem, gpu: &mut Session, programs: &[Vec<Op>], what: &str) -> Vec<Evaluation> {
    let got = gpu.evaluate(programs).unwrap_or_else(|e| panic!("{}", e.msg));
    for (k, program) in programs.iter().enumerate() {
        let want = cpu(problem, program);
        match (&want, got[k].size) {
            (Some((repaired, size)), Some(raw)) => {
                assert_eq!(raw, *size, "{} #{}: size of {:?}", what, k, program);
                assert_eq!(&got[k].program, repaired, "{} #{}: repair of {:?}", what, k, program);
            }
            (None, None) => {}
            (Some((_, size)), None) => panic!(
                "{} #{}: the device refused ({}) a program the CPU admits at {} bytes: {:?}",
                what, k, STATUS_NAMES[got[k].status as usize], size, program
            ),
            (None, Some(raw)) => panic!("{} #{}: the device admits {} bytes of a program the CPU refuses: {:?}", what, k, raw, program),
        }
    }
    return got;
}

#[test]
fn the_device_sizes_simple_programs_exactly_like_the_manifest_encoder() {
    let old = noise(4096, 1);
    let mut new = old.clone();
    new[100..900].fill(0);
    new[2000..2300].copy_from_slice(&old[50..350]);
    let p = problem(&old, &new, &HOST);
    let mut gpu = session(&p);
    let programs = vec![
        vec![Op::new(Kind::Replace, vec![span(0, 4096)])],
        vec![],
        vec![Op::new(Kind::Zero, vec![span(100, 800)])],
        vec![
            Op::new(Kind::Zero, vec![span(100, 800)]),
            Op::copy(Kind::SourceCopy, vec![span(2000, 300)], vec![span(50, 300)]),
        ],
        vec![Op::copy(Kind::SourceCopy, vec![span(2000, 300)], vec![span(50, 300)])],
    ];
    assert_agree(&p, &mut gpu, &programs, "simple");
}

fn random_program(r: &mut Lcg, old_len: u64, new_len: u64, max_ops: u64) -> Vec<Op> {
    let mut program: Vec<Op> = Vec::new();
    if new_len == 0 {
        return program;
    }
    let ops = r.below(max_ops + 1);
    for _ in 0..ops {
        let kind = match r.below(4) {
            0 => Kind::Replace,
            1 => Kind::Zero,
            2 => Kind::SourceCopy,
            _ => Kind::TargetCopy,
        };
        let pieces = 1 + r.below(3);
        let mut dst: Vec<Span> = Vec::new();
        let mut total = 0u64;
        for _ in 0..pieces {
            let start = r.below(new_len);
            let room = new_len - start;
            let cap = std::cmp::min(room, 1 + r.below(300));
            let len = 1 + r.below(cap);
            dst.push(span(start, len));
            total += len;
        }
        if r.below(40) == 0 {
            let last = dst.len() - 1;
            dst[last].len += new_len;
        }
        if !kind.is_copy() {
            program.push(Op::new(kind, dst));
            continue;
        }
        let bound = if kind == Kind::SourceCopy { old_len } else { new_len };
        if bound == 0 {
            program.push(Op::new(Kind::Replace, dst));
            continue;
        }
        let mut src: Vec<Span> = Vec::new();
        let mut left = total;
        while left > 0 {
            let start = r.below(bound);
            let take = std::cmp::min(left, std::cmp::min(bound - start, 1 + r.below(300)));
            src.push(span(start, take));
            left -= take;
            if src.len() > 6 {
                break;
            }
        }
        if left > 0 || r.below(40) == 0 {
            src.push(span(0, 1));
        }
        program.push(Op::copy(kind, dst, src));
    }
    return program;
}

fn fuzz(old: &[u8], new: &[u8], profile: &ResourceProfile, seed: u64, rounds: usize, max_ops: u64) -> (usize, usize) {
    let p = problem(old, new, profile);
    let mut gpu = session(&p);
    let mut r = Lcg(seed);
    let (mut admitted, mut refused) = (0usize, 0usize);
    for round in 0..rounds {
        let programs: Vec<Vec<Op>> = (0..NCAND)
            .map(|_| random_program(&mut r, old.len() as u64, new.len() as u64, max_ops))
            .collect();
        let results = assert_agree(&p, &mut gpu, &programs, &format!("fuzz {} round {}", seed, round));
        for result in &results {
            if result.size.is_some() {
                admitted += 1;
            } else {
                refused += 1;
            }
        }
    }
    return (admitted, refused);
}

#[test]
fn random_programs_agree_with_the_cpu_replay_on_a_changed_image() {
    let old = noise(3000, 5);
    let mut new = old.clone();
    new[200..700].fill(0);
    new[1500..1900].copy_from_slice(&old[300..700]);
    new[2500..2600].reverse();
    let (admitted, refused) = fuzz(&old, &new, &HOST, 11, 12, 6);
    assert!(admitted > 20 && refused > 20, "the fuzz must exercise both outcomes: {} {}", admitted, refused);
}

#[test]
fn random_programs_agree_when_the_image_grows_or_shrinks() {
    let old = noise(1000, 6);
    let mut grown = noise(2500, 7);
    grown[..1000].copy_from_slice(&old);
    grown[1800..2100].fill(0);
    fuzz(&old, &grown, &HOST, 21, 10, 6);
    let long = noise(2500, 8);
    let mut shrunk = long[..1000].to_vec();
    shrunk[300..400].fill(0);
    fuzz(&long, &shrunk, &HOST, 22, 10, 6);
}

#[test]
fn random_programs_agree_on_odd_lengths_and_empty_images() {
    for (old_len, new_len) in [(1usize, 1usize), (2, 3), (5, 2), (7, 13), (13, 7), (0, 9), (9, 0), (0, 0), (4, 4)] {
        let old = noise(old_len, 30 + old_len as u64);
        let mut new = noise(new_len, 40 + new_len as u64);
        let common = std::cmp::min(old_len, new_len);
        new[..common / 2].copy_from_slice(&old[..common / 2]);
        fuzz(&old, &new, &HOST, 50 + old_len as u64 * 7 + new_len as u64, 4, 5);
    }
}

#[test]
fn random_programs_agree_on_repetitive_images_where_many_sources_match() {
    let old = vec![0u8; 2000];
    let mut new = vec![0u8; 2000];
    new[500..520].fill(9);
    new[1500..1503].fill(1);
    fuzz(&old, &new, &HOST, 61, 8, 7);
    let period: Vec<u8> = (0..3000usize).map(|k| (k % 7) as u8).collect();
    let mut shifted = period.clone();
    shifted.rotate_left(3);
    fuzz(&period, &shifted, &HOST, 62, 8, 7);
}

#[test]
fn the_embedded_profile_refuses_the_same_programs_on_both_sides() {
    let old = noise(6000, 70);
    let mut new = old.clone();
    for k in 0..40usize {
        new[k * 100 + 10..k * 100 + 40].fill(k as u8 + 1);
    }
    let p = problem(&old, &new, &EMBEDDED);
    let mut gpu = session(&p);
    let mut programs: Vec<Vec<Op>> = Vec::new();
    for ops in [1usize, 10, 30, 44, 45, 46, 50, 63, 64, 65] {
        let program: Vec<Op> = (0..ops)
            .map(|k| Op::new(Kind::Zero, vec![span(k as u64 * 3, 2)]))
            .collect();
        programs.push(program);
    }
    for extents in [100usize, 127, 128, 129, 200] {
        let dst: Vec<Span> = (0..extents).map(|k| span(k as u64 * 3, 2)).collect();
        programs.push(vec![Op::new(Kind::Zero, dst)]);
    }
    for chunk in programs.chunks(NCAND) {
        assert_agree(&p, &mut gpu, chunk, "embedded limits");
    }
}

#[test]
fn the_decode_memory_budget_of_the_embedded_profile_is_modeled_on_the_device() {
    let old = noise(4000, 71);
    let p = problem(&old, &old, &EMBEDDED);
    let mut gpu = session(&p);
    let mut verdicts: Vec<bool> = Vec::new();
    let counts: Vec<usize> = (20..=64).collect();
    for chunk in counts.chunks(NCAND) {
        let programs: Vec<Vec<Op>> = chunk
            .iter()
            .map(|n| {
                (0..*n)
                    .map(|k| Op::copy(Kind::SourceCopy, vec![span(k as u64 * 40, 20)], vec![span(k as u64 * 40, 20)]))
                    .collect()
            })
            .collect();
        let results = assert_agree(&p, &mut gpu, &programs, "decode budget");
        verdicts.extend(results.iter().map(|r| r.size.is_some()));
    }
    assert!(verdicts.iter().any(|v| *v) && verdicts.iter().any(|v| !*v), "the budget must bind inside the range: {:?}", verdicts);
}

fn wide_program(r: &mut Lcg, old_len: u64, new_len: u64) -> Vec<Op> {
    let mut program: Vec<Op> = Vec::new();
    let ops = 1 + r.below(5);
    for _ in 0..ops {
        let kind = match r.below(4) {
            0 => Kind::Replace,
            1 => Kind::Zero,
            _ => Kind::SourceCopy,
        };
        let count = 6 + r.below(40);
        let mut dst: Vec<Span> = Vec::new();
        let mut at = r.below(new_len / 8 + 1);
        for _ in 0..count {
            let len = 1 + r.below(std::cmp::max(1, new_len / (count * 3)));
            if at + len > new_len {
                break;
            }
            dst.push(span(at, len));
            at += len + r.below(std::cmp::max(1, new_len / (count * 3)));
        }
        if dst.is_empty() {
            continue;
        }
        if r.below(4) == 0 {
            let n = dst.len();
            for k in (1..n).rev() {
                dst.swap(k, r.below(k as u64 + 1) as usize);
            }
        }
        if kind != Kind::SourceCopy {
            program.push(Op::new(kind, dst));
            continue;
        }
        let total: u64 = dst.iter().map(|d| d.len).sum();
        if total > old_len {
            program.push(Op::new(Kind::Replace, dst));
            continue;
        }
        let mut src: Vec<Span> = Vec::new();
        let mut left = total;
        while left > 0 {
            let take = std::cmp::min(left, 1 + r.below(std::cmp::max(2, total / 4)));
            let start = r.below(old_len - take + 1);
            src.push(span(start, take));
            left -= take;
        }
        program.push(Op::copy(Kind::SourceCopy, dst, src));
    }
    return program;
}

#[test]
fn operations_with_many_ascending_extents_replay_like_the_cpu_model() {
    let old = noise(6000, 81);
    let mut new = old.clone();
    new[500..1500].copy_from_slice(&old[3000..4000]);
    new[2500..3000].fill(0);
    new[4000..4600].reverse();
    let p = problem(&old, &new, &HOST);
    let mut gpu = session(&p);
    let mut r = Lcg(5);
    let mut admitted = 0usize;
    for round in 0..30 {
        let programs: Vec<Vec<Op>> = (0..NCAND).map(|_| wide_program(&mut r, 6000, 6000)).collect();
        let results = assert_agree(&p, &mut gpu, &programs, &format!("wide round {}", round));
        admitted += results.iter().filter(|r| r.size.is_some()).count();
    }
    assert!(admitted > 300, "only {} programs were admitted", admitted);
}

fn nonzero_noise(len: usize, seed: u64) -> Vec<u8> {
    return noise(len, seed).into_iter().map(|b| if b == 0 { 1 } else { b }).collect();
}

#[test]
fn a_mismatch_that_crosses_scan_ranges_is_one_span_with_the_right_ends() {
    let old = nonzero_noise(1 << 20, 91);
    let new = old.clone();
    let p = problem(&old, &new, &HOST);
    let mut gpu = session(&p);
    let programs: Vec<Vec<Op>> = vec![
        vec![Op::new(Kind::Zero, vec![span(5000, 400_000)])],
        vec![Op::new(Kind::Zero, vec![span(8191, 2)])],
        vec![Op::new(Kind::Zero, vec![span(8192, 8192)])],
        vec![Op::new(Kind::Zero, vec![span(7000, 1192)])],
        vec![Op::new(Kind::Zero, vec![span(16_084, 300)])],
        vec![Op::new(Kind::Zero, vec![span(0, 1 << 20)])],
        vec![Op::new(Kind::Replace, vec![span(100, 700_000)])],
        vec![
            Op::new(Kind::Replace, vec![span(0, 1 << 20)]),
            Op::new(Kind::Zero, vec![span(500_000, 40_000)]),
            Op::new(Kind::Zero, vec![span(540_000, 20_000)]),
        ],
        vec![Op::copy(Kind::SourceCopy, vec![span(9000, 300_000)], vec![span(20_000, 300_000)])],
    ];
    let got = assert_agree(&p, &mut gpu, &programs, "ranges");
    let repair = got[0].program.last().map(|o| o.dst.clone()).unwrap_or_default();
    assert_eq!(repair, vec![span(5000, 400_000)], "the unbroken mismatch is one extent");
}

#[test]
fn random_programs_agree_on_an_image_of_many_scan_ranges() {
    let old = nonzero_noise(1 << 20, 93);
    let mut new = old.clone();
    new[100_000..300_000].copy_from_slice(&old[500_000..700_000]);
    new[800_000..820_000].fill(0);
    new[900_000..900_100].reverse();
    let p = problem(&old, &new, &HOST);
    let mut gpu = session(&p);
    let mut r = Lcg(17);
    for round in 0..4 {
        let programs: Vec<Vec<Op>> = (0..NCAND).map(|_| random_program(&mut r, 1 << 20, 1 << 20, 5)).collect();
        assert_agree(&p, &mut gpu, &programs, &format!("large round {}", round));
    }
}

#[test]
fn sizes_agree_at_every_varint_boundary() {
    let len = (2usize << 20) + 100;
    let old = nonzero_noise(len, 94);
    let new = old.clone();
    let p = problem(&old, &new, &HOST);
    let mut gpu = session(&p);
    let boundary = [127u64, 128, 129, 16383, 16384, 16385, 2_097_151, 2_097_152, 2_097_153];
    let mut programs: Vec<Vec<Op>> = Vec::new();
    for b in boundary {
        programs.push(vec![Op::new(Kind::Replace, vec![span(b, 1)])]);
        programs.push(vec![Op::new(Kind::Zero, vec![span(0, b)])]);
        programs.push(vec![Op::copy(Kind::SourceCopy, vec![span(b, 1)], vec![span(0, 1)])]);
        programs.push(vec![Op::copy(Kind::SourceCopy, vec![span(0, 1)], vec![span(b, 1)])]);
    }
    for steps in [vec![100u64, 100], vec![127, 1, 1], vec![16383, 1, 1, 1], vec![8000, 8000, 400]] {
        let mut at = 0u64;
        let mut program: Vec<Op> = Vec::new();
        for n in steps {
            program.push(Op::new(Kind::Replace, vec![span(at, n)]));
            at += n;
        }
        programs.push(program);
    }
    programs.push(vec![Op::new(Kind::Zero, (0..40u64).map(|k| span(k * 100_000, 99_999)).collect())]);
    for chunk in programs.chunks(NCAND) {
        assert_agree(&p, &mut gpu, chunk, "varint boundary");
    }
}

#[test]
fn a_target_copy_reads_unknown_growth_only_after_it_is_written() {
    let old = noise(1000, 95);
    let new = noise(2500, 96);
    let p = problem(&old, &new, &HOST);
    let mut gpu = session(&p);
    let copy = Op::copy(Kind::TargetCopy, vec![span(0, 100)], vec![span(1200, 100)]);
    let programs = vec![
        vec![copy.clone()],
        vec![Op::new(Kind::Zero, vec![span(1200, 100)]), copy.clone()],
        vec![Op::new(Kind::Replace, vec![span(1200, 100)]), copy.clone()],
        vec![Op::new(Kind::Zero, vec![span(1200, 99)]), copy.clone()],
        vec![Op::copy(Kind::TargetCopy, vec![span(0, 100)], vec![span(950, 100)])],
        vec![Op::new(Kind::Replace, vec![span(1000, 50)]), Op::copy(Kind::TargetCopy, vec![span(0, 100)], vec![span(950, 100)])],
    ];
    let got = assert_agree(&p, &mut gpu, &programs, "growth reads");
    assert_eq!(got[0].status, S_TARGET as u32, "the unwritten read is refused for its reason");
    assert!(got[1].size.is_some() && got[2].size.is_some());
    assert_eq!(got[3].status, S_TARGET as u32);
    assert_eq!(got[4].status, S_TARGET as u32);
    assert!(got[5].size.is_some());
}

#[test]
fn a_target_copy_across_a_fragmented_target_agrees_with_the_cpu() {
    let old = noise(4000, 97);
    let new = noise(4000, 98);
    let p = problem(&old, &new, &HOST);
    let mut gpu = session(&p);
    let build = vec![
        Op::new(Kind::Replace, vec![span(1000, 100), span(1300, 50)]),
        Op::new(Kind::Zero, vec![span(1100, 100)]),
        Op::copy(Kind::SourceCopy, vec![span(1200, 100)], vec![span(3000, 100)]),
        Op::new(Kind::Replace, vec![span(1350, 20)]),
    ];
    let mut programs: Vec<Vec<Op>> = Vec::new();
    for (dst, src) in [(span(2000, 400), span(1000, 400)), (span(1990, 380), span(1000, 380)), (span(3000, 200), span(1050, 200))] {
        let mut program = build.clone();
        program.push(Op::copy(Kind::TargetCopy, vec![dst], vec![src]));
        programs.push(program);
    }
    let mut program = build.clone();
    program.push(Op::copy(
        Kind::TargetCopy,
        vec![span(3500, 150), span(2500, 250)],
        vec![span(1000, 80), span(1090, 200), span(1300, 120)],
    ));
    programs.push(program);
    assert_agree(&p, &mut gpu, &programs, "fragmented target copy");
}

#[test]
fn a_total_length_past_the_sum_limit_is_refused_as_an_overflow() {
    let old = noise(1 << 20, 99);
    let p = problem(&old, &old, &HOST);
    let mut gpu = session(&p);
    let program = vec![Op::copy(Kind::SourceCopy, vec![span(0, 1 << 20); 2048], vec![span(0, 1 << 20); 2048])];
    let got = gpu.evaluate(&[program]).unwrap_or_else(|e| panic!("{}", e.msg));
    assert_eq!(got[0].status, S_OVERFLOW as u32, "{}", STATUS_NAMES[got[0].status as usize]);
    assert!(got[0].size.is_none());
}

#[test]
fn each_workspace_capacity_refuses_with_its_own_reason() {
    let len = 4 * EXT_CAP;
    let old = vec![0u8; len];
    let new: Vec<u8> = (0..len).map(|k| if k % 2 == 0 { 1 } else { 0 }).collect();
    let p = problem(&old, &new, &HOST);
    let mut gpu = session(&p);
    let got = gpu.evaluate(&[vec![]]).unwrap_or_else(|e| panic!("{}", e.msg));
    assert_eq!(got[0].status, S_EXTCAP as u32, "{}", STATUS_NAMES[got[0].status as usize]);
    assert_eq!(got[0].mismatch_spans as usize, 2 * EXT_CAP);

    let full: Vec<Op> = (0..OP_CAP).map(|k| Op::new(Kind::Zero, vec![span(k as u64 * 2, 1)])).collect();
    let got = gpu.evaluate(&[full]).unwrap_or_else(|e| panic!("{}", e.msg));
    assert_eq!(got[0].status, S_OPCAP as u32, "{}", STATUS_NAMES[got[0].status as usize]);

    let too_many: Vec<Op> = (0..=OP_CAP).map(|k| Op::new(Kind::Zero, vec![span(k as u64, 1)])).collect();
    let got = gpu.evaluate(&[too_many]).unwrap_or_else(|e| panic!("{}", e.msg));
    assert_eq!(got[0].status, S_OPCAP as u32);

    let pieces = (RUN_CAP / 2 + RUN_CAP / 8) as u64;
    let half = (pieces / 2) as usize;
    let sparse: Vec<Span> = (0..pieces).map(|k| span(k * 4, 1)).collect();
    let mut shuffled = sparse.clone();
    for k in (1..shuffled.len()).rev() {
        shuffled.swap(k, (k * 7919) % (k + 1));
    }
    let ascending = vec![Op::new(Kind::Zero, sparse.clone())];
    let unordered = vec![Op::new(Kind::Zero, shuffled)];
    let many_pieces = vec![
        Op::new(Kind::Zero, sparse[..half].to_vec()),
        Op::new(Kind::Zero, (0..half as u64).map(|k| span(k * 4 + 2, 1)).collect()),
    ];
    let got = gpu.evaluate(&[ascending, unordered, many_pieces]).unwrap_or_else(|e| panic!("{}", e.msg));
    for (k, what) in ["ascending", "unordered", "many pieces"].iter().enumerate() {
        let status = got[k].status as usize;
        assert!(status == S_RUNCAP || status == S_WORK, "{}: {}", what, STATUS_NAMES[status]);
        assert!(got[k].size.is_none());
    }
    assert_eq!(got[0].status, S_RUNCAP as u32, "the merged write is refused for the arena");
    assert_eq!(got[2].status, S_RUNCAP as u32, "runs add up across operations");
}

#[test]
fn a_repair_the_profile_cannot_hold_is_refused_for_that_reason() {
    let old = noise(4096, 101);
    let mut new = old.clone();
    new[100..200].fill(3);
    new[2000..2100].fill(4);
    let one_op = ResourceProfile {
        max_operations: 1,
        ..EMBEDDED
    };
    let p = problem(&old, &new, &one_op);
    let mut gpu = session(&p);
    let programs = vec![
        vec![Op::new(Kind::Replace, vec![span(100, 100)])],
        vec![Op::new(Kind::Replace, vec![span(100, 100), span(2000, 100)])],
    ];
    let got = assert_agree(&p, &mut gpu, &programs, "one operation");
    assert_eq!(got[0].status, S_REPAIR as u32, "{}", STATUS_NAMES[got[0].status as usize]);
    assert!(got[1].size.is_some());
}

fn decoded_charge(problem: &Problem, program: &[Op]) -> Usage {
    let mapped = match problem.mapping.map(program, &Control::unbounded(), false) {
        Err(_) => panic!("an unbounded mapping cannot be interrupted"),
        Ok(m) => m,
    };
    assert!(mapped.mismatches.is_empty(), "the program is not complete, so its charge is not its own");
    let manifest = manifest_for(problem, mapped.ops, mapped.blob_len);
    let unbounded = ResourceProfile {
        max_decoded_bytes: usize::MAX,
        max_encoded_manifest: usize::MAX,
        ..*problem.profile
    };
    return match scan_manifest(&manifest.encode_to_vec(), &unbounded) {
        Err(e) => panic!("{}", e.msg),
        Ok(u) => u,
    };
}

fn workspace_program(ops: usize, extents: usize) -> Vec<Op> {
    return (0..ops)
        .map(|k| {
            let dst: Vec<Span> = (0..extents).map(|e| span(((k * extents + e) * 8) as u64, 4)).collect();
            if k % 2 == 0 {
                Op::copy(Kind::SourceCopy, dst.clone(), dst)
            } else {
                Op::new(Kind::Replace, dst)
            }
        })
        .collect();
}

fn agree_either_side_of_the_charge(old: &[u8], program: &[Op], limit: usize, what: &str) {
    for budget in [limit - 1, limit, limit + 1] {
        let profile = ResourceProfile {
            max_decoded_bytes: budget,
            ..EMBEDDED
        };
        let p = problem(old, old, &profile);
        let mut gpu = session(&p);
        let got = assert_agree(&p, &mut gpu, &[program.to_vec()], &format!("{} at {}", what, budget));
        assert_eq!(got[0].size.is_some(), budget >= limit, "{} at {}: the verdict does not follow the charge", what, budget);
    }
}

#[test]
fn the_contract_workspace_is_charged_on_the_device_one_byte_either_side_of_the_decoded_limit() {
    let old = noise(4096, 91);
    let base = problem(&old, &old, &EMBEDDED);
    for (ops, extents) in [(2usize, 30usize), (3, 20), (4, 16), (5, 10)] {
        let program = workspace_program(ops, extents);
        let total: usize = program.iter().map(|o| o.dst.len() + o.src.len()).sum();
        let usage = decoded_charge(&base, &program);
        assert_eq!(usage.transient_bytes, total * SPAN_COST + ALLOC_OVERHEAD, "{}x{}", ops, extents);
        agree_either_side_of_the_charge(&old, &program, usage.accounted(), &format!("{}x{} extents", ops, extents));
    }
}

#[test]
fn a_program_with_no_extents_is_admitted_exactly_at_its_charge_on_the_device() {
    let old = noise(4096, 93);
    let base = problem(&old, &old, &EMBEDDED);
    let program: Vec<Op> = Vec::new();
    let limit = decoded_charge(&base, &program).accounted();
    agree_either_side_of_the_charge(&old, &program, limit, "no extents");
}

#[test]
fn a_search_under_a_budget_that_binds_on_the_workspace_keeps_only_programs_the_cpu_admits() {
    let old = noise(8192, 95);
    let mut new = old.clone();
    new[1000..3000].copy_from_slice(&old[5000..7000]);
    new[4000..4400].fill(0);
    let limit = decoded_charge(&problem(&old, &old, &EMBEDDED), &workspace_program(5, 10)).accounted();
    let profile = ResourceProfile {
        max_decoded_bytes: limit - 1,
        ..HOST
    };
    let p = problem(&old, &new, &profile);
    let config = SearchConfig {
        max_search_seconds: 600.0,
        stagnation_seconds: 3600.0,
        min_improvement_bytes: 1,
        seed: 3,
        max_generations: Some(20),
        device: support::device(),
        release_version: 0,
        readable_target: true,
    };
    let found = ota_generator::search::run_with(&p, &config, &NoClock, None).unwrap_or_else(|e| panic!("{}", e.msg));
    assert!(!found.finalists.is_empty(), "the search kept nothing under the budget");
    for f in &found.finalists {
        let cpu = evaluate(&p, f.program.clone(), &Control::unbounded()).unwrap_or_else(|_| panic!("deadline"));
        assert!(cpu.is_some(), "the search kept a program the CPU refuses: {:?}", f.program);
        assert_eq!(cpu.unwrap().size, f.raw_size, "{:?}", f.program);
    }
}
