//! Block encodings for the project's compressed and sparse operations.
//!
//! A compressed chunk is one complete zlib stream (RFC 1950) wrapping one
//! DEFLATE stream (RFC 1951), produced from exactly one chunk with a fresh
//! compressor. Each frame therefore starts with an empty history and decodes
//! on its own. The wire format pins zlib; miniz is only the implementation,
//! and `docs/format.md` states that distinction.
//!
//! [`BlockDecoder`] is the one decoder both replay pipelines use. It writes
//! into a caller-provided chunk and keeps no history of its own: the output
//! chunk is the window (`TINFL_FLAG_USING_NON_WRAPPING_OUTPUT_BUF`), so no
//! separate 32 KiB ring is allocated. Its only allocation is the fixed-size
//! decompressor state, which is about 10 KiB and is measured in
//! `docs/memory.md`.
//!
//! The decoder accepts a frame only if it decodes to exactly its chunk and the
//! stored bytes end exactly at the end of the zlib stream. The zlib Adler-32 is
//! checked by the decoder. It detects corruption and has no authentication
//! value. Authenticity comes from the operation's `data_sha256_hash`, which the
//! signed manifest carries.

use crate::error::Code;
use crate::error::PayloadError;
use crate::error::err;
use crate::profile::ResourceProfile;
use alloc::boxed::Box;
use alloc::vec::Vec;
use miniz_oxide::inflate::TINFLStatus;
use miniz_oxide::inflate::core::DecompressorOxide;
use miniz_oxide::inflate::core::decompress;
use miniz_oxide::inflate::core::inflate_flags::TINFL_FLAG_PARSE_ZLIB_HEADER;
use miniz_oxide::inflate::core::inflate_flags::TINFL_FLAG_USING_NON_WRAPPING_OUTPUT_BUF;

/// Bytes of decoder state held by a [`BlockDecoder`], for the memory model.
pub const DECODER_STATE_BYTES: usize = core::mem::size_of::<DecompressorOxide>();

/// Decodes single-chunk zlib frames into caller-provided buffers.
pub struct BlockDecoder {
    state: Box<DecompressorOxide>,
}

impl BlockDecoder {
    /// Allocate the decoder state. Dropping the decoder releases it, so a
    /// caller can end the codec's lifetime before a later phase.
    pub fn new() -> BlockDecoder {
        return BlockDecoder {
            state: Box::new(DecompressorOxide::default()),
        };
    }

    /// Decode one zlib frame into `out`, which is exactly its chunk. `stored`
    /// must be exactly the frame's bytes: the stream has to end at its last
    /// byte, and it has to produce exactly `out.len()` bytes.
    pub fn decode(&mut self, stored: &[u8], out: &mut [u8]) -> Result<(), PayloadError> {
        if !zlib_header_ok(stored) {
            return Err(err(
                Code::frame_malformed,
                "frame is not a zlib stream without a preset dictionary",
            ));
        }
        self.state.init();
        let flags = TINFL_FLAG_PARSE_ZLIB_HEADER | TINFL_FLAG_USING_NON_WRAPPING_OUTPUT_BUF;
        let (status, consumed, produced) = decompress(&mut self.state, stored, out, 0, flags);
        if status != TINFLStatus::Done {
            return Err(err(
                Code::frame_malformed,
                "zlib frame does not decode to exactly one chunk",
            ));
        }
        if consumed != stored.len() {
            return Err(err(
                Code::frame_malformed,
                "bytes after the end of a zlib frame",
            ));
        }
        if produced != out.len() {
            return Err(err(
                Code::frame_malformed,
                "zlib frame decodes to a short chunk",
            ));
        }
        return Ok(());
    }
}

const MAX_VARINT_BYTES: usize = 10;

