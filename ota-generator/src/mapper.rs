//! The mapper: one program, replayed on the memory model, becomes the operations the
//! client will execute.
//!
//! Replay is sequential, as on the device. Each operation is checked on its own
//! terms, then executed on the target, then the next one starts. The result is:
//!
//! - the wire operations, whose sizes are the fitness of the program;
//! - the spans that still differ from the new image, which are empty exactly when the
//!   program is complete.
//!
//! Chunks are cut from each operation's own logical stream, so a chunk may start and
//! end anywhere in the image. A `Sparse` chunk is **kept** when the target already holds
//! the new bytes over it at that point of the program. Keeping in place is always
//! valid, because nothing is written and the bytes are already the ones the program
//! needs. A chunk that is all zero in the new image is written as zero. Every other
//! chunk is written from the new image.
//!
//! A [`Mapping`] is the immutable context of one partition update: its two images and
//! its codec chunk size. It owns the compressed frames of those images, so a frame can
//! only be reused for the images it was made from.

use std::cell::Cell;
use std::cell::RefCell;
use std::collections::HashMap;

use miniz_oxide::deflate::core::CompressorOxide;
use miniz_oxide::deflate::core::TDEFLFlush;
use miniz_oxide::deflate::core::TDEFLStatus;
use miniz_oxide::deflate::core::compress;
use miniz_oxide::deflate::core::create_comp_flags_from_zip_params;
use ota_payload::encoding::push_varint;
use ota_payload::error::Code;
use ota_payload::error::err;
use ota_payload::manifest::Extent;
use ota_payload::manifest::Operation;
use ota_payload::manifest::TYPE_REPLACE;
use ota_payload::manifest::TYPE_REPLACE_FRAMES;
use ota_payload::manifest::TYPE_REPLACE_SPARSE;
use ota_payload::manifest::TYPE_SOURCECOPY;
use ota_payload::manifest::TYPE_TARGET_COPY;
use ota_payload::manifest::TYPE_ZERO;
use ota_payload::sha256;
use ota_payload::sha256::SHA256_SIZE;

use crate::control::Control;
use crate::control::Halt;
use crate::control::Step;
use crate::memory::Images;
use crate::memory::Origin;
use crate::memory::Target;
use crate::program::Kind;
use crate::program::Op;
use crate::program::Chunks;
use crate::program::Span;
use crate::program::total;
use crate::program::zip_streams;

/// Compressed chunks kept before the cache is emptied. A bound on memory, not a
/// tuning value: an emptied cache only costs recompression.
pub const FRAME_CACHE_LIMIT: usize = 1 << 14;
const COMPRESS_PIECE: usize = 16 << 10;
const COMPRESS_OUT: usize = 16 << 10;

thread_local! {
    /// One compressor and one output buffer per level, kept between chunks. Making a compressor
    /// allocates and clears a few hundred kilobytes, which costs more than compressing a small chunk,
    /// and a payload with small chunks compresses hundreds of thousands of them. A compressor that is
    /// reset starts from the state a new one has, so its output is the same.
    static COMPRESSORS: RefCell<HashMap<u8, (CompressorOxide, Vec<u8>)>> = RefCell::new(HashMap::new());
}

fn zlib_stream(raw: &[u8], level: u8, ctl: &Control) -> Step<Vec<u8>> {
    let kept = COMPRESSORS.with(|c| c.borrow_mut().remove(&level));
    let (mut comp, mut buf) = match kept {
        Some((mut comp, buf)) => {
            comp.reset();
            (comp, buf)
        }
        None => {
            let flags = create_comp_flags_from_zip_params(level as i32, 1, 0);
            (CompressorOxide::new(flags), vec![0u8; COMPRESS_OUT])
        }
    };
    let out = deflate_stream(&mut comp, &mut buf, raw, ctl);
    COMPRESSORS.with(|c| c.borrow_mut().insert(level, (comp, buf)));
    return out;
}

