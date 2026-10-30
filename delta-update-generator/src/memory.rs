//! The sequential memory model: the target, as runs of bytes with known origins.
//!
//! The client executes a program in order on one target. Replaying it byte by byte
//! would cost one image of work per candidate, so this model keeps **runs** instead.
//! A run is a byte range whose contents all come from one origin: the old image,
//! the new image, or zero, starting at some offset. The runs always cover
//! `[0, len)` exactly once, and a write replaces the runs it overlaps, so the last
//! write to any byte wins, as it does on the device.
//!
//! The origin of a byte is what the byte *is*, not what the search intends. Two
//! different origins can hold equal bytes, and [`Images::matches`] compares contents,
//! so a run is checked against the new image by value.

use std::collections::BTreeMap;

use crate::control::Control;
use crate::control::Step;
use crate::program::Span;

const SCAN_BYTES: usize = 4096;

/// Whether `a` and `b` hold the same bytes. The control is checked on entry and before each piece
/// of [`SCAN_BYTES`], so an equal pair of any length observes the deadline.
pub fn bytes_equal(a: &[u8], b: &[u8], ctl: &Control) -> Step<bool> {
    ctl.checkpoint()?;
    if a.len() != b.len() {
        return Ok(false);
    }
    for (x, y) in a.chunks(SCAN_BYTES).zip(b.chunks(SCAN_BYTES)) {
        ctl.checkpoint()?;
        if x != y {
            return Ok(false);
        }
    }
    return Ok(true);
}

/// Whether every byte of `a` is zero, examined in the same bounded pieces as [`bytes_equal`].
pub fn all_zero(a: &[u8], ctl: &Control) -> Step<bool> {
    ctl.checkpoint()?;
    for piece in a.chunks(SCAN_BYTES) {
        ctl.checkpoint()?;
        if piece.iter().any(|b| *b != 0) {
            return Ok(false);
        }
    }
    return Ok(true);
}

/// Where the bytes of a run come from. The offset is the origin of the run's first
/// byte; the rest follow it one byte at a time.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Origin {
    /// Bytes of the immutable old image, from this offset.
    Old(u64),
    /// Bytes the operation wrote from the new image, from this offset.
    New(u64),
    /// Zeros.
    Zero,
}

impl Origin {
    /// The origin of the byte `by` bytes after this one.
    pub fn advance(self, by: u64) -> Origin {
        return match self {
            Origin::Old(x) => Origin::Old(x + by),
            Origin::New(x) => Origin::New(x + by),
            Origin::Zero => Origin::Zero,
        };
    }
}

/// A byte range of the target whose contents are all `origin`'s bytes.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Run {
    pub start: u64,
    pub len: u64,
    pub origin: Origin,
}

impl Run {
    pub fn end(&self) -> u64 {
        return self.start + self.len;
    }
}

/// The two images a program is checked against. Both have the partition's length.
#[derive(Clone, Copy)]
pub struct Images<'a> {
    pub old: &'a [u8],
    pub new: &'a [u8],
}

impl<'a> Images<'a> {
    fn origin_byte(&self, origin: Origin, k: u64) -> Option<u8> {
        return match origin {
            Origin::Old(x) => self.old.get((x + k) as usize).copied(),
            Origin::New(x) => Some(self.new[(x + k) as usize]),
            Origin::Zero => Some(0),
        };
    }

    /// Whether a run holds exactly the new image's bytes at its position. Compares contents. A
    /// `New` origin at its own position is equal by construction, which saves the comparison but
    /// not the control check. A deadline interrupts the comparison with `Halt::Deadline`.
    pub fn matches(&self, run: &Run, ctl: &Control) -> Step<bool> {
        let n = run.len as usize;
        let s = run.start as usize;
        let want = &self.new[s..s + n];
        return match run.origin {
            Origin::Old(x) => match self.old.get(x as usize..x as usize + n) {
                Some(bytes) => bytes_equal(bytes, want, ctl),
                None => {
                    ctl.checkpoint()?;
                    Ok(false)
                }
            },
            Origin::New(x) if x == run.start => {
                ctl.checkpoint()?;
                Ok(true)
            }
            Origin::New(x) => bytes_equal(&self.new[x as usize..x as usize + n], want, ctl),
            Origin::Zero => all_zero(want, ctl),
        };
    }