/// Reads canonical unsigned LEB128 varints from a packed table. A value must use
/// the fewest bytes, so every value has exactly one encoding and a signed table
/// has one meaning. Non-minimal encodings, values past 64 bits, and truncation
/// are all `frame_table_invalid`.
pub struct Varints<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> Varints<'a> {
    pub fn new(bytes: &'a [u8]) -> Varints<'a> {
        return Varints {
            bytes: bytes,
            pos: 0,
        };
    }

    /// The next entry of a table whose length the plan has already checked. A
    /// missing entry is an error, not a default, so replay cannot run past the
    /// table's count.
    pub fn next_entry(&mut self) -> Result<u64, PayloadError> {
        return match self.next() {
            Err(e) => Err(e),
            Ok(None) => Err(table_error("frame table ended before its last chunk")),
            Ok(Some(v)) => Ok(v),
        };
    }

    /// The next value, or `None` once the table is exhausted.
    pub fn next(&mut self) -> Result<Option<u64>, PayloadError> {
        if self.pos >= self.bytes.len() {
            return Ok(None);
        }
        let start = self.pos;
        let mut value: u64 = 0;
        loop {
            if self.pos >= self.bytes.len() {
                return Err(table_error("truncated varint in frame table"));
            }
            let byte = self.bytes[self.pos];
            self.pos += 1;
            let count = self.pos - start;
            let bits = (byte & 0x7f) as u64;
            if count == MAX_VARINT_BYTES && (bits > 1 || byte & 0x80 != 0) {
                return Err(table_error("varint in frame table exceeds 64 bits"));
            }
            value |= bits << (7 * (count - 1));
            if byte & 0x80 == 0 {
                if count > 1 && byte == 0 {
                    return Err(table_error("non-canonical varint in frame table"));
                }
                return Ok(Some(value));
            }
        }
    }
}

/// The number of chunks an operation of `total` destination bytes is cut into:
/// `ceil(total / chunk)`. `None` when `chunk` is zero or the count overflows.
pub fn chunk_count(total: u64, chunk: u64) -> Option<u64> {
    if chunk == 0 {
        return None;
    }
    let whole = total / chunk;
    if total % chunk == 0 {
        return Some(whole);
    }
    return whole.checked_add(1);
}

/// The length of chunk `index` of an operation of `total` bytes. Every chunk is
/// `chunk` bytes except the last, which holds the remainder. The caller has
/// checked `index` against [`chunk_count`], so the result is never zero.
pub fn chunk_len(total: u64, chunk: u64, index: u64) -> u64 {
    let start = index.saturating_mul(chunk);
    let rest = total.saturating_sub(start);
    if rest < chunk {
        return rest;
    }
    return chunk;
}

/// The blob length a frame table implies, after checking it against the
/// operation's chunks. The table must hold exactly one entry per chunk. An entry
/// is 0 for a raw chunk, or a stored length in `1..chunk_len` for a zlib frame.
/// Stored lengths must be strictly less than their chunk, because a frame that
/// is not smaller than raw is stored raw.
pub fn frame_table_blob_len(table: &[u8], total: u64, chunk: u64) -> Result<u64, PayloadError> {
    let chunks = match chunk_count(total, chunk) {
        None => return Err(table_error("frame table covers no chunks")),
        Some(n) => n,
    };
    let mut reader = Varints::new(table);
    let mut sum: u64 = 0;
    let mut i: u64 = 0;
    while i < chunks {
        let value = match reader.next() {
            Err(e) => return Err(e),
            Ok(None) => return Err(table_error("frame table does not have one entry per chunk")),
            Ok(Some(v)) => v,
        };
        let len = chunk_len(total, chunk, i);
        let stored = if value == 0 {
            len
        } else {
            if value >= len {
                return Err(table_error("stored frame is not smaller than its chunk"));
            }
            value
        };
        sum = match sum.checked_add(stored) {
            None => return Err(table_error("frame table blob length overflows")),
            Some(t) => t,
        };
        i += 1;
    }
    match reader.next() {
        Err(e) => return Err(e),
        Ok(None) => {}
        Ok(Some(_)) => return Err(table_error("frame table does not have one entry per chunk")),
    };
    return Ok(sum);
}

/// Append `value` as a canonical varint: the fewest bytes, least significant
/// group first. The inverse of [`Varints::next`].
pub fn push_varint(out: &mut Vec<u8>, value: u64) {
    let mut rest = value;
    loop {
        let low = (rest & 0x7f) as u8;
        rest >>= 7;
        if rest == 0 {
            out.push(low);
            return;
        }
        out.push(low | 0x80);
    }
}

/// How one chunk of a `REPLACE_SPARSE` operation is produced (`docs/format.md` §4.2).
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Mode {
    /// The destination already holds the new content, so the chunk is not written.
    Keep,
    /// The chunk is written as zeros.
    Zero,
    /// The next `chunk_len` bytes of the blob are the chunk.
    Raw,
    /// The next zlib frame of the blob is the chunk. Its length is the next
    /// frame-table entry.
    Zlib,
}

