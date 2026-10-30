//! The encoding pass: from the device's raw programs to the smallest admissible payload.
//!
//! The device searches over *what* a program writes and in what order, scoring every data operation
//! as a raw `REPLACE`. It knows nothing about compression, so its best raw program is not
//! necessarily the one that compresses best. This pass runs on the CPU after the search, over a
//! small set of candidates: the device's finalists, the whole-image raw replacement, and the repair
//! of the empty program, which the device does not need to have kept.
//!
//! For each candidate it chooses an encoding for every data operation: the raw blob, or
//! `REPLACE_FRAMES` or `REPLACE_SPARSE` at a compression level. A change is accepted only when the
//! *whole payload* is smaller, counting the manifest and the frame tables, and the profile admits
//! it, so an encoding that costs more than it saves is never taken. Every trial goes through the
//! mapper and `evaluate`, the same replay and admission as everything else, so a variant is
//! complete and admissible exactly when the payload built from it would be.
//!
//! `ZERO`, `SOURCE_COPY` and `TARGET_COPY` have no other encoding and are left as they are. The
//! operation order and each operation's extent stream are never touched, so a trial changes what
//! one operation stores and nothing about what the program writes.
//!
//! The pass is deterministic and bounded by work, not by time. Every trial costs the data bytes of all its
//! operations (a sixteenth of that when the frames are already cached, plus a charge for every codec chunk),
//! and the pass stops trying variants once the total passes [`WORK_BUDGET`], keeping the best it has. The
//! strongest uniform encoding of the first [`FREE_CANDIDATES`] candidates, the device's best raw program and
//! the whole-image baseline, is tried whatever the budget, so the pass does not return an unencoded program
//! only because the budget was small. These charges are logical: they do not depend on how a trial is
//! evaluated.
//!
//! A trial is evaluated in one of two ways with the same result. By replay, the whole program is mapped. By
//! reuse, the candidate is analysed once ([`Prepared`]), and a trial encodes only the operations it changes,
//! taking the others' encodings from a cache bounded in bytes. The analysis rests on one fact: after any data
//! operation the target holds the new bytes over its destination, whatever the encoding, so the keep and zero
//! decisions are the same for every encoding.

use std::cell::Cell;
use std::cell::RefCell;
use std::collections::HashSet;

use delta_update_payload::manifest::Manifest;
use delta_update_payload::manifest::Operation;
use delta_update_payload::manifest::TYPE_REPLACE_FRAMES;
use delta_update_payload::manifest::TYPE_REPLACE_SPARSE;
use delta_update_payload::sha256;

use crate::control::Control;
use crate::control::Halt;
use crate::control::Step;
use crate::evaluate::Evaluated;
use crate::evaluate::Problem;
use crate::evaluate::evaluate;
use crate::evaluate::manifest_for;
use crate::evaluate::payload_size;
use crate::evaluate::within_count_limits;
use crate::mapper::Chunk;
use crate::mapper::Coded;
use crate::mapper::Mapped;
use crate::mapper::copy_wire;
use crate::mapper::packed_mode;
use crate::mapper::placeholder;
use crate::mapper::raw_replace_wire;
use crate::mapper::wire;
use crate::mapper::zero_wire;
use crate::program::Kind;
use crate::program::Op;
use crate::program::Program;
use crate::program::Span;
use crate::program::total;

/// Work the pass may spend in all, over every trial. A trial that compresses costs the data bytes of
/// its program plus [`CHUNK_COST`] for every codec chunk they are cut into: a chunk is compressed,
/// framed and mapped on its own, and with small chunks that costs more than the bytes do. A trial whose
/// frames are cached only maps and compares, which costs a sixteenth of the bytes. The unit is about
/// twenty nanoseconds, so the budget is a few seconds of work.
pub const WORK_BUDGET: u64 = 1 << 28;
/// Candidates, from the front of the list, whose strongest uniform encoding is tried whatever the
/// budget.
pub const FREE_CANDIDATES: usize = 2;
/// What one codec chunk costs, in bytes of data. Mapping and compressing a chunk costs a few
/// microseconds before it costs anything for its bytes.
pub const CHUNK_COST: u64 = 512;
/// Most operations the per-operation refinement visits in one candidate, largest first.
pub const REFINED_OPS: usize = 64;

type Encoding = (Kind, Option<u8>);

type Entry = (Encoding, Coded);

const RAW: Encoding = (Kind::Replace, None);

const ENCODINGS: [Encoding; 8] = [
    (Kind::Sparse, Some(9)),
    (Kind::Frames, Some(9)),
    (Kind::Sparse, Some(6)),
    (Kind::Sparse, Some(1)),
    (Kind::Frames, Some(6)),
    (Kind::Frames, Some(1)),
    (Kind::Sparse, None),
    RAW,
];

/// What the pass found.
#[derive(Clone, Debug)]
pub struct Outcome {
    /// The smallest admissible program, with its exact payload size.
    pub best: Evaluated,
    /// Candidates whose raw program the profile admits, and candidates it refused outright.
    pub admitted_raw: usize,
    pub refused_raw: usize,
    /// Programs evaluated in all, trials included.
    pub trials: usize,
    /// Whole-program replays the pass made to evaluate them.
    pub mappings: u64,
    /// The size of the smallest admissible raw candidate, before any encoding was chosen. `None`
    /// when the profile admitted no raw candidate.
    pub raw_best: Option<u64>,
}

fn op_cost(op: &Op, chunk: u64) -> u64 {
    let bytes = total(&op.dst).unwrap_or(0);
    return bytes.saturating_add(bytes.div_ceil(chunk.max(1)).saturating_mul(CHUNK_COST));
}

fn data_cost(program: &[Op], chunk: u64) -> u64 {
    return program
        .iter()
        .filter(|o| o.kind.carries_data())
        .map(|o| op_cost(o, chunk))
        .fold(0u64, |a, b| a.saturating_add(b));
}

fn mapping_cost(program: &[Op], chunk: u64) -> u64 {
    return program
        .iter()
        .filter(|o| o.kind.carries_data())
        .map(|o| {
            let bytes = total(&o.dst).unwrap_or(0);
            bytes / 16 + bytes.div_ceil(chunk.max(1)).saturating_mul(CHUNK_COST)
        })
        .fold(0u64, |a, b| a.saturating_add(b));
}

fn encoded(program: &[Op], only: Option<usize>, enc: Encoding) -> Program {
    return program
        .iter()
        .enumerate()
        .map(|(i, op)| {
            if op.kind.carries_data() && only.map_or(true, |k| k == i) {
                return Op {
                    kind: enc.0,
                    dst: op.dst.clone(),
                    src: Vec::new(),
                    level: enc.1,
                };
            }
            return op.clone();
        })
        .collect();
}

const SPARSE_RAW: Encoding = (Kind::Sparse, None);

/// Most bytes one candidate's analysis may retain: the packed chunk modes of its data operations, the spans of
/// its repair, and the encodings its trials have kept. Each is charged by the capacity of the buffer that holds it,
/// as [`heap_bytes`] describes. A candidate that needs more is evaluated by replay.
pub const ANALYSIS_BYTES: usize = 8 << 20;
const ALLOCATION_OVERHEAD: usize = 32;

