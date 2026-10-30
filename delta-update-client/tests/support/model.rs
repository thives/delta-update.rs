use std::cell::Cell;

use delta_update_client::image::OldImage;
use delta_update_generator::builder::Kind;
use delta_update_generator::builder::Op;
use delta_update_payload::error::PayloadError;
use delta_update_payload::sha256;

pub struct Old {
    pub data: Vec<u8>,
    pub reads: Cell<usize>,
}

impl Old {
    pub fn new(data: Vec<u8>) -> Old {
        return Old {
            data: data,
            reads: Cell::new(0),
        };
    }
}

impl OldImage for Old {
    fn capacity(&self) -> u64 {
        return self.data.len() as u64;
    }

    fn read_at(&self, offset: u64, out: &mut [u8]) -> Result<(), PayloadError> {
        self.reads.set(self.reads.get() + 1);
        let off = offset as usize;
        out.copy_from_slice(&self.data[off..(off + out.len())]);
        return Ok(());
    }
}

pub fn gather(bytes: &[u8], ranges: &[(u64, u64)]) -> Vec<u8> {
    let mut out = Vec::new();
    for (s, n) in ranges {
        out.extend_from_slice(&bytes[(*s as usize)..((*s + *n) as usize)]);
    }
    return out;
}

fn write_stream(
    target: &mut [u8],
    dst: &[(u64, u64)],
    bytes: &[u8],
    write: impl Fn(usize) -> bool,
) {
    let mut p = 0usize;
    for (s, n) in dst {
        for j in 0..(*n as usize) {
            if write(p) {
                target[(*s as usize) + j] = bytes[p];
            }
            p += 1;
        }
    }
}

pub fn apply_op(target: &mut [u8], old: &[u8], op: &Op, chunk: usize) {
    match op.kind {
        Kind::Zero => {
            for (s, n) in &op.dst {
                target[(*s as usize)..((*s + *n) as usize)].fill(0);
            }
        }
        Kind::Replace | Kind::Frames => {
            write_stream(target, &op.dst, &op.data, |_| true);
        }
        Kind::Sparse => {
            write_stream(target, &op.dst, &op.data, |p| {
                !op.keep.get(p / chunk).copied().unwrap_or(false)
            });
        }
        Kind::SourceCopy => {
            let bytes = gather(old, &op.src);
            write_stream(target, &op.dst, &bytes, |_| true);
        }
        Kind::TargetCopy => {
            let bytes = gather(&target.to_vec(), &op.src);
            write_stream(target, &op.dst, &bytes, |_| true);
        }
    }
}

pub fn expected(initial: &[u8], old: &[u8], ops: &[Op], chunk: usize) -> Vec<u8> {
    let mut target = initial.to_vec();
    for op in ops {
        apply_op(&mut target, old, op, chunk);
    }
    return target;
}

pub fn source_hash(state: &[u8], ranges: &[(u64, u64)]) -> [u8; sha256::SHA256_SIZE] {
    return sha256::hash_bytes(&gather(state, ranges));
}

pub struct Rng {
    state: u64,
}

impl Rng {
    pub fn new(seed: u64) -> Rng {
        return Rng {
            state: seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1,
        };
    }

    pub fn next(&mut self) -> u64 {
        self.state ^= self.state << 13;
        self.state ^= self.state >> 7;
        self.state ^= self.state << 17;
        return self.state;
    }

    pub fn below(&mut self, n: u64) -> u64 {
        return self.next() % n;
    }

    pub fn bytes(&mut self, n: usize) -> Vec<u8> {
        return match self.below(3) {
            0 => (0..n).map(|i| (b"update-engine "[i % 14]) as u8).collect(),
            1 => (0..n).map(|_| (self.next() >> 24) as u8).collect(),
            _ => vec![0u8; n],
        };
    }
}

pub fn random_program(
    rng: &mut Rng,
    len: usize,
    chunk: usize,
    old: &[u8],
    initial: &[u8],
    count: usize,
) -> (Vec<Op>, Vec<u8>) {
    let mut state = initial.to_vec();
    let mut ops: Vec<Op> = Vec::new();
    for _ in 0..count {
        let kind = match rng.below(6) {
            0 => Kind::Replace,
            1 => Kind::Zero,
            2 => Kind::SourceCopy,
            3 => Kind::TargetCopy,
            4 => Kind::Frames,
            _ => Kind::Sparse,
        };
        let op = random_op(rng, len, chunk, old, &state, kind);
        apply_op(&mut state, old, &op, chunk);
        ops.push(op);
    }
    return (ops, state);
}

fn random_range(rng: &mut Rng, len: usize, max: usize) -> (u64, u64) {
    let n = 1 + rng.below(max.min(len) as u64);
    let start = rng.below(len as u64 - n + 1);
    return (start, n);
}

fn random_op(rng: &mut Rng, len: usize, chunk: usize, old: &[u8], state: &[u8], kind: Kind) -> Op {
    let extents = 1 + rng.below(3) as usize;
    let dst: Vec<(u64, u64)> = (0..extents)
        .map(|_| random_range(rng, len, 3 * chunk))
        .collect();
    let total: u64 = dst.iter().map(|(_, n)| *n).sum();
    let mut op = Op::new(kind, dst.clone());
    match kind {
        Kind::Zero => {}
        Kind::Replace | Kind::Frames => {
            op.data = rng.bytes(total as usize);
        }
        Kind::Sparse => {
            op.data = rng.bytes(total as usize);
            let chunks = (total as usize).div_ceil(chunk);
            op.keep = (0..chunks).map(|_| rng.below(4) == 0).collect();
        }
        Kind::SourceCopy => {
            op.src = split_total(rng, old.len(), total);
            op.src_hash = source_hash(old, &op.src);
        }
        Kind::TargetCopy => {
            let mut src = None;
            for _ in 0..64 {
                let candidate = split_total(rng, len, total);
                if !intersects(&candidate, &dst) {
                    src = Some(candidate);
                    break;
                }
            }
            match src {
                Some(s) => {
                    op.src_hash = source_hash(state, &s);
                    op.src = s;
                }
                None => {
                    op.kind = Kind::Replace;
                    op.data = rng.bytes(total as usize);
                }
            }
        }
    }
    return op;
}

fn split_total(rng: &mut Rng, len: usize, total: u64) -> Vec<(u64, u64)> {
    let total = total.min(len as u64);
    if rng.below(2) == 0 || total < 2 {
        let start = rng.below(len as u64 - total + 1);
        return vec![(start, total)];
    }
    let first = 1 + rng.below(total - 1);
    let a = rng.below(len as u64 - first + 1);
    let b = rng.below(len as u64 - (total - first) + 1);
    return vec![(a, first), (b, total - first)];
}

fn intersects(a: &[(u64, u64)], b: &[(u64, u64)]) -> bool {
    for (s, n) in a {
        for (t, m) in b {
            if *s < *t + *m && *t < *s + *n {
                return true;
            }
        }
    }
    return false;
}