/// The error for a chunk the map does not cover. A validated operation never
/// reaches it, so replay uses it as a guard rather than a path.
pub fn map_does_not_cover() -> PayloadError {
    return err(
        Code::block_map_invalid,
        "block map does not cover the operation",
    );
}

/// Bytes a two-bit chunk map needs for `chunks` chunks, or `None` on overflow.
pub fn map_len(blocks: u64) -> Option<u64> {
    let whole = blocks / 4;
    if blocks % 4 == 0 {
        return Some(whole);
    }
    return whole.checked_add(1);
}

/// The mode of chunk `block` in a map, or `None` when the map does not cover it.
pub fn mode_at(map: &[u8], block: u64) -> Option<Mode> {
    let index = usize::try_from(block / 4).ok()?;
    let byte = *map.get(index)?;
    let shift = 2 * (block % 4);
    return Some(match (byte >> shift) & 3 {
        0 => Mode::Keep,
        1 => Mode::Zero,
        2 => Mode::Raw,
        _ => Mode::Zlib,
    });
}

/// Split a `REPLACE_SPARSE` operation's packed field into its block map and its
/// frame table. The map has the canonical length for `blocks`, and the table is
/// the rest.
pub fn split_sparse(packed: &[u8], blocks: u64) -> Result<(&[u8], &[u8]), PayloadError> {
    let want = match map_len(blocks) {
        None => return Err(err(Code::block_map_invalid, "block map length overflows")),
        Some(v) => v,
    };
    let cut = match usize::try_from(want) {
        Err(_) => return Err(err(Code::block_map_invalid, "block map exceeds memory")),
        Ok(v) => v,
    };
    if cut > packed.len() {
        return Err(err(
            Code::block_map_invalid,
            "packed data is shorter than the block map",
        ));
    }
    return Ok((&packed[..cut], &packed[cut..]));
}

/// Check the components of a packed `frame_table` field against the profile. The
/// field of a `REPLACE_FRAMES` operation is one table, so `sparse` is false. The
/// field of a `REPLACE_SPARSE` operation over `chunks` chunks is its canonical chunk
/// map, then a table. Each component has its own limit, so an unused byte of one
/// cannot cover the other. The plan calls this once the chunk count is known; the
/// prescan bounds the combined field before that, since block size is not yet known.
pub fn check_packed_components(
    sparse: bool,
    chunks: u64,
    packed_len: usize,
    profile: &ResourceProfile,
) -> Result<(), PayloadError> {
    let map = if sparse {
        match map_len(chunks) {
            None => usize::MAX,
            Some(m) => usize::try_from(m).unwrap_or(usize::MAX),
        }
    } else {
        0
    };
    if map > profile.max_map_bytes {
        return Err(err(Code::resource_limit, "block map exceeds the profile"));
    }
    if packed_len.saturating_sub(map) > profile.max_frame_table_bytes {
        return Err(err(Code::resource_limit, "frame table exceeds the profile"));
    }
    return Ok(());
}

/// The blob length a sparse operation implies, after checking its chunk map and
/// frame table against the operation's chunks. The map must have unused bits
/// clear. The table must hold exactly one entry per zlib chunk, each in
/// `1..chunk_len`. Any violation is `block_map_invalid` or `frame_table_invalid`,
/// and nothing is written in either case.
pub fn sparse_blob_len(packed: &[u8], total: u64, chunk: u64) -> Result<u64, PayloadError> {
    let chunks = match chunk_count(total, chunk) {
        None => return Err(err(Code::block_map_invalid, "operation covers no chunks")),
        Some(n) => n,
    };
    let (map, table) = match split_sparse(packed, chunks) {
        Err(e) => return Err(e),
        Ok(v) => v,
    };
    let rest = chunks % 4;
    if rest != 0 && (map[map.len() - 1] >> (2 * rest)) != 0 {
        return Err(err(
            Code::block_map_invalid,
            "unused bits of the chunk map are set",
        ));
    }
    let mut entries = Varints::new(table);
    let mut sum: u64 = 0;
    let mut i: u64 = 0;
    while i < chunks {
        let mode = match mode_at(map, i) {
            None => return Err(map_does_not_cover()),
            Some(m) => m,
        };
        let len = chunk_len(total, chunk, i);
        let stored = match mode {
            Mode::Keep | Mode::Zero => 0,
            Mode::Raw => len,
            Mode::Zlib => {
                let n = match entries.next_entry() {
                    Err(e) => return Err(e),
                    Ok(v) => v,
                };
                if n == 0 || n >= len {
                    return Err(table_error("stored frame is not smaller than its chunk"));
                }
                n
            }
        };
        sum = match sum.checked_add(stored) {
            None => return Err(table_error("frame table blob length overflows")),
            Some(t) => t,
        };
        i += 1;
    }
    match entries.next() {
        Err(e) => return Err(e),
        Ok(None) => {}
        Ok(Some(_)) => {
            return Err(table_error("frame table has entries for no zlib block"));
        }
    };
    return Ok(sum);
}

