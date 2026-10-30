//! The search's chromosome: an ordered program of byte-range operations.
//!
//! A program is a list of [`Op`]s executed in declared order. Each operation names
//! its destination as byte spans, and a copy also names its source spans. Nothing
//! here is aligned: a span may start and end at any byte of the image. What the
//! program is *allowed* to do is decided by replaying it on the memory model
//! (`memory`) and by the mapper (`mapper`), not by any rule in this module.
//!
//! Keep and zero chunks of a `Sparse` operation are not part of the chromosome.
//! The mapper derives them from the target state at that point of the program, so
//! a chromosome describes what is written and in what order, not how each chunk is
//! encoded.

use crate::control::Control;
use crate::control::Step;

/// A byte range of the partition: `len` bytes from `start`. Spans are never empty
/// in a valid program, and the mapper checks that they lie inside the image.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct Span {
    pub start: u64,
    pub len: u64,
}

impl Span {
    /// One past the last byte. Overflow is refused by the mapper before this is
    /// used on untrusted values, so a valid span cannot wrap here.
    pub fn end(&self) -> u64 {
        return self.start + self.len;
    }
}

/// The operations the search may choose. The first five have a wire form the client
/// executes. `Frames` and `Sparse` are the encodings `REPLACE_FRAMES` and
/// `REPLACE_SPARSE`; `Replace` is the reference `REPLACE` with a raw blob.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Kind {
    /// Raw blob of the destination's logical stream.
    Replace,
    /// Per-chunk raw or zlib frames, with a frame table.
    Frames,
    /// Per-chunk keep, zero, raw or zlib. Keep and zero are derived by the mapper.
    Sparse,
    /// Zeros over the destination. No blob.
    Zero,
    /// A copy from the immutable old image.
    SourceCopy,
    /// A copy from the target, as the target stands when the operation runs.
    TargetCopy,
}

impl Kind {
    /// Whether the operation carries a blob of destination bytes.
    pub fn carries_data(self) -> bool {
        return match self {
            Kind::Replace | Kind::Frames | Kind::Sparse => true,
            Kind::Zero | Kind::SourceCopy | Kind::TargetCopy => false,
        };
    }

    /// Whether the operation reads a source, and so has source spans.
    pub fn is_copy(self) -> bool {
        return self == Kind::SourceCopy || self == Kind::TargetCopy;
    }
}

/// One operation of a program.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub struct Op {
    pub kind: Kind,
    /// Destination spans, in declared order. They may overlap each other and other
    /// operations' spans, and they may be unsorted.
    pub dst: Vec<Span>,
    /// Copy source spans, in declared order. Empty for every other kind.
    pub src: Vec<Span>,
    /// Compression level for the chunks of a data operation. `None` stores every
    /// chunk raw. Ignored by the other kinds.
    pub level: Option<u8>,
}

impl Op {
    /// A destination-only operation with no compression.
    pub fn new(kind: Kind, dst: Vec<Span>) -> Op {
        return Op {
            kind: kind,
            dst: dst,
            src: Vec::new(),
            level: None,
        };
    }

    /// A copy of `src` into `dst`, which the caller pairs by stream position.
    pub fn copy(kind: Kind, dst: Vec<Span>, src: Vec<Span>) -> Op {
        return Op {
            kind: kind,
            dst: dst,
            src: src,
            level: None,
        };
    }
}

/// A program: its operations in execution order.
pub type Program = Vec<Op>;

/// Total length of a list of spans, or `None` when it overflows.
pub fn total(spans: &[Span]) -> Option<u64> {
    let mut sum: u64 = 0;
    for s in spans {
        sum = match sum.checked_add(s.len) {
            None => return None,
            Some(v) => v,
        };
    }
    return Some(sum);
}

/// The absolute spans that cover the stream bytes `[from, to)` of `spans`, in stream
/// order. A stream is the spans concatenated in their declared order.
pub fn slice_stream(spans: &[Span], from: u64, to: u64) -> Vec<Span> {
    let mut out: Vec<Span> = Vec::new();
    if from >= to {
        return out;
    }
    let mut base: u64 = 0;
    for s in spans {
        if base >= to {
            break;
        }
        let end = base + s.len;
        if end > from {
            let a = if from > base { from } else { base };
            let b = if to < end { to } else { end };
            out.push(Span {
                start: s.start + (a - base),
                len: b - a,
            });
        }
        base = end;
    }
    return out;
}