fn packed_modes(mapped: &Mapped) -> Vec<Vec<u8>> {
    return mapped
        .ops
        .iter()
        .filter(|o| o.r#type == TYPE_REPLACE_SPARSE)
        .map(|o| o.frame_table.clone().unwrap_or_default())
        .collect();
}

fn heap_bytes(capacity: usize, size: usize) -> Option<usize> {
    if capacity == 0 {
        return Some(0);
    }
    return capacity.checked_mul(size)?.checked_add(ALLOCATION_OVERHEAD);
}

fn table_bytes(stored: &Coded) -> Option<usize> {
    return heap_bytes(stored.table.capacity(), size_of::<u8>());
}

struct Prepared {
    modes: Vec<Vec<u8>>,
    data_ops: usize,
    mismatches: Vec<Span>,
    coded: RefCell<Vec<Vec<Entry>>>,
    retained: Cell<usize>,
    budget: usize,
}

impl Prepared {
    fn new(
        problem: &Problem,
        source: &Program,
        budget: usize,
        ctl: &Control,
    ) -> Step<Option<Prepared>> {
        let data_ops = source.iter().filter(|o| o.kind.carries_data()).count();
        let first = match problem
            .mapping
            .map(&encoded(source, None, SPARSE_RAW), ctl, false)
        {
            Err(Halt::Failed(_)) => return Ok(None),
            Err(Halt::Deadline) => return Err(Halt::Deadline),
            Ok(m) => m,
        };
        let (modes, mismatches) = if first.mismatches.is_empty() {
            (packed_modes(&first), Vec::new())
        } else {
            let repair = heap_bytes(first.mismatches.capacity(), size_of::<Span>());
            if repair.map_or(true, |bytes| bytes > budget) {
                return Ok(None);
            }
            let mut completed = source.clone();
            completed.push(Op::new(Kind::Replace, first.mismatches.clone()));
            let second =
                match problem
                    .mapping
                    .map(&encoded(&completed, None, SPARSE_RAW), ctl, false)
                {
                    Err(Halt::Failed(_)) => return Ok(None),
                    Err(Halt::Deadline) => return Err(Halt::Deadline),
                    Ok(m) => m,
                };
            if !second.mismatches.is_empty() {
                return Ok(None);
            }
            (packed_modes(&second), first.mismatches)
        };
        let prepared = Prepared {
            modes: modes,
            data_ops: data_ops,
            mismatches: mismatches,
            coded: RefCell::new((0..=data_ops).map(|_| Vec::new()).collect()),
            retained: Cell::new(0),
            budget: budget,
        };
        if !prepared.charge(prepared.base_bytes()) {
            return Ok(None);
        }
        return Ok(Some(prepared));
    }

    fn base_bytes(&self) -> Option<usize> {
        let mut total = heap_bytes(self.modes.capacity(), size_of::<Vec<u8>>())?;
        for buffer in &self.modes {
            total = total.checked_add(heap_bytes(buffer.capacity(), size_of::<u8>())?)?;
        }
        total = total.checked_add(heap_bytes(self.mismatches.capacity(), size_of::<Span>())?)?;
        let coded = self.coded.borrow();
        return total.checked_add(heap_bytes(coded.capacity(), size_of::<Vec<Entry>>())?);
    }

    fn charge(&self, bytes: Option<usize>) -> bool {
        let total = match bytes.and_then(|b| self.retained.get().checked_add(b)) {
            None => return false,
            Some(t) => t,
        };
        if total > self.budget {
            return false;
        }
        self.retained.set(total);
        return true;
    }

    fn covers(&self, program: &Program) -> bool {
        let data = program.iter().filter(|o| o.kind.carries_data()).count();
        if data == self.data_ops {
            return true;
        }
        return data == self.data_ops + 1
            && !self.mismatches.is_empty()
            && program
                .last()
                .map_or(false, |o| o.kind.carries_data() && o.dst == self.mismatches);
    }

    fn coded(&self, problem: &Problem, index: usize, op: &Op, ctl: &Control) -> Step<Coded> {
        let key = (op.kind, op.level);
        if let Some(found) = self.cached(index, key) {
            return Ok(found);
        }
        let modes = &self.modes[index];
        let sparse = op.kind == Kind::Sparse;
        let coded = problem.mapping.encode(op, ctl, |k, _| {
            return Ok(if sparse {
                packed_mode(modes, k)
            } else {
                Chunk::Content
            });
        })?;
        self.retain(index, key, &coded);
        return Ok(coded);
    }

    fn cached(&self, index: usize, key: Encoding) -> Option<Coded> {
        let outer = self.coded.borrow();
        return outer
            .get(index)?
            .iter()
            .find(|(k, _)| *k == key)
            .map(|(_, found)| found.clone());
    }

    fn retain(&self, index: usize, key: Encoding, coded: &Coded) -> bool {
        let mut outer = self.coded.borrow_mut();
        let entries = match outer.get_mut(index) {
            None => return false,
            Some(entries) => entries,
        };
        let buffer: Option<Vec<Entry>> = if entries.capacity() == 0 {
            Some(Vec::with_capacity(ENCODINGS.len()))
        } else {
            None
        };
        if buffer.is_none() && entries.len() == entries.capacity() {
            return false;
        }
        let stored = coded.clone();
        let buffer_bytes = match &buffer {
            None => Some(0),
            Some(fresh) => heap_bytes(fresh.capacity(), size_of::<Entry>()),
        };
        let bytes = buffer_bytes
            .zip(table_bytes(&stored))
            .and_then(|(a, b)| a.checked_add(b));
        if !self.charge(bytes) {
            return false;
        }
        if let Some(fresh) = buffer {
            *entries = fresh;
        }
        entries.push((key, stored));
        return true;
    }

    fn manifest(
        &self,
        problem: &Problem,
        program: &Program,
        ctl: &Control,
    ) -> Step<Option<(Program, Manifest, u64)>> {
        if !within_count_limits(problem.profile, program) {
            return Ok(None);
        }
        let mut out = program.clone();
        let mut ops: Vec<Operation> = Vec::with_capacity(program.len() + 1);
        let mut cursor: u64 = 0;
        let mut index = 0usize;
        for op in program {
            ctl.checkpoint()?;
            let (operation, data_len) = match op.kind {
                Kind::Zero => (zero_wire(op, cursor), 0),
                Kind::SourceCopy | Kind::TargetCopy => {
                    (copy_wire(op, cursor, Some(placeholder())), 0)
                }
                Kind::Replace => {
                    let len = match total(&op.dst) {
                        None => return Ok(None),
                        Some(t) => t,
                    };
                    (raw_replace_wire(&op.dst, cursor, len, placeholder()), len)
                }
                Kind::Frames | Kind::Sparse => {
                    let coded = self.coded(problem, index, op, ctl)?;
                    let hash = if coded.data_len == 0 {
                        sha256::hash_bytes(&[]).to_vec()
                    } else {
                        placeholder()
                    };
                    let kind = if op.kind == Kind::Frames {
                        TYPE_REPLACE_FRAMES
                    } else {
                        TYPE_REPLACE_SPARSE
                    };
                    (
                        wire(
                            kind,
                            &op.dst,
                            &[],
                            cursor,
                            coded.data_len,
                            Some(hash),
                            None,
                            Some(coded.table),
                        ),
                        coded.data_len,
                    )
                }
            };
            if op.kind.carries_data() {
                index += 1;
            }
            ops.push(operation);
            cursor += data_len;
        }
        if !self.mismatches.is_empty() && index == self.data_ops {
            let len = match total(&self.mismatches) {
                None => return Ok(None),
                Some(t) => t,
            };
            out.push(Op::new(Kind::Replace, self.mismatches.clone()));
            ops.push(raw_replace_wire(
                &self.mismatches,
                cursor,
                len,
                placeholder(),
            ));
            cursor += len;
        }
        return Ok(Some((out, manifest_for(problem, ops, cursor), cursor)));
    }

    fn evaluate(
        &self,
        problem: &Problem,
        program: &Program,
        ctl: &Control,
    ) -> Step<Option<Evaluated>> {
        let (shipped, manifest, blob_len) = match self.manifest(problem, program, ctl)? {
            None => return Ok(None),
            Some(found) => found,
        };
        return Ok(match payload_size(problem, manifest, blob_len) {
            Err(_) => None,
            Ok(size) => Some(Evaluated {
                program: shipped,
                size: size,
            }),
        });
    }
}

struct Trials<'s> {
    source: &'s Program,
    budget: usize,
    analysis: Option<Option<Prepared>>,
}