    /// Whether every byte of `spans` is zero in the new image.
    pub fn new_is_zero(&self, spans: &[Span], ctl: &Control) -> Step<bool> {
        for s in spans {
            if !all_zero(&self.new[s.start as usize..s.end() as usize], ctl)? {
                return Ok(false);
            }
        }
        return Ok(true);
    }
}

/// The target during replay: runs covering `[0, len)` in offset order.
pub struct Target {
    len: u64,
    runs: BTreeMap<u64, Run>,
}

impl Target {
    /// The target before any operation: old-image bytes where present, unknown beyond it.
    pub fn new(len: u64) -> Target {
        let mut runs = BTreeMap::new();
        if len > 0 {
            runs.insert(
                0,
                Run {
                    start: 0,
                    len: len,
                    origin: Origin::Old(0),
                },
            );
        }
        return Target {
            len: len,
            runs: runs,
        };
    }

    fn split_at(&mut self, at: u64) {
        if at == 0 || at >= self.len {
            return;
        }
        let run = match self.runs.range(..=at).next_back() {
            None => return,
            Some((_, r)) => *r,
        };
        if run.start == at {
            return;
        }
        let left = Run {
            start: run.start,
            len: at - run.start,
            origin: run.origin,
        };
        let right = Run {
            start: at,
            len: run.end() - at,
            origin: run.origin.advance(at - run.start),
        };
        self.runs.insert(left.start, left);
        self.runs.insert(right.start, right);
    }

    /// Write `span` with bytes from `origin`, which is the origin of the span's first
    /// byte. Overwrites whatever the span covered.
    pub fn write(&mut self, span: Span, origin: Origin) {
        if span.len == 0 {
            return;
        }
        let end = span.end();
        self.split_at(span.start);
        self.split_at(end);
        let inside: Vec<u64> = self
            .runs
            .range(span.start..end)
            .map(|(k, _)| *k)
            .collect();
        for k in inside {
            self.runs.remove(&k);
        }
        self.runs.insert(
            span.start,
            Run {
                start: span.start,
                len: span.len,
                origin: origin,
            },
        );
    }

    /// The runs that cover `span`, clipped to it, in offset order.
    pub fn runs_in(&self, span: Span) -> Vec<Run> {
        let mut out: Vec<Run> = Vec::new();
        if span.len == 0 {
            return out;
        }
        let end = span.end();
        let first = match self.runs.range(..=span.start).next_back() {
            None => span.start,
            Some((k, _)) => *k,
        };
        for (_, r) in self.runs.range(first..end) {
            let a = if r.start > span.start { r.start } else { span.start };
            let b = if r.end() < end { r.end() } else { end };
            if b <= a {
                continue;
            }
            out.push(Run {
                start: a,
                len: b - a,
                origin: r.origin.advance(a - r.start),
            });
        }
        return out;
    }

    /// Whether the target already holds the new image's bytes over every span. Each run is
    /// compared under the control, so a deadline interrupts the test.
    pub fn holds_new(&self, spans: &[Span], images: &Images, ctl: &Control) -> Step<bool> {
        for s in spans {
            for r in self.runs_in(*s) {
                if !images.matches(&r, ctl)? {
                    return Ok(false);
                }
            }
        }
        return Ok(true);
    }

    /// The target's bytes over `spans`, concatenated in stream order.
    pub fn gather(&self, spans: &[Span], images: &Images) -> Vec<u8> {
        let mut out: Vec<u8> = Vec::new();
        for s in spans {
            for r in self.runs_in(*s) {
                append_run(&mut out, &r, images);
            }
        }
        return out;
    }