/// The codec chunks of a stream, in order. Each item is the chunk's length and the absolute spans that
/// cover it, which are what [`slice_stream`] gives for the chunk's stream range.
///
/// The cursor moves forward through the spans once, so cutting a whole stream costs its extents and
/// its chunks, not their product. It is the same cut as calling `slice_stream` for each chunk, which
/// the tests check. The spans are taken in declared order and are not sorted or merged. The stream's
/// total must fit in `u64`, which the caller checks before it cuts.
pub struct Chunks<'a> {
    spans: &'a [Span],
    chunk: u64,
    span: usize,
    within: u64,
    left: u64,
}

impl<'a> Chunks<'a> {
    /// The chunks of `spans` cut every `chunk` bytes. `chunk` must be nonzero.
    pub fn new(spans: &'a [Span], chunk: u64) -> Chunks<'a> {
        return Chunks {
            spans: spans,
            chunk: chunk,
            span: 0,
            within: 0,
            left: total(spans).unwrap_or(0),
        };
    }
}

/// Extents a chunk visits between two control checks, counting the zero-length ones, so that a long run of
/// them cannot pass a control unobserved. A check is made before every `CUT_CHECK_EXTENTS`th extent a chunk visits.
pub const CUT_CHECK_EXTENTS: usize = 64;

impl<'a> Chunks<'a> {
    /// The next chunk, as [`Iterator::next`] gives it, with the control consulted first: before the chunk is built,
    /// and before every [`CUT_CHECK_EXTENTS`]th extent it visits. A deadline returns `Halt::Deadline` and leaves the
    /// cursor where it was, so a later call cuts the same chunk.
    pub fn next_checked(&mut self, ctl: &Control) -> Step<Option<(u64, Vec<Span>)>> {
        return self.cut(Some(ctl));
    }

    fn cut(&mut self, ctl: Option<&Control>) -> Step<Option<(u64, Vec<Span>)>> {
        if self.left == 0 {
            return Ok(None);
        }
        if let Some(ctl) = ctl {
            ctl.checkpoint()?;
        }
        let want = if self.left < self.chunk { self.left } else { self.chunk };
        let mut span = self.span;
        let mut within = self.within;
        let mut need = want;
        let mut visited = 0usize;
        let mut pieces: Vec<Span> = Vec::new();
        while need > 0 {
            visited += 1;
            if let Some(ctl) = ctl {
                if visited % CUT_CHECK_EXTENTS == 0 {
                    ctl.checkpoint()?;
                }
            }
            let s = self.spans[span];
            let take = if s.len - within < need { s.len - within } else { need };
            if take > 0 {
                pieces.push(Span {
                    start: s.start + within,
                    len: take,
                });
            }
            within += take;
            need -= take;
            if within == s.len {
                span += 1;
                within = 0;
            }
        }
        self.span = span;
        self.within = within;
        self.left -= want;
        return Ok(Some((want, pieces)));
    }
}

impl<'a> Iterator for Chunks<'a> {
    type Item = (u64, Vec<Span>);