impl<'s> Trials<'s> {
    fn new(source: &'s Program, budget: usize) -> Trials<'s> {
        return Trials {
            source: source,
            budget: budget,
            analysis: None,
        };
    }

    fn evaluate(
        &mut self,
        problem: &Problem,
        program: Program,
        ctl: &Control,
    ) -> Step<Option<Evaluated>> {
        if !within_count_limits(problem.profile, &program) {
            return Ok(None);
        }
        if self.analysis.is_none() {
            self.analysis = Some(Prepared::new(problem, self.source, self.budget, ctl)?);
        }
        if let Some(Some(prepared)) = &self.analysis {
            if prepared.covers(&program) {
                return prepared.evaluate(problem, &program, ctl);
            }
        }
        return evaluate(problem, program, ctl);
    }
}

/// Choose the smallest admissible program among `candidates` and their encodings. `None` when the
/// profile admits no candidate in any encoding.
pub fn optimize(
    problem: &Problem,
    candidates: Vec<Program>,
    ctl: &Control,
) -> Step<Option<Outcome>> {
    return optimize_with(problem, candidates, ctl, ANALYSIS_BYTES);
}

pub(crate) fn optimize_with(
    problem: &Problem,
    candidates: Vec<Program>,
    ctl: &Control,
    budget: usize,
) -> Step<Option<Outcome>> {
    let mut best: Option<Evaluated> = None;
    let mut raw_best: Option<u64> = None;
    let mut admitted_raw = 0usize;
    let mut refused_raw = 0usize;
    let mut trials = 0usize;
    let mut work = 0u64;
    let mut seen: HashSet<Program> = HashSet::new();
    let chunk = problem.mapping.chunk() as u64;
    let maps_before = problem.mapping.maps();
    for (index, raw) in candidates.into_iter().enumerate() {
        if !seen.insert(raw.clone()) {
            continue;
        }
        let base = evaluate(problem, raw.clone(), ctl)?;
        trials += 1;
        work = work.saturating_add(mapping_cost(&raw, chunk));
        let mut local = base;
        match &local {
            Some(e) => {
                admitted_raw += 1;
                raw_best = Some(raw_best.map_or(e.size, |b| b.min(e.size)));
            }
            None => refused_raw += 1,
        }
        let source: Program = match &local {
            Some(e) => e.program.clone(),
            None => raw.clone(),
        };
        if source.iter().any(|o| o.kind.carries_data()) {
            let cost = data_cost(&source, chunk);
            let mut reuse = Trials::new(&source, budget);
            for (n, enc) in ENCODINGS.iter().take(ENCODINGS.len() - 1).enumerate() {
                let unconditional = index < FREE_CANDIDATES && n == 0;
                if !unconditional && work >= WORK_BUDGET {
                    break;
                }
                work = work.saturating_add(cost);
                trials += 1;
                if let Some(e) = reuse.evaluate(problem, encoded(&source, None, *enc), ctl)? {
                    if local.as_ref().map_or(true, |l| e.size < l.size) {
                        local = Some(e);
                    }
                }
            }
            if let Some(start) = local.clone() {
                let mut current = start;
                let mut order: Vec<usize> = (0..current.program.len())
                    .filter(|i| current.program[*i].kind.carries_data())
                    .collect();
                order.sort_by_key(|i| {
                    std::cmp::Reverse(total(&current.program[*i].dst).unwrap_or(0))
                });
                order.truncate(REFINED_OPS);
                if order.len() > 1 {
                    for i in order {
                        for enc in ENCODINGS {
                            if work >= WORK_BUDGET {
                                break;
                            }
                            if current.program[i].kind == enc.0 && current.program[i].level == enc.1
                            {
                                continue;
                            }
                            work = work
                                .saturating_add(mapping_cost(&current.program, chunk))
                                .saturating_add(op_cost(&current.program[i], chunk));
                            trials += 1;
                            let variant = encoded(&current.program, Some(i), enc);
                            if let Some(e) = reuse.evaluate(problem, variant, ctl)? {
                                if e.size < current.size {
                                    current = e;
                                }
                            }
                        }
                    }
                }
                local = Some(current);
            }
        }
        if let Some(e) = local {
            if best.as_ref().map_or(true, |b| e.size < b.size) {
                best = Some(e);
            }
        }
    }
    return Ok(best.map(|b| Outcome {
        best: b,
        admitted_raw: admitted_raw,
        refused_raw: refused_raw,
        trials: trials,
        mappings: problem.mapping.maps() - maps_before,
        raw_best: raw_best,
    }));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::control::Halt;
    use crate::control::test_clocks::Ticking;
    use crate::program::Span;
    use crate::program::raw_baseline;
    use delta_update_payload::profile::EMBEDDED;
    use delta_update_payload::profile::HOST;
    use delta_update_payload::profile::ResourceProfile;
    use delta_update_payload::sha256;
    use prost::Message;

    fn s(start: u64, len: u64) -> Span {
        return Span {
            start: start,
            len: len,
        };
    }

    fn problem<'a>(
        old: &'a [u8],
        new: &'a [u8],
        profile: &'a ResourceProfile,
        chunk: usize,
    ) -> Problem<'a> {
        return Problem::new(old, new, chunk, "A/B", profile, 256)
            .unwrap_or_else(|e| panic!("{}", e.msg));
    }

    fn optimized(p: &Problem, candidates: Vec<Program>) -> Outcome {
        return match optimize(p, candidates, &Control::unbounded()) {
            Ok(Some(o)) => o,
            Ok(None) => panic!("no candidate admitted"),
            Err(_) => panic!("an unbounded pass cannot time out"),
        };
    }

    fn text(n: usize) -> Vec<u8> {
        return (0..n).map(|k| b"the quick brown fox "[k % 20]).collect();
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

    #[test]
    fn a_compressible_replacement_is_framed_or_sparse_and_smaller_than_raw() {
        let old = vec![0u8; 8192];
        let new = text(8192);
        let p = problem(&old, &new, &HOST, 64);
        let raw = optimized(&p, vec![raw_baseline(8192)]);
        let raw_size = raw.raw_best.unwrap_or(0);
        assert!(
            raw.best.size < raw_size,
            "{} against raw {}",
            raw.best.size,
            raw_size
        );
        assert!(
            raw.best.program.iter().all(|o| o.kind != Kind::Replace),
            "a framed or sparse encoding was chosen"
        );
    }

    #[test]
    fn incompressible_data_stays_raw_when_framing_costs_more() {
        let old = vec![0u8; 4096];
        let new = noise(4096, 3);
        let p = problem(&old, &new, &HOST, 64);
        let out = optimized(&p, vec![raw_baseline(4096)]);
        assert_eq!(out.best.size, out.raw_best.unwrap_or(0));
        assert_eq!(out.best.program, raw_baseline(4096));
    }

    #[test]
    fn a_sparse_encoding_keeps_unchanged_chunks_that_the_raw_baseline_rewrote() {
        let old = noise(8192, 5);
        let mut new = old.clone();
        new[4096..4160].copy_from_slice(&noise(64, 6));
        let p = problem(&old, &new, &HOST, 64);
        let out = optimized(&p, vec![raw_baseline(8192)]);
        assert!(
            out.best.size < out.raw_best.unwrap_or(0) - 7000,
            "{}",
            out.best.size
        );
    }

    #[test]
    fn the_result_is_never_larger_than_the_smallest_admissible_raw_candidate() {
        let old = noise(4096, 8);
        let mut new = old.clone();
        new[100..600].fill(0);
        new[3000..3100].copy_from_slice(&noise(100, 9));
        let p = problem(&old, &new, &HOST, 64);
        let candidates = vec![
            raw_baseline(4096),
            vec![
                Op::new(Kind::Zero, vec![s(100, 500)]),
                Op::new(Kind::Replace, vec![s(3000, 100)]),
            ],
            Vec::new(),
        ];
        let out = optimized(&p, candidates);
        let raw = out.raw_best.unwrap_or(u64::MAX);
        assert!(out.best.size <= raw, "{} against {}", out.best.size, raw);
        assert_eq!(out.admitted_raw, 3);
    }

    #[test]
    fn an_encoding_rescues_a_raw_candidate_the_manifest_limit_refuses() {
        let len = 2_097_153;
        let old = vec![1u8; len];
        let new = vec![0u8; len];
        let tight = ResourceProfile {
            max_encoded_manifest: 161,
            ..HOST
        };
        let p = problem(&old, &new, &tight, len);
        let ctl = Control::unbounded();
        let raw = raw_baseline(len as u64);
        assert!(
            settled(evaluate(&p, raw.clone(), &ctl)).is_none(),
            "the raw candidate must be refused"
        );
        let mapped = settled(p.mapping.map(&raw, &ctl, false));
        let raw_manifest = manifest_for(&p, mapped.ops, mapped.blob_len)
            .encode_to_vec()
            .len();
        assert!(
            raw_manifest > 158,
            "the raw manifest is {} bytes",
            raw_manifest
        );
        let out = optimized(&p, vec![raw]);
        assert_eq!((out.admitted_raw, out.refused_raw), (0, 1));
        assert!(
            out.best
                .program
                .iter()
                .any(|o| matches!(o.kind, Kind::Sparse | Kind::Frames)),
            "{:?}",
            out.best.program
        );
        let again = settled(evaluate(&p, out.best.program.clone(), &ctl));
        assert_eq!(
            again.map(|e| e.size),
            Some(out.best.size),
            "the returned program is admitted at its size"
        );
    }

    #[test]
    fn programs_the_profile_never_admits_yield_nothing() {
        let old = vec![0u8; 64];
        let new = text(64);
        let none = ResourceProfile {
            max_operations: 0,
            ..EMBEDDED
        };
        let p = problem(&old, &new, &none, 16);
        assert!(matches!(
            optimize(
                &p,
                vec![raw_baseline(64), Vec::new()],
                &Control::unbounded()
            ),
            Ok(None)
        ));
    }

    #[test]
    fn encodings_leave_copies_and_zeros_and_the_operation_order_alone() {
        let program = vec![
            Op::copy(Kind::SourceCopy, vec![s(0, 4)], vec![s(8, 4)]),
            Op::new(Kind::Replace, vec![s(10, 6)]),
            Op::new(Kind::Zero, vec![s(20, 3)]),
        ];
        let out = encoded(&program, None, (Kind::Sparse, Some(9)));
        assert_eq!(out[0], program[0]);
        assert_eq!(out[2], program[2]);
        assert_eq!(out[1].kind, Kind::Sparse);
        assert_eq!(out[1].level, Some(9));
        assert_eq!(out[1].dst, program[1].dst);
        let one = encoded(&program, Some(0), RAW);
        assert_eq!(
            one, program,
            "an index that is not a data operation changes nothing"
        );
    }

    #[test]
    fn identical_candidates_are_tried_once() {
        let old = vec![0u8; 512];
        let new = text(512);
        let p = problem(&old, &new, &HOST, 64);
        let once = optimized(&p, vec![raw_baseline(512)]);
        let twice = optimized(&p, vec![raw_baseline(512), raw_baseline(512)]);
        assert_eq!(once.trials, twice.trials);
    }

    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            return self.0;
        }

        fn below(&mut self, n: u64) -> u64 {
            return self.next() % n;
        }
    }

    fn small_alphabet(r: &mut Rng, len: usize) -> Vec<u8> {
        let mut out: Vec<u8> = Vec::with_capacity(len);
        for _ in 0..len {
            out.push([0u8, 0, 0, 1, 2][r.below(5) as usize]);
        }
        return out;
    }

    fn random_span(r: &mut Rng, len: u64) -> Span {
        let start = r.below(len);
        return Span {
            start: start,
            len: 1 + r.below(len - start),
        };
    }

    fn random_program(r: &mut Rng, old_len: u64, new_len: u64) -> Program {
        let mut program: Program = Vec::new();
        for _ in 0..1 + r.below(4) {
            let mut dst: Vec<Span> = Vec::new();
            for _ in 0..1 + r.below(3) {
                dst.push(random_span(r, new_len));
            }
            let want = total(&dst).unwrap_or(0);
            let kind = match r.below(6) {
                0 => Kind::Zero,
                1 => Kind::SourceCopy,
                2 => Kind::TargetCopy,
                _ => Kind::Replace,
            };
            let image = if kind == Kind::SourceCopy {
                old_len
            } else {
                new_len
            };
            if kind.is_copy() {
                if image >= want {
                    let at = r.below(image - want + 1);
                    program.push(Op::copy(
                        kind,
                        dst,
                        vec![Span {
                            start: at,
                            len: want,
                        }],
                    ));
                } else {
                    program.push(Op::new(Kind::Replace, dst));
                }
            } else {
                program.push(Op::new(kind, dst));
            }
        }
        return program;
    }

    fn randomly_encoded(r: &mut Rng, program: &Program) -> Program {
        let mut out = program.clone();
        for i in 0..out.len() {
            if out[i].kind.carries_data() {
                let enc = ENCODINGS[r.below(ENCODINGS.len() as u64) as usize];
                out = encoded(&out, Some(i), enc);
            }
        }
        return out;
    }

    fn settled<T>(step: Step<T>) -> T {
        return match step {
            Ok(v) => v,
            Err(_) => panic!("an unbounded evaluation cannot time out"),
        };
    }

    fn decisions(out: &Option<Outcome>) -> Option<(Evaluated, usize, usize, usize, Option<u64>)> {
        return out.as_ref().map(|o| {
            (
                o.best.clone(),
                o.trials,
                o.admitted_raw,
                o.refused_raw,
                o.raw_best,
            )
        });
    }

    #[test]
    fn reused_trials_decide_exactly_as_ordinary_evaluations() {
        let tight = ResourceProfile {
            max_operations: 3,
            max_encoded_manifest: 300,
            ..HOST
        };
        let profiles = [&HOST, &EMBEDDED, &tight];
        let mut r = Rng(0x2545_F491_4F6C_DD1D);
        let (mut reused, mut replayed) = (0u64, 0u64);
        for case in 0..200 {
            let old_len = r.below(160);
            let new_len = 1 + r.below(200);
            let old = small_alphabet(&mut r, old_len as usize);
            let new = small_alphabet(&mut r, new_len as usize);
            let profile = profiles[r.below(3) as usize];
            let chunk = [1usize, 2, 3, 4, 7, 16, 64][r.below(7) as usize];
            let p = problem(&old, &new, profile, chunk);
            let candidates = vec![
                random_program(&mut r, old_len, new_len),
                raw_baseline(new_len),
            ];
            let fast = optimize_with(
                &p,
                candidates.clone(),
                &Control::unbounded(),
                ANALYSIS_BYTES,
            );
            let fast = settled(fast);
            let slow = settled(optimize_with(&p, candidates, &Control::unbounded(), 0));
            assert_eq!(decisions(&fast), decisions(&slow), "case {}", case);
            reused += fast.as_ref().map_or(0, |o| o.mappings);
            replayed += slow.as_ref().map_or(0, |o| o.mappings);
        }
        assert!(
            reused < replayed,
            "reuse made {} whole-program replays, replay {}",
            reused,
            replayed
        );
    }

    #[test]
    fn kept_and_recomputed_encodings_decide_exactly_alike_at_every_budget() {
        let mut r = Rng(0xD1B5_4A32_D192_ED03);
        let budgets = [1usize, 300, 1 << 10, 1 << 13, ANALYSIS_BYTES];
        for case in 0..80 {
            let old_len = r.below(160);
            let new_len = 1 + r.below(200);
            let old = small_alphabet(&mut r, old_len as usize);
            let new = small_alphabet(&mut r, new_len as usize);
            let profile = [&HOST, &EMBEDDED][r.below(2) as usize];
            let chunk = [1usize, 2, 4, 16, 64][r.below(5) as usize];
            let p = problem(&old, &new, profile, chunk);
            let candidates = vec![
                random_program(&mut r, old_len, new_len),
                raw_baseline(new_len),
            ];
            let slow = settled(optimize_with(
                &p,
                candidates.clone(),
                &Control::unbounded(),
                0,
            ));
            for budget in budgets {
                let fast = settled(optimize_with(
                    &p,
                    candidates.clone(),
                    &Control::unbounded(),
                    budget,
                ));
                assert_eq!(
                    decisions(&fast),
                    decisions(&slow),
                    "case {}, budget {}",
                    case,
                    budget
                );
            }
        }
    }

    #[test]
    fn analysed_trials_match_ordinary_evaluations_on_every_variant() {
        let mut r = Rng(0x9E37_79B9_7F4A_7C15);
        let mut compared = 0usize;
        for case in 0..150 {
            let old_len = r.below(120);
            let new_len = 1 + r.below(160);
            let old = small_alphabet(&mut r, old_len as usize);
            let new = small_alphabet(&mut r, new_len as usize);
            let chunk = [1usize, 3, 4, 8, 64][r.below(5) as usize];
            let p = problem(&old, &new, &HOST, chunk);
            let ctl = Control::unbounded();
            let raw = random_program(&mut r, old_len, new_len);
            let mut sources: Vec<Program> = vec![raw.clone()];
            if let Some(e) = settled(evaluate(&p, raw, &ctl)) {
                sources.push(e.program);
            }
            for source in sources {
                let prepared = match settled(Prepared::new(&p, &source, ANALYSIS_BYTES, &ctl)) {
                    Some(prepared) => prepared,
                    None => continue,
                };
                let mut variants: Vec<Program> = vec![
                    randomly_encoded(&mut r, &source),
                    randomly_encoded(&mut r, &source),
                ];
                for enc in ENCODINGS {
                    variants.push(encoded(&source, None, enc));
                }
                if let Some(e) = settled(evaluate(&p, encoded(&source, None, SPARSE_RAW), &ctl)) {
                    variants.push(randomly_encoded(&mut r, &e.program));
                }
                for variant in variants {
                    if !prepared.covers(&variant) {
                        continue;
                    }
                    compared += 1;
                    let analysed = settled(prepared.evaluate(&p, &variant, &ctl));
                    let ordinary = settled(evaluate(&p, variant.clone(), &ctl));
                    assert_eq!(analysed, ordinary, "case {}: {:?}", case, variant);
                }
            }
        }
        assert!(compared > 1000, "only {} variants were compared", compared);
    }

    #[test]
    fn the_analysis_refuses_a_candidate_past_its_bound() {
        let old = vec![0u8; 64];
        let new = text(64);
        let p = problem(&old, &new, &HOST, 4);
        let ctl = Control::unbounded();
        let source = raw_baseline(64);
        assert!(settled(Prepared::new(&p, &source, 0, &ctl)).is_none());
        assert!(settled(Prepared::new(&p, &source, ANALYSIS_BYTES, &ctl)).is_some());
    }

    #[test]
    fn a_program_the_profile_cannot_count_is_refused_before_any_mapping_or_analysis() {
        let old = vec![0u8; 64];
        let new = text(64);
        let source = vec![Op::new(Kind::Replace, vec![s(0, 32)])];
        let no_operations = ResourceProfile {
            max_operations: 0,
            ..HOST
        };
        let no_extents = ResourceProfile {
            max_total_extents: 0,
            ..HOST
        };
        let clock = Ticking::new(1.0);
        let expired = Control::new(&clock, 0.5, 1.0, 0, None);
        for profile in [&no_operations, &no_extents] {
            let p = problem(&old, &new, profile, 8);
            for ctl in [&Control::unbounded(), &expired] {
                let before = p.mapping.maps();
                let mut trials = Trials::new(&source, ANALYSIS_BYTES);
                let trial =
                    trials.evaluate(&p, encoded(&source, None, (Kind::Sparse, Some(9))), ctl);
                assert!(
                    matches!(trial, Ok(None)),
                    "a refused trial is a refusal, not a deadline"
                );
                assert!(
                    trials.analysis.is_none(),
                    "an analysis was made for a program the profile cannot count"
                );
                assert!(matches!(evaluate(&p, source.clone(), ctl), Ok(None)));
                assert!(matches!(
                    optimize_with(&p, vec![source.clone()], ctl, ANALYSIS_BYTES),
                    Ok(None)
                ));
                assert_eq!(
                    p.mapping.maps(),
                    before,
                    "a program the profile cannot count was mapped"
                );
            }
        }
    }

    fn empty_analysis(budget: usize) -> Prepared {
        return Prepared {
            modes: Vec::new(),
            data_ops: 0,
            mismatches: Vec::new(),
            coded: RefCell::new(Vec::new()),
            retained: Cell::new(0),
            budget: budget,
        };
    }

    #[test]
    fn a_repair_too_large_to_retain_is_refused_after_one_replay() {
        let old = vec![0u8; 2 << 20];
        let new: Vec<u8> = (0..2usize << 20).map(|k| (k & 1) as u8).collect();
        let p = problem(&old, &new, &HOST, 4096);
        let ctl = Control::unbounded();
        let source: Program = Vec::new();
        assert_eq!(
            settled(p.mapping.map(&source, &ctl, false))
                .mismatches
                .len(),
            1 << 20
        );
        let before = p.mapping.maps();
        assert!(settled(Prepared::new(&p, &source, ANALYSIS_BYTES, &ctl)).is_none());
        assert_eq!(
            p.mapping.maps() - before,
            1,
            "the repair was replayed again before it was refused"
        );
    }

    #[test]
    fn the_analysis_budget_is_exact_at_its_boundary() {
        let old = vec![0u8; 64];
        let new = text(64);
        let p = problem(&old, &new, &HOST, 4);
        let ctl = Control::unbounded();
        for source in [raw_baseline(64), Vec::new()] {
            let need = settled(Prepared::new(&p, &source, ANALYSIS_BYTES, &ctl))
                .unwrap_or_else(|| panic!("analysed"))
                .retained
                .get();
            assert!(
                settled(Prepared::new(&p, &source, need, &ctl)).is_some(),
                "{:?}",
                source
            );
            assert!(
                settled(Prepared::new(&p, &source, need - 1, &ctl)).is_none(),
                "{:?}",
                source
            );
        }
    }

    #[test]
    fn a_cached_encoding_is_kept_only_while_it_fits_the_budget() {
        let old = vec![0u8; 256];
        let new = text(256);
        let p = problem(&old, &new, &HOST, 16);
        let source = raw_baseline(256);
        let ctl = Control::unbounded();
        let variant = encoded(&source, None, (Kind::Sparse, Some(9)));
        let want = settled(evaluate(&p, variant.clone(), &ctl));
        let roomy = settled(Prepared::new(&p, &source, ANALYSIS_BYTES, &ctl))
            .unwrap_or_else(|| panic!("analysed"));
        let base = roomy.retained.get();
        assert_eq!(settled(roomy.evaluate(&p, &variant, &ctl)), want);
        let entry = roomy.retained.get() - base;
        assert!(entry > 0, "the variant's encoding is not cached");
        let short = settled(Prepared::new(&p, &source, base + entry - 1, &ctl))
            .unwrap_or_else(|| panic!("analysed"));
        assert_eq!(settled(short.evaluate(&p, &variant, &ctl)), want);
        assert!(short.coded.borrow().iter().all(Vec::is_empty));
        assert_eq!(short.retained.get(), base);
        let exact = settled(Prepared::new(&p, &source, base + entry, &ctl))
            .unwrap_or_else(|| panic!("analysed"));
        assert_eq!(settled(exact.evaluate(&p, &variant, &ctl)), want);
        assert_eq!(exact.coded.borrow()[0].len(), 1);
        assert_eq!(exact.retained.get(), base + entry);
    }

    fn table(len: usize) -> Coded {
        return Coded {
            data_len: len as u64,
            table: vec![0xA5; len],
        };
    }

    fn expected_charge(capacity: usize, size: usize) -> usize {
        return if capacity == 0 {
            0
        } else {
            capacity * size + ALLOCATION_OVERHEAD
        };
    }

    fn snapshot(prepared: &Prepared) -> (usize, Vec<(usize, usize, Vec<usize>)>) {
        let outer = prepared.coded.borrow();
        let ops = outer
            .iter()
            .map(|entries| {
                let tables = entries
                    .iter()
                    .map(|(_, kept)| kept.table.capacity())
                    .collect();
                return (entries.len(), entries.capacity(), tables);
            })
            .collect();
        return (prepared.retained.get(), ops);
    }

    #[test]
    fn the_first_kept_encoding_is_charged_its_entry_buffer_and_its_table() {
        let old = vec![0u8; 256];
        let new = text(256);
        let p = problem(&old, &new, &HOST, 16);
        let ctl = Control::unbounded();
        let source = raw_baseline(256);
        let prepared = settled(Prepared::new(&p, &source, ANALYSIS_BYTES, &ctl))
            .unwrap_or_else(|| panic!("analysed"));
        let before = prepared.retained.get();
        assert!(prepared.retain(0, (Kind::Sparse, Some(9)), &table(40)));
        let outer = prepared.coded.borrow();
        let entries = outer[0].capacity();
        let kept = outer[0][0].1.table.capacity();
        assert!(entries >= ENCODINGS.len(), "room for every encoding");
        assert_eq!(kept, 40);
        assert_eq!(
            prepared.retained.get() - before,
            expected_charge(entries, size_of::<Entry>()) + expected_charge(kept, size_of::<u8>())
        );
    }

    #[test]
    fn the_analysis_charges_an_entry_slot_for_each_data_operation_and_the_repair() {
        let old = vec![0u8; 256];
        let new = text(256);
        let p = problem(&old, &new, &HOST, 16);
        let ctl = Control::unbounded();
        let source = vec![
            Op::new(Kind::Replace, vec![s(0, 128)]),
            Op::new(Kind::Replace, vec![s(128, 128)]),
        ];
        let prepared = settled(Prepared::new(&p, &source, ANALYSIS_BYTES, &ctl))
            .unwrap_or_else(|| panic!("analysed"));
        let outer = prepared.coded.borrow();
        assert_eq!(
            outer.len(),
            3,
            "one slot per data operation, and one for a repair"
        );
        let modes: usize = prepared
            .modes
            .iter()
            .map(|m| expected_charge(m.capacity(), 1))
            .sum();
        let want = expected_charge(prepared.modes.capacity(), size_of::<Vec<u8>>())
            + modes
            + expected_charge(prepared.mismatches.capacity(), size_of::<Span>())
            + expected_charge(outer.capacity(), size_of::<Vec<Entry>>());
        assert_eq!(prepared.retained.get(), want);
    }

    #[test]
    fn an_empty_table_owns_no_buffer_and_costs_only_the_entry_buffer() {
        let old = vec![0u8; 256];
        let new = text(256);
        let p = problem(&old, &new, &HOST, 16);
        let ctl = Control::unbounded();
        let prepared = settled(Prepared::new(&p, &raw_baseline(256), ANALYSIS_BYTES, &ctl))
            .unwrap_or_else(|| panic!("analysed"));
        let before = prepared.retained.get();
        assert!(prepared.retain(0, (Kind::Sparse, Some(9)), &table(0)));
        let outer = prepared.coded.borrow();
        assert_eq!(outer[0][0].1.table.capacity(), 0);
        assert_eq!(
            prepared.retained.get() - before,
            expected_charge(outer[0].capacity(), size_of::<Entry>())
        );
    }

    #[test]
    fn later_encodings_of_an_operation_are_charged_only_their_tables() {
        let old = vec![0u8; 256];
        let new = text(256);
        let p = problem(&old, &new, &HOST, 16);
        let ctl = Control::unbounded();
        let prepared = settled(Prepared::new(&p, &raw_baseline(256), ANALYSIS_BYTES, &ctl))
            .unwrap_or_else(|| panic!("analysed"));
        assert!(prepared.retain(0, (Kind::Sparse, Some(9)), &table(40)));
        let after_first = prepared.retained.get();
        let capacity = prepared.coded.borrow()[0].capacity();
        assert!(prepared.retain(0, (Kind::Sparse, Some(1)), &table(100)));
        let outer = prepared.coded.borrow();
        assert_eq!(
            outer[0].capacity(),
            capacity,
            "the entries were not regrown"
        );
        assert_eq!(
            prepared.retained.get() - after_first,
            expected_charge(outer[0][1].1.table.capacity(), 1)
        );
    }

    #[test]
    fn a_full_operation_refuses_another_encoding_and_keeps_its_entries() {
        let old = vec![0u8; 256];
        let new = text(256);
        let p = problem(&old, &new, &HOST, 16);
        let ctl = Control::unbounded();
        let prepared = settled(Prepared::new(&p, &raw_baseline(256), ANALYSIS_BYTES, &ctl))
            .unwrap_or_else(|| panic!("analysed"));
        let mut kept = 0usize;
        for level in 0..=u8::MAX {
            if !prepared.retain(0, (Kind::Frames, Some(level)), &table(8)) {
                break;
            }
            kept += 1;
        }
        let capacity = prepared.coded.borrow()[0].capacity();
        assert_eq!(kept, capacity, "the entries kept are the buffer's room");
        assert!(kept >= ENCODINGS.len());
        let before = snapshot(&prepared);
        assert!(!prepared.retain(0, (Kind::Frames, Some(200)), &table(8)));
        assert_eq!(snapshot(&prepared), before);
        assert!(
            prepared.cached(0, (Kind::Frames, Some(0))).is_some(),
            "an existing entry was dropped"
        );
    }

    #[test]
    fn operations_and_levels_are_kept_apart_and_each_is_charged() {
        let old = vec![0u8; 256];
        let new = text(256);
        let p = problem(&old, &new, &HOST, 16);
        let ctl = Control::unbounded();
        let source = vec![
            Op::new(Kind::Replace, vec![s(0, 128)]),
            Op::new(Kind::Replace, vec![s(128, 128)]),
        ];
        let prepared = settled(Prepared::new(&p, &source, ANALYSIS_BYTES, &ctl))
            .unwrap_or_else(|| panic!("analysed"));
        let before = prepared.retained.get();
        assert!(prepared.retain(0, (Kind::Sparse, Some(9)), &table(40)));
        assert!(prepared.retain(0, (Kind::Sparse, Some(1)), &table(50)));
        assert!(prepared.retain(1, (Kind::Frames, Some(9)), &table(60)));
        let len = |o: usize, k: Encoding| prepared.cached(o, k).map(|c| c.table.len());
        assert_eq!(len(0, (Kind::Sparse, Some(9))), Some(40));
        assert_eq!(len(0, (Kind::Sparse, Some(1))), Some(50));
        assert_eq!(len(1, (Kind::Frames, Some(9))), Some(60));
        assert_eq!(
            len(1, (Kind::Sparse, Some(9))),
            None,
            "an encoding is kept for its own operation only"
        );
        assert_eq!(
            len(7, (Kind::Frames, Some(9))),
            None,
            "an index past the operations has no entries"
        );
        let outer = prepared.coded.borrow();
        let buffers = expected_charge(outer[0].capacity(), size_of::<Entry>())
            + expected_charge(outer[1].capacity(), size_of::<Entry>());
        let tables = expected_charge(outer[0][0].1.table.capacity(), 1)
            + expected_charge(outer[0][1].1.table.capacity(), 1)
            + expected_charge(outer[1][0].1.table.capacity(), 1);
        assert_eq!(prepared.retained.get() - before, buffers + tables);
    }

    #[test]
    fn a_kept_encoding_is_admitted_at_its_exact_charge_and_refused_one_byte_short() {
        let old = vec![0u8; 256];
        let new = text(256);
        let p = problem(&old, &new, &HOST, 16);
        let ctl = Control::unbounded();
        let source = raw_baseline(256);
        let roomy = settled(Prepared::new(&p, &source, ANALYSIS_BYTES, &ctl))
            .unwrap_or_else(|| panic!("analysed"));
        let base = roomy.retained.get();
        assert!(roomy.retain(0, (Kind::Sparse, Some(9)), &table(40)));
        let need = roomy.retained.get() - base;
        let mut exact = settled(Prepared::new(&p, &source, ANALYSIS_BYTES, &ctl))
            .unwrap_or_else(|| panic!("analysed"));
        exact.budget = base + need;
        assert!(exact.retain(0, (Kind::Sparse, Some(9)), &table(40)));
        assert_eq!(exact.retained.get(), base + need);
        let mut short = settled(Prepared::new(&p, &source, ANALYSIS_BYTES, &ctl))
            .unwrap_or_else(|| panic!("analysed"));
        short.budget = base + need - 1;
        let before = snapshot(&short);
        assert!(!short.retain(0, (Kind::Sparse, Some(9)), &table(40)));
        assert_eq!(snapshot(&short), before);
        assert!(short.cached(0, (Kind::Sparse, Some(9))).is_none());
    }

    #[test]
    fn a_charge_that_overflows_the_total_is_refused_and_changes_nothing() {
        let old = vec![0u8; 256];
        let new = text(256);
        let p = problem(&old, &new, &HOST, 16);
        let ctl = Control::unbounded();
        let mut prepared = settled(Prepared::new(&p, &raw_baseline(256), ANALYSIS_BYTES, &ctl))
            .unwrap_or_else(|| panic!("analysed"));
        prepared.budget = usize::MAX;
        prepared.retained.set(usize::MAX - 8);
        let before = snapshot(&prepared);
        assert!(
            !prepared.retain(0, (Kind::Sparse, Some(9)), &table(40)),
            "the first encoding: buffer and table"
        );
        assert_eq!(snapshot(&prepared), before);
        prepared.retained.set(0);
        assert!(prepared.retain(0, (Kind::Sparse, Some(9)), &table(40)));
        prepared.retained.set(usize::MAX - 8);
        let before = snapshot(&prepared);
        assert!(
            !prepared.retain(0, (Kind::Sparse, Some(1)), &table(40)),
            "a later encoding: its table"
        );
        assert_eq!(snapshot(&prepared), before);
    }

    #[test]
    fn a_refused_analysis_decides_as_the_analysed_pass_does() {
        let old = noise(512, 4);
        let mut new = old.clone();
        new[100..300].fill(7);
        let p = problem(&old, &new, &HOST, 16);
        let ctl = Control::unbounded();
        let candidates = vec![raw_baseline(512)];
        let need = settled(Prepared::new(&p, &candidates[0], ANALYSIS_BYTES, &ctl))
            .unwrap_or_else(|| panic!("analysed"))
            .retained
            .get();
        let kept = settled(optimize_with(&p, candidates.clone(), &ctl, need));
        let refused = settled(optimize_with(&p, candidates, &ctl, need - 1));
        assert_eq!(decisions(&kept), decisions(&refused));
        let kept_maps = kept.as_ref().map_or(0, |o| o.mappings);
        let refused_maps = refused.as_ref().map_or(0, |o| o.mappings);
        assert!(
            kept_maps < refused_maps,
            "the kept analysis made {} replays, the refused one {}",
            kept_maps,
            refused_maps
        );
    }

    #[test]
    fn an_analysis_is_not_refused_because_the_raw_program_is_refused() {
        let old = vec![0u8; 4096];
        let new = text(4096);
        let tight = ResourceProfile {
            max_encoded_manifest: 100,
            ..HOST
        };
        let p = problem(&old, &new, &tight, 64);
        let source = raw_baseline(4096);
        let ctl = Control::unbounded();
        assert!(
            settled(evaluate(&p, source.clone(), &ctl)).is_none(),
            "the raw program must be refused"
        );
        let prepared = settled(Prepared::new(&p, &source, ANALYSIS_BYTES, &ctl))
            .unwrap_or_else(|| panic!("analysed"));
        for enc in ENCODINGS {
            let variant = encoded(&source, None, enc);
            assert!(
                settled(evaluate(&p, variant.clone(), &ctl)).is_none(),
                "{:?} must be refused too",
                enc
            );
            let analysed = settled(prepared.evaluate(&p, &variant, &ctl));
            assert_eq!(
                analysed,
                settled(evaluate(&p, variant.clone(), &ctl)),
                "{:?}",
                enc
            );
        }
    }

    #[test]
    fn the_retained_charges_cannot_wrap() {
        assert_eq!(heap_bytes(0, usize::MAX), Some(0));
        assert_eq!(heap_bytes(usize::MAX, 2), None);
        assert_eq!(heap_bytes(usize::MAX / 2 + 1, 2), None);
        let prepared = empty_analysis(usize::MAX);
        prepared.retained.set(1);
        assert!(
            !prepared.charge(Some(usize::MAX)),
            "a total past usize::MAX is refused"
        );
        assert!(!prepared.charge(None), "an overflowed charge is refused");
        assert_eq!(
            prepared.retained.get(),
            1,
            "a refused charge changes nothing"
        );
    }

    #[test]
    fn the_analysis_is_charged_by_the_capacity_its_buffers_hold() {
        let mut mismatches: Vec<Span> = Vec::with_capacity(100);
        mismatches.push(s(0, 1));
        let mut prepared = empty_analysis(usize::MAX);
        prepared.mismatches = mismatches;
        assert_eq!(prepared.base_bytes(), heap_bytes(100, size_of::<Span>()));
    }

    #[test]
    fn a_sparse_operation_with_no_blob_carries_the_digest_of_no_bytes() {
        let old = vec![7u8; 64];
        let new = old.clone();
        let p = problem(&old, &new, &HOST, 8);
        let ctl = Control::unbounded();
        let source = vec![Op::new(Kind::Replace, vec![s(0, 64)])];
        let prepared = settled(Prepared::new(&p, &source, ANALYSIS_BYTES, &ctl))
            .unwrap_or_else(|| panic!("analysed"));
        let variant = encoded(&source, None, (Kind::Sparse, None));
        let (_, manifest, blob_len) = match settled(prepared.manifest(&p, &variant, &ctl)) {
            Some(found) => found,
            None => panic!("a kept operation is admissible"),
        };
        assert_eq!(blob_len, 0);
        let hash = manifest.partitions[0].operations[0]
            .data_sha256_hash
            .clone();
        assert_eq!(hash, Some(sha256::hash_bytes(&[]).to_vec()));
        let mapped = settled(p.mapping.map(&variant, &ctl, false));
        assert_eq!(mapped.ops[0].data_sha256_hash, hash);
    }

    #[test]
    fn analysed_trials_size_the_manifest_across_the_varint_boundaries() {
        let old: Vec<u8> = (0..20_000usize).map(|k| (k / 7 % 3) as u8).collect();
        let mut new = old.clone();
        new[100..300].fill(9);
        new[16_300..19_000].fill(5);
        let p = problem(&old, &new, &HOST, 4096);
        let ctl = Control::unbounded();
        let source = vec![
            Op::new(Kind::Replace, vec![s(100, 16_400)]),
            Op::new(Kind::Replace, vec![s(16_500, 3_500)]),
        ];
        let prepared = settled(Prepared::new(&p, &source, ANALYSIS_BYTES, &ctl))
            .unwrap_or_else(|| panic!("analysed"));
        let mut r = Rng(11);
        let mut variants: Vec<Program> = Vec::new();
        for enc in ENCODINGS {
            variants.push(encoded(&source, None, enc));
        }
        for _ in 0..8 {
            variants.push(randomly_encoded(&mut r, &source));
        }
        for variant in variants {
            let analysed = settled(prepared.evaluate(&p, &variant, &ctl));
            let ordinary = settled(evaluate(&p, variant.clone(), &ctl));
            assert_eq!(analysed, ordinary, "{:?}", variant);
        }
    }

    #[test]
    fn an_expired_control_interrupts_the_analysis_and_a_new_encoding() {
        let old = vec![0u8; 256];
        let new = text(256);
        let p = problem(&old, &new, &HOST, 16);
        let source = raw_baseline(256);
        let clock = Ticking::new(1.0);
        let expired = Control::new(&clock, 0.5, 1.0, 0, None);
        assert!(matches!(
            Prepared::new(&p, &source, ANALYSIS_BYTES, &expired),
            Err(Halt::Deadline)
        ));
        let prepared = settled(Prepared::new(
            &p,
            &source,
            ANALYSIS_BYTES,
            &Control::unbounded(),
        ))
        .unwrap_or_else(|| panic!("analysed"));
        let variant = encoded(&source, None, (Kind::Sparse, Some(9)));
        assert!(matches!(
            prepared.evaluate(&p, &variant, &expired),
            Err(Halt::Deadline)
        ));
    }
}