fn deflate_stream(comp: &mut CompressorOxide, buf: &mut [u8], raw: &[u8], ctl: &Control) -> Step<Vec<u8>> {
    let mut out: Vec<u8> = Vec::new();
    for piece in raw.chunks(COMPRESS_PIECE) {
        ctl.checkpoint()?;
        let mut input = piece;
        while !input.is_empty() {
            let (status, used, produced) = compress(comp, input, buf, TDEFLFlush::None);
            if status == TDEFLStatus::BadParam || (used == 0 && produced == 0) {
                return Err(Halt::Failed(err(Code::frame_malformed, "the compressor made no progress")));
            }
            out.extend_from_slice(&buf[..produced]);
            input = &input[used..];
        }
    }
    loop {
        ctl.checkpoint()?;
        let (status, _, produced) = compress(comp, &[], buf, TDEFLFlush::Finish);
        out.extend_from_slice(&buf[..produced]);
        match status {
            TDEFLStatus::Done => break,
            TDEFLStatus::BadParam => {
                return Err(Halt::Failed(err(Code::frame_malformed, "the compressor refused to finish")));
            }
            _ => {}
        }
    }
    return Ok(out);
}

pub(crate) fn stored_frame(raw: &[u8], level: u8, ctl: &Control) -> Step<Option<Vec<u8>>> {
    let frame = zlib_stream(raw, level, ctl)?;
    return Ok(if frame.len() < raw.len() { Some(frame) } else { None });
}

pub(crate) struct FrameCache {
    frames: HashMap<(Vec<Span>, u8), Option<Vec<u8>>>,
}

impl FrameCache {
    fn new() -> FrameCache {
        return FrameCache {
            frames: HashMap::new(),
        };
    }

    fn lookup(&mut self, pieces: &[Span], level: u8, new: &[u8], ctl: &Control) -> Step<Option<Vec<u8>>> {
        let key = (pieces.to_vec(), level);
        if let Some(entry) = self.frames.get(&key) {
            return Ok(entry.clone());
        }
        let raw = gather_bytes(pieces, new);
        let kept = stored_frame(&raw, level, ctl)?;
        if self.frames.len() >= FRAME_CACHE_LIMIT {
            self.frames.clear();
        }
        self.frames.insert(key, kept.clone());
        return Ok(kept);
    }
}

pub(crate) fn gather_bytes(pieces: &[Span], image: &[u8]) -> Vec<u8> {
    let mut out: Vec<u8> = Vec::new();
    for s in pieces {
        out.extend_from_slice(&image[s.start as usize..s.end() as usize]);
    }
    return out;
}

/// The immutable context of one partition update: its two images, its codec chunk size,
/// and the compressed frames of those images. Nothing else can supply a frame, so a frame
/// is never reused for images it was not made from.
pub struct Mapping<'a> {
    images: Images<'a>,
    chunk: usize,
    frames: RefCell<FrameCache>,
    maps: Cell<u64>,
}

impl<'a> Mapping<'a> {
    /// A mapping of `images`, cutting data chunks at `chunk` bytes.
    pub fn new(images: Images<'a>, chunk: usize) -> Mapping<'a> {
        return Mapping {
            images: images,
            chunk: chunk,
            frames: RefCell::new(FrameCache::new()),
            maps: Cell::new(0),
        };
    }

    /// How many whole-program replays this mapping has made.
    pub fn maps(&self) -> u64 {
        return self.maps.get();
    }

    pub(crate) fn encode<M>(&self, op: &Op, ctl: &Control, mode: M) -> Step<Coded>
    where
        M: FnMut(usize, &[Span]) -> Step<Chunk>,
    {
        let mut frames = self.frames.borrow_mut();
        return encode_op(op, self.chunk, &mut frames, self.images.new, None, ctl, mode);
    }

    /// The images this mapping replays against.
    pub fn images(&self) -> Images<'a> {
        return self.images;
    }

    /// The codec chunk size.
    pub fn chunk(&self) -> usize {
        return self.chunk;
    }