    fn next(&mut self) -> Option<(u64, Vec<Span>)> {
        return self.cut(None).ok().flatten();
    }
}

/// Pair two streams of equal total length by stream position. Each pair has equal
/// lengths, and the pairs cover both streams in order.
pub fn zip_streams(src: &[Span], dst: &[Span]) -> Vec<(Span, Span)> {
    let mut out: Vec<(Span, Span)> = Vec::new();
    let mut i = 0usize;
    let mut j = 0usize;
    let mut si: u64 = 0;
    let mut dj: u64 = 0;
    while i < src.len() && j < dst.len() {
        let a = src[i].len - si;
        let b = dst[j].len - dj;
        let n = if a < b { a } else { b };
        out.push((
            Span {
                start: src[i].start + si,
                len: n,
            },
            Span {
                start: dst[j].start + dj,
                len: n,
            },
        ));
        si += n;
        dj += n;
        if si == src[i].len {
            i += 1;
            si = 0;
        }
        if dj == dst[j].len {
            j += 1;
            dj = 0;
        }
    }
    return out;
}

/// The program that writes the whole new image in one raw replacement. It is the
/// search's starting point, and it is valid for every pair of images. An empty
/// image needs no operations at all.
pub fn raw_baseline(len: u64) -> Program {
    if len == 0 {
        return Vec::new();
    }
    return vec![Op::new(
        Kind::Replace,
        vec![Span { start: 0, len: len }],
    )];
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::control::Halt;
    use crate::control::test_clocks::Ticking;

    fn s(start: u64, len: u64) -> Span {
        return Span { start: start, len: len };
    }

    #[test]
    fn stream_slices_follow_declared_order_across_extents() {
        let spans = vec![s(100, 4), s(10, 2), s(50, 3)];
        assert_eq!(slice_stream(&spans, 0, 4), vec![s(100, 4)]);
        assert_eq!(slice_stream(&spans, 3, 7), vec![s(103, 1), s(10, 2), s(50, 1)]);
        assert_eq!(slice_stream(&spans, 6, 9), vec![s(50, 3)]);
        assert_eq!(slice_stream(&spans, 2, 2), Vec::<Span>::new());
    }

    #[test]
    fn slices_of_one_stream_concatenate_to_the_stream() {
        let spans = vec![s(7, 5), s(0, 3), s(40, 6)];
        let total_len = total(&spans).unwrap_or(0);
        let mut joined: Vec<Span> = Vec::new();
        for k in 0..total_len {
            joined.extend(slice_stream(&spans, k, k + 1));
        }
        let expected: Vec<u64> = vec![7, 8, 9, 10, 11, 0, 1, 2, 40, 41, 42, 43, 44, 45];
        let got: Vec<u64> = joined.iter().map(|x| x.start).collect();
        assert_eq!(got, expected);
    }

    #[test]
    fn the_chunk_cursor_cuts_exactly_as_slice_stream_does() {
        for spans in cut_cases() {
            let len = total(&spans).unwrap_or(0);
            for chunk in 1..=7u64 {
                let mut cut = Chunks::new(&spans, chunk);
                let mut from = 0u64;
                while from < len {
                    let to = if from + chunk < len { from + chunk } else { len };
                    let (bytes, pieces) = cut.next().unwrap_or_else(|| panic!("the cut ended early"));
                    assert_eq!(bytes, to - from, "chunk {} at {} of {:?}", chunk, from, spans);
                    assert_eq!(pieces, slice_stream(&spans, from, to), "chunk {} at {} of {:?}", chunk, from, spans);
                    from = to;
                }
                assert!(cut.next().is_none(), "the cut went past the stream");
            }
        }
        assert!(Chunks::new(&[], 4).next().is_none());
    }

    fn cut_cases() -> Vec<Vec<Span>> {
        return vec![
            vec![s(100, 4), s(10, 2), s(50, 3)],
            vec![s(0, 9), s(4, 9), s(2, 1)],
            vec![s(7, 1), s(7, 1), s(7, 1), s(3, 2)],
            vec![s(5, 0), s(9, 3), s(1, 1)],
            vec![s(40, 13)],
        ];
    }

    #[test]
    fn the_checked_cut_is_the_iterator_cut_under_an_unbounded_control() {
        let unbounded = Control::unbounded();
        for spans in cut_cases() {
            let len = total(&spans).unwrap_or(0);
            for chunk in 1..=7u64 {
                let mut cut = Chunks::new(&spans, chunk);
                let mut from = 0u64;
                while from < len {
                    let to = (from + chunk).min(len);
                    let (bytes, pieces) = match cut.next_checked(&unbounded) {
                        Ok(Some(found)) => found,
                        _ => panic!("the cut ended early"),
                    };
                    assert_eq!(bytes, to - from, "chunk {} at {} of {:?}", chunk, from, spans);
                    assert_eq!(pieces, slice_stream(&spans, from, to), "chunk {} at {} of {:?}", chunk, from, spans);
                    from = to;
                }
                assert!(matches!(cut.next_checked(&unbounded), Ok(None)), "the cut went past the stream");
            }
        }
    }

    #[test]
    fn an_expired_control_cuts_no_chunk_and_moves_no_cursor() {
        let spans = vec![s(0, 3), s(10, 3)];
        let clock = Ticking::new(1.0);
        let expired = Control::new(&clock, 0.5, 1.0, 0, None);
        let mut cut = Chunks::new(&spans, 2);
        assert!(matches!(cut.next_checked(&expired), Err(Halt::Deadline)));
        assert_eq!(clock.reads.get(), 1, "the control is read once, before the chunk is built");
        assert_eq!((cut.span, cut.within, cut.left), (0, 0, 6));
    }

    #[test]
    fn an_interrupted_cut_leaves_the_cursor_where_it_was_and_the_next_cut_is_whole() {
        let spans: Vec<Span> = (0..1000u64).map(|k| s(2 * k, 1)).collect();
        let clock = Ticking::new(1.0);
        let control = Control::new(&clock, 3.5, 1.0, 0, None);
        let mut cut = Chunks::new(&spans, 1000);
        assert!(matches!(cut.next_checked(&control), Err(Halt::Deadline)));
        assert_eq!(clock.reads.get(), 4);
        assert_eq!((cut.span, cut.within, cut.left), (0, 0, 1000));
        let (bytes, pieces) = match cut.next_checked(&Control::unbounded()) {
            Ok(Some(found)) => found,
            _ => panic!("the chunk is cut once the control allows it"),
        };
        assert_eq!(bytes, 1000);
        assert_eq!(pieces, slice_stream(&spans, 0, 1000));
    }

    #[test]
    fn zero_length_extents_count_toward_the_checks() {
        let mut spans = vec![s(0, 0); 1000];
        spans.push(s(5, 1));
        let clock = Ticking::new(1.0);
        let control = Control::new(&clock, 3.5, 1.0, 0, None);
        let mut cut = Chunks::new(&spans, 1);
        assert!(matches!(cut.next_checked(&control), Err(Halt::Deadline)));
        assert_eq!(clock.reads.get(), 4);
    }

    #[test]
    fn zipped_streams_pair_equal_lengths_in_order() {
        let src = vec![s(0, 4), s(20, 3)];
        let dst = vec![s(100, 2), s(200, 5)];
        let pairs = zip_streams(&src, &dst);
        assert_eq!(
            pairs,
            vec![
                (s(0, 2), s(100, 2)),
                (s(2, 2), s(200, 2)),
                (s(20, 3), s(202, 3)),
            ]
        );
    }

    #[test]
    fn overflowing_totals_are_none() {
        assert_eq!(total(&[s(0, u64::MAX), s(0, 1)]), None);
        assert_eq!(total(&[s(0, 2), s(9, 3)]), Some(5));
    }

    #[test]
    fn raw_baseline_is_one_replace_or_nothing() {
        assert_eq!(raw_baseline(0), Vec::<Op>::new());
        let p = raw_baseline(9);
        assert_eq!(p.len(), 1);
        assert_eq!(p[0].kind, Kind::Replace);
        assert_eq!(p[0].dst, vec![s(0, 9)]);
    }

    #[test]
    #[ignore]
    fn cutting_one_byte_chunks_over_many_extents_is_timed() {
        let spans: Vec<Span> = (0..20_000u64).map(|k| s(k * 32, 8)).collect();
        let len = total(&spans).unwrap_or(0);
        let started = std::time::Instant::now();
        let mut count = 0usize;
        for k in 0..len {
            count += slice_stream(&spans, k, k + 1).len();
        }
        let old = started.elapsed().as_secs_f64();
        let started = std::time::Instant::now();
        let mut cursor = 0usize;
        for (_, pieces) in Chunks::new(&spans, 1) {
            cursor += pieces.len();
        }
        let new = started.elapsed().as_secs_f64();
        assert_eq!(count, cursor);
        eprintln!("cut of 160000 one-byte chunks: slice_stream {:.3} s, cursor {:.4} s", old, new);
    }
}