    /// The spans whose bytes differ from the new image, in ascending order and merged where they
    /// touch. Empty exactly when the target equals the new image. Every run is compared under the
    /// control, matching runs included, and the control is checked before each piece of
    /// [`SCAN_BYTES`]. An interrupted scan returns `Halt::Deadline`, never a partial list.
    pub fn mismatches(&self, images: &Images, ctl: &Control) -> Step<Vec<Span>> {
        ctl.checkpoint()?;
        let mut out: Vec<Span> = Vec::new();
        for r in self.runs.values() {
            if images.matches(r, ctl)? {
                continue;
            }
            for k in 0..r.len {
                if k % SCAN_BYTES as u64 == 0 {
                    ctl.checkpoint()?;
                }
                let p = r.start + k;
                if images.origin_byte(r.origin, k) == Some(images.new[p as usize]) {
                    continue;
                }
                match out.last_mut() {
                    Some(last) if last.end() == p => last.len += 1,
                    _ => out.push(Span { start: p, len: 1 }),
                }
            }
        }
        return Ok(out);
    }
}

fn append_run(out: &mut Vec<u8>, r: &Run, images: &Images) {
    let n = r.len as usize;
    match r.origin {
        Origin::Old(x) => out.extend_from_slice(&images.old[x as usize..x as usize + n]),
        Origin::New(x) => out.extend_from_slice(&images.new[x as usize..x as usize + n]),
        Origin::Zero => out.resize(out.len() + n, 0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::control::Halt;
    use crate::control::test_clocks::Ticking;

    fn s(start: u64, len: u64) -> Span {
        return Span { start: start, len: len };
    }

    fn assert_partition(t: &Target) {
        let mut next = 0u64;
        for r in t.runs.values() {
            assert_eq!(r.start, next, "runs must be contiguous");
            assert!(r.len > 0, "runs must be non-empty");
            next = r.end();
        }
        assert_eq!(next, t.len, "runs must cover the image");
    }

    fn images(old: &[u8], new: &[u8]) -> (Vec<u8>, Vec<u8>) {
        return (old.to_vec(), new.to_vec());
    }

    #[test]
    fn a_fresh_target_is_the_old_image() {
        let (old, new) = images(&[1, 2, 3, 4, 5], &[1, 9, 3, 4, 5]);
        let images = Images { old: &old, new: &new };
        let t = Target::new(5);
        assert_partition(&t);
        assert_eq!(t.gather(&[s(0, 5)], &images), old);
        assert_eq!(t.mismatches(&images, &Control::unbounded()).unwrap_or_default(), vec![s(1, 1)]);
    }

    #[test]
    fn later_writes_win_and_runs_stay_a_partition() {
        let old = vec![0u8; 16];
        let new = vec![0u8; 16];
        let images = Images { old: &old, new: &new };
        let mut t = Target::new(16);
        t.write(s(4, 8), Origin::Zero);
        assert_partition(&t);
        t.write(s(6, 2), Origin::Old(10));
        assert_partition(&t);
        let runs = t.runs_in(s(0, 16));
        let origins: Vec<(u64, u64, Origin)> = runs.iter().map(|r| (r.start, r.len, r.origin)).collect();
        assert_eq!(
            origins,
            vec![
                (0, 4, Origin::Old(0)),
                (4, 2, Origin::Zero),
                (6, 2, Origin::Old(10)),
                (8, 4, Origin::Zero),
                (12, 4, Origin::Old(12)),
            ]
        );
        assert_eq!(t.gather(&[s(4, 4)], &images), vec![0, 0, 0, 0]);
    }

    #[test]
    fn a_clipped_run_keeps_its_origin_offset() {
        let old = vec![0u8; 32];
        let new = vec![0u8; 32];
        let _images = Images { old: &old, new: &new };
        let mut t = Target::new(32);
        t.write(s(10, 10), Origin::New(10));
        let runs = t.runs_in(s(15, 3));
        assert_eq!(
            runs,
            vec![Run {
                start: 15,
                len: 3,
                origin: Origin::New(15),
            }]
        );
    }

    #[test]
    fn holds_new_compares_contents_not_origins() {
        let old = vec![9u8, 2, 3, 4];
        let new = vec![7u8, 2, 3, 4];
        let images = Images { old: &old, new: &new };
        let ctl = Control::unbounded();
        let t = Target::new(4);
        assert!(t.holds_new(&[s(1, 3)], &images, &ctl).unwrap_or(false));
        assert!(!t.holds_new(&[s(0, 2)], &images, &ctl).unwrap_or(true));
        let mut u = Target::new(4);
        u.write(s(0, 2), Origin::Old(1));
        assert!(!u.holds_new(&[s(0, 2)], &images, &ctl).unwrap_or(true));
    }

    #[test]
    fn mismatches_merge_across_run_boundaries() {
        let old = vec![1u8, 1, 1, 1, 1, 1];
        let new = vec![2u8, 2, 1, 2, 2, 1];
        let images = Images { old: &old, new: &new };
        let mut t = Target::new(6);
        t.write(s(0, 2), Origin::New(0));
        t.write(s(2, 1), Origin::Zero);
        assert_eq!(t.mismatches(&images, &Control::unbounded()).unwrap_or_default(), vec![s(2, 3)]);
    }

    #[test]
    fn an_empty_image_has_no_runs_and_no_mismatches() {
        let images = Images { old: &[], new: &[] };
        let t = Target::new(0);
        assert_partition(&t);
        assert!(t.mismatches(&images, &Control::unbounded()).unwrap_or_default().is_empty());
    }

    #[test]
    fn a_bounded_comparison_checks_the_control_before_each_piece() {
        let a = vec![4u8; 10 * SCAN_BYTES];
        let clock = Ticking::new(0.0);
        let ctl = Control::new(&clock, 1.0, 1.0, 0, None);
        assert_eq!(bytes_equal(&a, &a.clone(), &ctl).ok(), Some(true));
        assert_eq!(clock.reads.get(), 11);
    }

    #[test]
    fn all_zero_finds_one_nonzero_byte_in_any_piece() {
        let mut a = vec![0u8; 3 * SCAN_BYTES + 5];
        assert_eq!(all_zero(&a, &Control::unbounded()).ok(), Some(true));
        a[2 * SCAN_BYTES + 7] = 1;
        assert_eq!(all_zero(&a, &Control::unbounded()).ok(), Some(false));
    }

    #[test]
    fn an_expired_control_interrupts_a_run_that_already_matches() {
        let old: Vec<u8> = (0..4096u32).map(|k| (k % 251) as u8).collect();
        let (o, n) = images(&old, &old);
        let view = Images { old: &o, new: &n };
        let t = Target::new(old.len() as u64);
        let clock = Ticking::new(1.0);
        let expired = Control::new(&clock, 0.5, 1.0, 0, None);
        assert!(matches!(t.mismatches(&view, &expired), Err(Halt::Deadline)));
    }

    #[test]
    fn an_expired_control_interrupts_the_keep_and_zero_tests() {
        let old: Vec<u8> = (0..64u8).collect();
        let new = old.clone();
        let (o, n) = images(&old, &new);
        let view = Images { old: &o, new: &n };
        let t = Target::new(64);
        let clock = Ticking::new(1.0);
        let expired = Control::new(&clock, 0.5, 1.0, 0, None);
        assert!(matches!(t.holds_new(&[s(0, 64)], &view, &expired), Err(Halt::Deadline)));
        assert!(matches!(view.new_is_zero(&[s(0, 64)], &expired), Err(Halt::Deadline)));
    }

    #[test]
    fn an_expired_control_interrupts_a_run_held_as_new_bytes_at_its_own_offset() {
        let old = vec![0u8; 64];
        let new: Vec<u8> = (0..64u8).collect();
        let view = Images { old: &old, new: &new };
        let mut t = Target::new(64);
        t.write(s(0, 64), Origin::New(0));
        let clock = Ticking::new(1.0);
        let expired = Control::new(&clock, 0.5, 1.0, 0, None);
        assert!(matches!(t.mismatches(&view, &expired), Err(Halt::Deadline)));
    }
}