fn table_error(msg: &'static str) -> PayloadError {
    return err(Code::frame_table_invalid, msg);
}

fn zlib_header_ok(stored: &[u8]) -> bool {
    if stored.len() < 2 {
        return false;
    }
    let cmf = stored[0];
    let flg = stored[1];
    let method_deflate = (cmf & 0x0f) == 8;
    let window_ok = (cmf >> 4) <= 7;
    let no_dictionary = (flg & 0x20) == 0;
    let checksum_ok = ((cmf as u16) * 256 + (flg as u16)) % 31 == 0;
    return method_deflate && window_ok && no_dictionary && checksum_ok;
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;
    use alloc::vec::Vec;
    use miniz_oxide::deflate::compress_to_vec_zlib;

    const BS: usize = 4096;

    fn noise(len: usize, seed: u64) -> Vec<u8> {
        let mut x = seed | 1;
        let mut out = Vec::with_capacity(len);
        for _ in 0..len {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            out.push((x >> 24) as u8);
        }
        return out;
    }

    fn frame_of(block: &[u8]) -> Vec<u8> {
        return compress_to_vec_zlib(block, 6);
    }

    fn decode(stored: &[u8], out_len: usize) -> Result<Vec<u8>, Code> {
        let mut out = vec![0u8; out_len];
        let mut dec = BlockDecoder::new();
        return match dec.decode(stored, &mut out) {
            Err(e) => Err(e.code),
            Ok(()) => Ok(out),
        };
    }

    #[test]
    fn round_trips_compressible_and_incompressible_blocks_without_expanding_noise() {
        let zeros = vec![0u8; BS];
        let text: Vec<u8> = (0..BS).map(|i| b"update engine "[i % 14]).collect();
        let noise_blocks = [noise(BS, 0x9e37_79b9), noise(BS, 7)];
        for block in [&zeros, &text, &noise_blocks[0], &noise_blocks[1]] {
            let frame = frame_of(block);
            assert_eq!(decode(&frame, BS).unwrap(), *block);
        }
        assert!(frame_of(&zeros).len() < 64);
        for block in &noise_blocks {
            assert!(frame_of(block).len() >= BS);
        }
    }

    #[test]
    fn rejects_a_non_zlib_header() {
        let mut frame = frame_of(&vec![1u8; BS]);
        frame[0] = 0x00;
        assert_eq!(decode(&frame, BS).err(), Some(Code::frame_malformed));
        assert_eq!(decode(&[], BS).err(), Some(Code::frame_malformed));
        assert_eq!(decode(&[0x78], BS).err(), Some(Code::frame_malformed));
    }

    #[test]
    fn rejects_a_preset_dictionary() {
        let mut frame = frame_of(&vec![1u8; BS]);
        frame[0] = 0x78;
        frame[1] = 0xbb;
        assert_eq!(decode(&frame, BS).err(), Some(Code::frame_malformed));
    }

    #[test]
    fn rejects_truncated_and_trailing_bytes() {
        let frame = frame_of(&noise(BS, 3));
        assert_eq!(
            decode(&frame[..frame.len() - 1], BS).err(),
            Some(Code::frame_malformed)
        );
        let mut trailing = frame.clone();
        trailing.push(0);
        assert_eq!(decode(&trailing, BS).err(), Some(Code::frame_malformed));
    }

    #[test]
    fn rejects_output_longer_or_shorter_than_one_block() {
        let long = frame_of(&vec![7u8; 2 * BS]);
        assert_eq!(decode(&long, BS).err(), Some(Code::frame_malformed));
        let short = frame_of(&vec![7u8; BS / 2]);
        assert_eq!(decode(&short, BS).err(), Some(Code::frame_malformed));
    }

    #[test]
    fn rejects_a_bad_checksum() {
        let mut frame = frame_of(&noise(BS, 11));
        let last = frame.len() - 1;
        frame[last] ^= 1;
        assert_eq!(decode(&frame, BS).err(), Some(Code::frame_malformed));
    }

    #[test]
    fn garbage_never_panics_and_never_decodes_to_the_wrong_block() {
        let block = noise(BS, 99);
        let frame = frame_of(&block);
        for i in 0..frame.len() {
            let mut bad = frame.clone();
            bad[i] ^= 0x5a;
            match decode(&bad, BS) {
                Err(_) => {}
                Ok(out) => assert_eq!(out, block, "flip at byte {}", i),
            }
        }
    }

    fn varint_values(bytes: &[u8]) -> Result<Vec<u64>, Code> {
        let mut reader = Varints::new(bytes);
        let mut out = Vec::new();
        loop {
            match reader.next() {
                Err(e) => return Err(e.code),
                Ok(None) => return Ok(out),
                Ok(Some(v)) => out.push(v),
            }
        }
    }

    #[test]
    fn varints_round_trip_at_every_length_boundary() {
        let values = [0u64, 1, 127, 128, 255, 16383, 16384, 1 << 35, u64::MAX];
        let mut table = Vec::new();
        for v in values {
            push_varint(&mut table, v);
        }
        assert_eq!(varint_values(&table).unwrap(), values.to_vec());
        let mut one = Vec::new();
        push_varint(&mut one, 127);
        assert_eq!(one, [0x7f]);
        let mut two = Vec::new();
        push_varint(&mut two, 128);
        assert_eq!(two, [0x80, 0x01]);
    }

    #[test]
    fn non_canonical_truncated_and_overlong_varints_are_refused() {
        assert_eq!(
            varint_values(&[0x80, 0x00]).err(),
            Some(Code::frame_table_invalid)
        );
        assert_eq!(
            varint_values(&[0x80, 0x80, 0x00]).err(),
            Some(Code::frame_table_invalid)
        );
        assert_eq!(
            varint_values(&[0x80]).err(),
            Some(Code::frame_table_invalid)
        );
        let mut eleven = [0xffu8; 11];
        eleven[10] = 0x01;
        assert_eq!(
            varint_values(&eleven).err(),
            Some(Code::frame_table_invalid)
        );
        let mut overflow = [0xffu8; 10];
        overflow[9] = 0x02;
        assert_eq!(
            varint_values(&overflow).err(),
            Some(Code::frame_table_invalid)
        );
        let mut max = [0xffu8; 10];
        max[9] = 0x01;
        assert_eq!(varint_values(&max).unwrap(), alloc::vec![u64::MAX]);
    }

    #[test]
    fn frame_table_must_have_one_entry_per_chunk_and_stored_lengths_below_their_chunk() {
        const B: u64 = 512;
        let mut table = Vec::new();
        push_varint(&mut table, 0);
        push_varint(&mut table, 100);
        assert_eq!(frame_table_blob_len(&table, 2 * B, B).unwrap(), B + 100);
        let code = |t: &[u8], total: u64| frame_table_blob_len(t, total, B).err().map(|e| e.code);
        assert_eq!(code(&table, 3 * B), Some(Code::frame_table_invalid));
        assert_eq!(code(&table, B), Some(Code::frame_table_invalid));
        let mut full = Vec::new();
        push_varint(&mut full, B);
        assert_eq!(code(&full, B), Some(Code::frame_table_invalid));
        let mut partial = Vec::new();
        push_varint(&mut partial, 0);
        push_varint(&mut partial, 100);
        assert_eq!(code(&partial, B + 100), Some(Code::frame_table_invalid));
        let mut raw_tail = Vec::new();
        push_varint(&mut raw_tail, 0);
        push_varint(&mut raw_tail, 0);
        assert_eq!(
            frame_table_blob_len(&raw_tail, B + 100, B).unwrap(),
            B + 100
        );
        assert_eq!(chunk_count(2 * B + 1, B), Some(3));
        assert_eq!(chunk_len(2 * B + 1, B, 2), 1);
        assert_eq!(chunk_count(0, B), Some(0));
        assert_eq!(frame_table_blob_len(&[], 0, B).unwrap(), 0);
        assert_eq!(code(&[0], 0), Some(Code::frame_table_invalid));
    }
}