    /// Replay `program` on the target. With `finalize` false the hashes are placeholders and
    /// no blob bytes are kept, which is all the fitness needs. With `finalize` true the blob
    /// bytes and real hashes are produced, for the payload that is emitted.
    ///
    /// A program that is structurally invalid is a [`Halt::Failed`]. A program that is valid
    /// but incomplete returns its mismatches, so the caller can repair it. A deadline
    /// returns [`Halt::Deadline`] and no partial mapping.
    pub fn map(&self, program: &[Op], ctl: &Control, finalize: bool) -> Step<Mapped> {
        self.maps.set(self.maps.get() + 1);
        let mut frames = self.frames.borrow_mut();
        return replay(program, &self.images, self.chunk, &mut frames, ctl, finalize);
    }
}

/// What a replay produced.
pub struct Mapped {
    /// The operations in execution order. The hashes are placeholders of the final
    /// length unless the replay ran in finalize mode.
    pub ops: Vec<Operation>,
    /// Total blob bytes, which the manifest records as `signatures_offset`.
    pub blob_len: u64,
    /// The blob bytes, in operation order. Present only in finalize mode.
    pub blobs: Option<Vec<u8>>,
    /// Spans where the replayed target still differs from the new image. Empty
    /// exactly when the program is complete.
    pub mismatches: Vec<Span>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Chunk {
    Keep,
    Zero,
    Content,
}

fn replay(
    program: &[Op],
    images: &Images,
    chunk: usize,
    frames: &mut FrameCache,
    ctl: &Control,
    finalize: bool,
) -> Step<Mapped> {
    let len = images.new.len() as u64;
    let mut target = Target::new(len);
    let mut ops: Vec<Operation> = Vec::with_capacity(program.len());
    let mut cursor: u64 = 0;
    let mut blobs: Option<Vec<u8>> = if finalize { Some(Vec::new()) } else { None };
    for op in program {
        ctl.checkpoint()?;
        check_op(op, len, images.old.len() as u64)?;
        let (wire, data_len) = match op.kind {
            Kind::Zero => (zero_op(op, &mut target, cursor), 0),
            Kind::SourceCopy | Kind::TargetCopy => {
                copy_op(op, &mut target, images, cursor, finalize, ctl)?
            }
            Kind::Replace => replace_op(op, &mut target, images, cursor, &mut blobs, ctl)?,
            Kind::Frames | Kind::Sparse => data_op(
                op,
                &mut target,
                images,
                chunk,
                frames,
                cursor,
                &mut blobs,
                ctl,
            )?,
        };
        ops.push(wire);
        cursor += data_len;
    }
    let mismatches = target.mismatches(images, ctl)?;
    return Ok(Mapped {
        ops: ops,
        blob_len: cursor,
        blobs: blobs,
        mismatches: mismatches,
    });
}

fn check_op(op: &Op, len: u64, old_len: u64) -> Result<(), Halt> {
    if op.dst.is_empty() {
        return Err(Halt::Failed(err(
            Code::extent_out_of_range,
            "operation has no destination",
        )));
    }
    check_spans(&op.dst, len)?;
    let dst_total = match total(&op.dst) {
        None => {
            return Err(Halt::Failed(err(
                Code::overflow,
                "destination total overflows",
            )));
        }
        Some(t) => t,
    };
    if op.kind.is_copy() {
        if op.src.is_empty() {
            return Err(Halt::Failed(err(
                Code::extent_out_of_range,
                "copy has no source",
            )));
        }
        check_spans(
            &op.src,
            if op.kind == Kind::SourceCopy {
                old_len
            } else {
                len
            },
        )?;
        let src_total = match total(&op.src) {
            None => return Err(Halt::Failed(err(Code::overflow, "source total overflows"))),
            Some(t) => t,
        };
        if src_total != dst_total {
            return Err(Halt::Failed(err(
                Code::blob_mismatch,
                "copy streams differ in length",
            )));
        }
        if op.kind == Kind::TargetCopy {
            for s in &op.src {
                for d in &op.dst {
                    if s.start < d.end() && d.start < s.end() {
                        return Err(Halt::Failed(err(
                            Code::copy_overlap,
                            "an in-target copy's source intersects its destination",
                        )));
                    }
                }
            }
        }
    } else if !op.src.is_empty() {
        return Err(Halt::Failed(err(
            Code::unsupported_field,
            "source spans on a non-copy operation",
        )));
    }
    if let Some(level) = op.level {
        if level == 0 || level > 9 {
            return Err(Halt::Failed(err(
                Code::unsupported_field,
                "compression level outside 1 to 9",
            )));
        }
    }
    return Ok(());
}

fn check_spans(spans: &[Span], len: u64) -> Result<(), Halt> {
    for s in spans {
        if s.len == 0 {
            return Err(Halt::Failed(err(Code::extent_out_of_range, "zero-length extent")));
        }
        let end = match s.start.checked_add(s.len) {
            None => return Err(Halt::Failed(err(Code::extent_out_of_range, "extent overflows"))),
            Some(e) => e,
        };
        if end > len {
            return Err(Halt::Failed(err(Code::extent_out_of_range, "extent is past the image")));
        }
    }
    return Ok(());
}

pub(crate) fn wire(
    kind: i32,
    dst: &[Span],
    src: &[Span],
    cursor: u64,
    data_len: u64,
    hash: Option<Vec<u8>>,
    src_hash: Option<Vec<u8>>,
    frame_table: Option<Vec<u8>>,
) -> Operation {
    return Operation {
        r#type: kind,
        data_offset: Some(cursor),
        data_length: Some(data_len),
        src_extents: extents(src),
        src_length: None,
        dst_extents: extents(dst),
        dst_length: None,
        data_sha256_hash: hash,
        src_sha256_hash: src_hash,
        frame_table: frame_table,
    };
}

fn extents(spans: &[Span]) -> Vec<Extent> {
    return spans
        .iter()
        .map(|s| Extent {
            start_byte: Some(s.start),
            num_bytes: Some(s.len),
        })
        .collect();
}

pub(crate) fn placeholder() -> Vec<u8> {
    return vec![0u8; SHA256_SIZE];
}

pub(crate) fn raw_replace_wire(dst: &[Span], cursor: u64, len: u64, hash: Vec<u8>) -> Operation {
    return wire(TYPE_REPLACE, dst, &[], cursor, len, Some(hash), None, None);
}

pub(crate) fn zero_wire(op: &Op, cursor: u64) -> Operation {
    return wire(TYPE_ZERO, &op.dst, &[], cursor, 0, None, None, None);
}

fn zero_op(op: &Op, target: &mut Target, cursor: u64) -> Operation {
    for s in &op.dst {
        target.write(*s, Origin::Zero);
    }
    return zero_wire(op, cursor);
}

fn copy_op(
    op: &Op,
    target: &mut Target,
    images: &Images,
    cursor: u64,
    finalize: bool,
    ctl: &Control,
) -> Step<(Operation, u64)> {
    if op.kind == Kind::TargetCopy {
        for s in &op.src {
            for r in target.runs_in(*s) {
                if let Origin::Old(x) = r.origin {
                    if x + r.len > images.old.len() as u64 {
                        return Err(Halt::Failed(err(
                            Code::source_mismatch,
                            "target copy reads uninitialized growth bytes",
                        )));
                    }
                }
            }
        }
    }
    let pairs = zip_streams(&op.src, &op.dst);
    let src_hash = if finalize {
        let bytes = match op.kind {
            Kind::SourceCopy => gather_bytes(&op.src, images.old),
            _ => target.gather(&op.src, images),
        };
        Some(sha256::hash_bytes(&bytes).to_vec())
    } else {
        Some(placeholder())
    };
    for (s, d) in pairs {
        ctl.checkpoint()?;
        match op.kind {
            Kind::SourceCopy => target.write(d, Origin::Old(s.start)),
            _ => {
                for r in target.runs_in(s) {
                    let at = d.start + (r.start - s.start);
                    target.write(
                        Span {
                            start: at,
                            len: r.len,
                        },
                        r.origin,
                    );
                }
            }
        }
    }
    return Ok((copy_wire(op, cursor, src_hash), 0));
}

pub(crate) fn copy_wire(op: &Op, cursor: u64, src_hash: Option<Vec<u8>>) -> Operation {
    let kind = if op.kind == Kind::SourceCopy {
        TYPE_SOURCECOPY
    } else {
        TYPE_TARGET_COPY
    };
    return wire(kind, &op.dst, &op.src, cursor, 0, None, src_hash, None);
}

fn replace_op(
    op: &Op,
    target: &mut Target,
    images: &Images,
    cursor: u64,
    blobs: &mut Option<Vec<u8>>,
    ctl: &Control,
) -> Step<(Operation, u64)> {
    let stream = match total(&op.dst) {
        None => return Err(Halt::Failed(err(Code::overflow, "destination total overflows"))),
        Some(t) => t,
    };
    let data_start = match blobs {
        None => 0usize,
        Some(b) => b.len(),
    };
    for s in &op.dst {
        ctl.checkpoint()?;
        target.write(*s, Origin::New(s.start));
        if let Some(bytes) = blobs {
            bytes.extend_from_slice(&images.new[s.start as usize..s.end() as usize]);
        }
    }
    let hash = match blobs {
        None => placeholder(),
        Some(bytes) => sha256::hash_bytes(&bytes[data_start..]).to_vec(),
    };
    return Ok((raw_replace_wire(&op.dst, cursor, stream, hash), stream));
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Coded {
    pub data_len: u64,
    pub table: Vec<u8>,
}

pub(crate) fn encode_op<M>(
    op: &Op,
    chunk: usize,
    frames: &mut FrameCache,
    new: &[u8],
    mut blob: Option<&mut Vec<u8>>,
    ctl: &Control,
    mut mode: M,
) -> Step<Coded>
where
    M: FnMut(usize, &[Span]) -> Step<Chunk>,
{
    let stream = total(&op.dst).unwrap_or(0);
    let chunk64 = chunk as u64;
    let count = stream.div_ceil(chunk64);
    let sparse = op.kind == Kind::Sparse;
    let mut modes: Vec<u8> = Vec::new();
    if sparse {
        modes.resize(count.div_ceil(4) as usize, 0);
    }
    let mut table: Vec<u8> = Vec::new();
    let mut data_len: u64 = 0;
    let mut cut = Chunks::new(&op.dst, chunk64);
    let mut k = 0usize;
    while let Some((bytes, pieces)) = cut.next_checked(ctl)? {
        let chunk_mode = mode(k, &pieces)?;
        debug_assert!(sparse || chunk_mode == Chunk::Content, "only sparse chunks are kept or zeroed");
        match chunk_mode {
            Chunk::Keep => {}
            Chunk::Zero => {
                modes[k / 4] |= 1 << (2 * (k % 4));
            }
            Chunk::Content => {
                let frame: Option<Vec<u8>> = match op.level {
                    None => None,
                    Some(l) => frames.lookup(&pieces, l, new, ctl)?,
                };
                let stored: u64 = match &frame {
                    None => bytes,
                    Some(f) => f.len() as u64,
                };
                match op.kind {
                    Kind::Frames => push_varint(&mut table, stored_entry(&frame)),
                    Kind::Sparse => match &frame {
                        None => modes[k / 4] |= 2 << (2 * (k % 4)),
                        Some(f) => {
                            modes[k / 4] |= 3 << (2 * (k % 4));
                            push_varint(&mut table, f.len() as u64);
                        }
                    },
                    _ => {}
                };
                if let Some(out) = blob.as_mut() {
                    match &frame {
                        None => out.extend_from_slice(&gather_bytes(&pieces, new)),
                        Some(f) => out.extend_from_slice(f),
                    };
                }
                data_len += stored;
            }
        }
        k += 1;
    }
    let table = if sparse {
        let mut packed = modes;
        packed.extend_from_slice(&table);
        packed
    } else {
        table
    };
    return Ok(Coded {
        data_len: data_len,
        table: table,
    });
}

pub(crate) fn packed_mode(modes: &[u8], k: usize) -> Chunk {
    return match (modes[k / 4] >> (2 * (k % 4))) & 3 {
        0 => Chunk::Keep,
        1 => Chunk::Zero,
        _ => Chunk::Content,
    };
}

fn data_op(
    op: &Op,
    target: &mut Target,
    images: &Images,
    chunk: usize,
    frames: &mut FrameCache,
    cursor: u64,
    blobs: &mut Option<Vec<u8>>,
    ctl: &Control,
) -> Step<(Operation, u64)> {
    if total(&op.dst).is_none() {
        return Err(Halt::Failed(err(Code::overflow, "destination total overflows")));
    }
    let sparse = op.kind == Kind::Sparse;
    let data_start = match blobs {
        None => 0usize,
        Some(b) => b.len(),
    };
    let coded = encode_op(
        op,
        chunk,
        frames,
        images.new,
        blobs.as_mut(),
        ctl,
        |_, pieces| {
            let chunk_mode = if !sparse {
                Chunk::Content
            } else if target.holds_new(pieces, images, ctl)? {
                Chunk::Keep
            } else if images.new_is_zero(pieces, ctl)? {
                Chunk::Zero
            } else {
                Chunk::Content
            };
            match chunk_mode {
                Chunk::Keep => {}
                Chunk::Zero => {
                    for p in pieces {
                        target.write(*p, Origin::Zero);
                    }
                }
                Chunk::Content => {
                    for p in pieces {
                        target.write(*p, Origin::New(p.start));
                    }
                }
            }
            return Ok(chunk_mode);
        },
    )?;
    let kind = match op.kind {
        Kind::Frames => TYPE_REPLACE_FRAMES,
        _ => TYPE_REPLACE_SPARSE,
    };
    let hash = match blobs {
        None => {
            if coded.data_len == 0 {
                sha256::hash_bytes(&[]).to_vec()
            } else {
                placeholder()
            }
        }
        Some(bytes) => sha256::hash_bytes(&bytes[data_start..]).to_vec(),
    };
    return Ok((
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
    ));
}

fn stored_entry(frame: &Option<Vec<u8>>) -> u64 {
    return match frame {
        None => 0,
        Some(f) => f.len() as u64,
    };
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::control::test_clocks::Ticking;
    use crate::program::Kind;
    use crate::program::Program;
    use crate::program::Span;
    use ota_payload::error::PayloadError;

    fn s(start: u64, len: u64) -> Span {
        return Span { start: start, len: len };
    }

    fn run(
        program: &[Op],
        old: &[u8],
        new: &[u8],
        chunk: usize,
    ) -> Result<Mapped, PayloadError> {
        let images = Images { old: old, new: new };
        let mapping = Mapping::new(images, chunk);
        return match mapping.map(program, &Control::unbounded(), true) {
            Err(Halt::Failed(e)) => Err(e),
            Err(Halt::Deadline) => panic!("an unbounded mapping cannot time out"),
            Ok(m) => Ok(m),
        };
    }

    #[test]
    fn a_raw_baseline_is_complete_for_any_length() {
        let old = vec![0u8; 100];
        let mut new = vec![0u8; 100];
        for k in 0..73 {
            new[19 + k] = (k as u8).wrapping_mul(7).wrapping_add(1);
        }
        let program = vec![Op::new(Kind::Replace, vec![s(19, 73)])];
        let mapped = run(&program, &old, &new, 4).unwrap_or_else(|e| panic!("{}", e.msg));
        assert!(mapped.mismatches.is_empty());
        assert_eq!(mapped.blob_len, 73);
        assert_eq!(mapped.ops[0].dst_extents[0].start_byte, Some(19));
        assert_eq!(mapped.ops[0].dst_extents[0].num_bytes, Some(73));
    }

    #[test]
    fn sparse_chunks_that_already_hold_the_new_bytes_are_kept() {
        let old = vec![5u8, 5, 5, 5, 0, 0, 0, 0];
        let new = vec![5u8, 5, 5, 5, 1, 2, 3, 4];
        let program = vec![Op::new(Kind::Sparse, vec![s(0, 8)])];
        let mapped = run(&program, &old, &new, 4).unwrap_or_else(|e| panic!("{}", e.msg));
        assert!(mapped.mismatches.is_empty());
        let packed = mapped.ops[0].frame_table.clone().unwrap_or_default();
        assert_eq!(packed[0] & 0x0f, 0b1000);
        assert_eq!(mapped.blob_len, 4);
    }

    #[test]
    fn a_zero_run_then_patch_completes_when_the_patch_overwrites() {
        let old = vec![9u8; 8];
        let new = vec![0u8, 0, 7, 7, 0, 0, 0, 0];
        let program = vec![
            Op::new(Kind::Zero, vec![s(0, 8)]),
            Op::new(Kind::Replace, vec![s(2, 2)]),
        ];
        let mapped = run(&program, &old, &new, 4).unwrap_or_else(|e| panic!("{}", e.msg));
        assert!(mapped.mismatches.is_empty());
        assert_eq!(mapped.ops.len(), 2);
    }

    #[test]
    fn an_incomplete_program_reports_exactly_its_mismatches() {
        let old = vec![1u8; 10];
        let new = vec![2u8, 2, 2, 1, 1, 2, 2, 1, 1, 1];
        let program = vec![Op::new(Kind::Replace, vec![s(0, 3)])];
        let mapped = run(&program, &old, &new, 4).unwrap_or_else(|e| panic!("{}", e.msg));
        assert_eq!(mapped.mismatches, vec![s(5, 2)]);
    }

    #[test]
    fn source_copies_read_the_old_image_at_any_offset() {
        let old: Vec<u8> = (0..32u8).collect();
        let mut new = old.clone();
        new[10..17].copy_from_slice(&old[3..10]);
        let program = vec![Op::copy(Kind::SourceCopy, vec![s(10, 7)], vec![s(3, 7)])];
        let mapped = run(&program, &old, &new, 4).unwrap_or_else(|e| panic!("{}", e.msg));
        assert_eq!(mapped.ops[0].r#type, TYPE_SOURCECOPY);
        assert_eq!(mapped.ops[0].src_extents[0].start_byte, Some(3));
        assert!(mapped.mismatches.is_empty());
        let none: Program = Vec::new();
        let bare = run(&none, &old, &new, 4).unwrap_or_else(|e| panic!("{}", e.msg));
        assert_eq!(bare.mismatches, vec![s(10, 7)]);
    }

    #[test]
    fn an_in_target_copy_reads_what_an_earlier_write_left() {
        let old = vec![4u8; 12];
        let mut new = old.clone();
        new[0..4].fill(0);
        new[8..12].fill(0);
        let program = vec![
            Op::new(Kind::Zero, vec![s(0, 4)]),
            Op::copy(Kind::TargetCopy, vec![s(8, 4)], vec![s(0, 4)]),
        ];
        let mapped = run(&program, &old, &new, 4).unwrap_or_else(|e| panic!("{}", e.msg));
        assert!(mapped.mismatches.is_empty());
        assert_eq!(mapped.ops[1].r#type, TYPE_TARGET_COPY);
    }

    #[test]
    fn an_in_target_copy_onto_its_own_source_is_refused() {
        let old = vec![0u8; 8];
        let new = old.clone();
        let program = vec![Op::copy(Kind::TargetCopy, vec![s(2, 4)], vec![s(0, 4)])];
        let refused = run(&program, &old, &new, 4);
        assert_eq!(refused.err().map(|e| e.code), Some(Code::copy_overlap));
    }

    #[test]
    fn a_finalized_raw_replacement_carries_its_extents_in_declared_order() {
        let old = vec![0u8; 16];
        let new: Vec<u8> = (0..16u8).collect();
        let program = vec![
            Op::new(Kind::Replace, vec![s(10, 2), s(0, 3)]),
            Op::new(Kind::Replace, vec![s(3, 7), s(12, 4)]),
        ];
        let mapped = run(&program, &old, &new, 4).unwrap_or_else(|e| panic!("{}", e.msg));
        assert!(mapped.mismatches.is_empty());
        let blobs = mapped.blobs.clone().unwrap_or_default();
        assert_eq!(blobs.len(), 16);
        assert_eq!(&blobs[..5], &[10, 11, 0, 1, 2]);
        assert_eq!(mapped.ops[0].data_length, Some(5));
        assert_eq!(mapped.ops[0].data_sha256_hash.clone(), Some(sha256::hash_bytes(&blobs[..5]).to_vec()));
        assert_eq!(mapped.ops[0].dst_extents[0].start_byte, Some(10));
    }

    #[test]
    fn a_raw_replacement_is_replayed_per_extent_not_per_codec_chunk() {
        let old = vec![0u8; 1 << 16];
        let new: Vec<u8> = (0..1usize << 16).map(|k| (k % 251) as u8 + 1).collect();
        let images = Images { old: &old, new: &new };
        let mapping = Mapping::new(images, 1);
        let clock = Ticking::new(0.0);
        let control = Control::new(&clock, f64::INFINITY, 1.0, 0, None);
        let program = vec![Op::new(Kind::Replace, vec![s(0, 1 << 16)])];
        let mapped = match mapping.map(&program, &control, false) {
            Err(_) => panic!("a raw replacement of the whole image must replay"),
            Ok(m) => m,
        };
        assert!(mapped.mismatches.is_empty());
        assert!(clock.reads.get() < 16, "the replay read the control {} times", clock.reads.get());
    }

    fn one_chunk_of_many_extents(extents: u64) -> (Vec<u8>, Vec<u8>, Op) {
        let old = vec![0u8; (2 * extents) as usize];
        let new = vec![1u8; (2 * extents) as usize];
        let op = Op::new(Kind::Sparse, (0..extents).map(|k| s(2 * k, 1)).collect());
        return (old, new, op);
    }

    #[test]
    fn an_expired_control_builds_no_chunk_of_a_many_piece_operation() {
        let (old, new, op) = one_chunk_of_many_extents(48);
        let mapping = Mapping::new(Images { old: &old, new: &new }, 48);
        let clock = Ticking::new(1.0);
        let expired = Control::new(&clock, 0.5, 1.0, 0, None);
        let decided = Cell::new(0usize);
        let step = mapping.encode(&op, &expired, |_, _| {
            decided.set(decided.get() + 1);
            return Ok(Chunk::Content);
        });
        assert!(matches!(step, Err(Halt::Deadline)));
        assert_eq!(decided.get(), 0, "a chunk was decided after the deadline");
        assert_eq!(clock.reads.get(), 1, "the control is read once, before the chunk is built");
    }

    #[test]
    fn a_deadline_inside_one_chunk_stops_after_bounded_work() {
        let (old, new, op) = one_chunk_of_many_extents(10_000);
        let mapping = Mapping::new(Images { old: &old, new: &new }, 10_000);
        let clock = Ticking::new(1.0);
        let control = Control::new(&clock, 3.5, 1.0, 0, None);
        let decided = Cell::new(0usize);
        let step = mapping.encode(&op, &control, |_, _| {
            decided.set(decided.get() + 1);
            return Ok(Chunk::Content);
        });
        assert!(matches!(step, Err(Halt::Deadline)));
        assert_eq!(decided.get(), 0);
        assert_eq!(clock.reads.get(), 4, "the cut read the control {} times", clock.reads.get());
    }

    #[test]
    fn a_replay_interrupted_inside_a_chunk_returns_no_mapping() {
        let (old, new, op) = one_chunk_of_many_extents(10_000);
        let mapping = Mapping::new(Images { old: &old, new: &new }, 10_000);
        let clock = Ticking::new(1.0);
        let control = Control::new(&clock, 3.5, 1.0, 0, None);
        assert!(matches!(mapping.map(&[op], &control, false), Err(Halt::Deadline)));
    }

    #[test]
    #[ignore]
    fn one_byte_chunks_over_many_short_extents_are_timed() {
        let old: Vec<u8> = (0..640_000usize).map(|k| (k * 31 % 251) as u8).collect();
        let mut new = old.clone();
        for k in 0..20_000usize {
            new[k * 32 + 3] ^= 0x5a;
        }
        let program = vec![Op::new(Kind::Sparse, (0..20_000u64).map(|k| s(k * 32, 8)).collect())];
        let images = Images { old: &old, new: &new };
        let mapping = Mapping::new(images, 1);
        let started = std::time::Instant::now();
        let mapped = match mapping.map(&program, &Control::unbounded(), false) {
            Err(_) => panic!("a valid program maps"),
            Ok(m) => m,
        };
        eprintln!(
            "one replay, 160000 one-byte chunks over 20000 extents: {:.3} s, blob {} bytes",
            started.elapsed().as_secs_f64(),
            mapped.blob_len
        );
    }
}
