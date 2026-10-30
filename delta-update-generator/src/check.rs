//! The generator's self-check: a payload is accepted only if the client's own rules
//! and a byte-accurate replay agree that it produces the new image.
//!
//! The mapper reasons about runs, so this check does not share its model. It parses
//! the finished bytes, verifies both signatures, runs the metadata policy, and then
//! executes every operation on a flat copy of the old image, decoding each frame. The
//! data and source hashes the manifest claims are checked on the way, and the final
//! image must equal the new one exactly.
//!
//! A full-image payload has no old image, so [`check_full_payload`] checks the rules of
//! that form instead: its operations cover the partition in order, and its replay from
//! several starting contents produces the new image.

use delta_update_payload::encoding::BlockDecoder;
use delta_update_payload::encoding::Mode;
use delta_update_payload::encoding::Varints;
use delta_update_payload::encoding::mode_at;
use delta_update_payload::encoding::split_sparse;
use delta_update_payload::error::Code;
use delta_update_payload::error::PayloadError;
use delta_update_payload::error::err;
use delta_update_payload::manifest::Manifest;
use delta_update_payload::manifest::Operation;
use delta_update_payload::manifest::TYPE_REPLACE;
use delta_update_payload::manifest::TYPE_REPLACE_FRAMES;
use delta_update_payload::manifest::TYPE_REPLACE_SPARSE;
use delta_update_payload::manifest::TYPE_SOURCECOPY;
use delta_update_payload::manifest::TYPE_TARGET_COPY;
use delta_update_payload::manifest::TYPE_ZERO;
use delta_update_payload::plan;
use delta_update_payload::plan::Validated;
use delta_update_payload::profile::ResourceProfile;
use delta_update_payload::rsa::PubKey;
use delta_update_payload::sha256;
use delta_update_payload::verify;

use crate::program::Span;
use crate::program::slice_stream;
use crate::program::total;

/// Verify `payload` under `profile` and `key`, then replay it from `old` and require
/// the result to be `new`. Returns the first rule the payload breaks.
///
/// Every range is bound to the images before anything is replayed, whether or not the manifest
/// records partition sizes, so the replay never indexes outside them.
pub fn check_payload(
    payload: &[u8],
    key: PubKey,
    old: &[u8],
    new: &[u8],
    profile: &ResourceProfile,
) -> Result<(), PayloadError> {
    let opened = match open(payload, key, profile) {
        Err(e) => return Err(e),
        Ok(o) => o,
    };
    match bind_images(opened.validated.manifest(), old, new) {
        Err(e) => return Err(e),
        Ok(()) => {}
    };
    match bind_geometry(opened.validated.manifest(), old, new) {
        Err(e) => return Err(e),
        Ok(()) => {}
    };
    let chunk = opened.validated.geometry().chunk_size as usize;
    return replay(opened.validated.manifest(), opened.blobs, chunk, old, new);
}

struct Opened<'p> {
    validated: Validated,
    blobs: &'p [u8],
}

fn open<'p>(
    payload: &'p [u8],
    key: PubKey,
    profile: &ResourceProfile,
) -> Result<Opened<'p>, PayloadError> {
    let authenticated = match verify::verify_payload(payload, &[key], profile) {
        Err(e) => return Err(e),
        Ok(a) => a,
    };
    let parsed = authenticated.into_parsed();
    let info = parsed.info;
    let manifest = parsed.manifest;
    if manifest.partitions.len() != 1 {
        return Err(err(
            Code::partition_mismatch,
            "the checker replays exactly one partition",
        ));
    }
    let blobs_start = info.blobs_start as usize;
    let blobs_end = blobs_start + info.sig_offset_blobs as usize;
    if blobs_end > payload.len() {
        return Err(err(Code::overflow, "blob region runs past the payload"));
    }
    let blobs = &payload[blobs_start..blobs_end];
    let validated = match plan::validate(manifest, profile) {
        Err(e) => return Err(e),
        Ok(v) => v,
    };
    if validated.geometry().blob_total != blobs.len() as u64 {
        return Err(err(
            Code::parse,
            "blob region length disagrees with the manifest",
        ));
    }
    return Ok(Opened {
        validated: validated,
        blobs: blobs,
    });
}

/// Verify a full-image payload under `profile` and `key`, and require it to write exactly `new` over any
/// starting contents of the same length.
///
/// The payload must record no old partition, and its new partition must be `new`, by length and SHA-256. Its
/// operations must be `REPLACE`, `REPLACE_FRAMES` or `ZERO`, and their destinations must cover the partition
/// in one ascending run with no gap and no overlap. That coverage is what proves every byte is written, since
/// a replay from zeros alone would also match a payload that never writes the zero bytes of `new`. So the
/// operations are then replayed from zeros, from erased flash (`0xff`) and from a fixed pattern, and each
/// replay must produce `new` exactly.
pub fn check_full_payload(
    payload: &[u8],
    key: PubKey,
    new: &[u8],
    profile: &ResourceProfile,
) -> Result<(), PayloadError> {
    let opened = match open(payload, key, profile) {
        Err(e) => return Err(e),
        Ok(o) => o,
    };
    let manifest = opened.validated.manifest();
    match require_full(manifest, new) {
        Err(e) => return Err(e),
        Ok(()) => {}
    };
    match require_coverage(manifest, new.len() as u64) {
        Err(e) => return Err(e),
        Ok(()) => {}
    };
    let chunk = opened.validated.geometry().chunk_size as usize;
    let ops = match manifest.partitions.first() {
        None => return Err(err(Code::partition_mismatch, "no partition")),
        Some(p) => &p.operations,
    };
    for which in 0..STARTS {
        let mut target = starting_contents(which, new.len());
        for op in ops {
            match execute(op, opened.blobs, chunk, &[], &mut target) {
                Err(e) => return Err(e),
                Ok(()) => {}
            };
        }
        if target != new {
            return Err(err(
                Code::blob_mismatch,
                "replayed payload does not produce the new image",
            ));
        }
    }
    return Ok(());
}

const STARTS: usize = 3;

fn starting_contents(which: usize, len: usize) -> Vec<u8> {
    return match which {
        0 => vec![0u8; len],
        1 => vec![0xffu8; len],
        _ => (0..len)
            .map(|i| ((i as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15) >> 56) as u8)
            .collect(),
    };
}

fn require_full(manifest: &Manifest, new: &[u8]) -> Result<(), PayloadError> {
    let partition = match manifest.partitions.first() {
        None => return Err(err(Code::partition_mismatch, "no partition")),
        Some(p) => p,
    };
    if partition.old_partition_info.is_some() {
        return Err(err(
            Code::unsupported_field,
            "a full-image payload records no old partition",
        ));
    }
    let info = match &partition.new_partition_info {
        None => {
            return Err(err(
                Code::required_field_missing,
                "a full-image payload records its new partition",
            ));
        }
        Some(i) => i,
    };
    let size = match info.size {
        None => {
            return Err(err(
                Code::required_field_missing,
                "the new partition size is missing",
            ));
        }
        Some(s) => s,
    };
    if size != new.len() as u64 {
        return Err(err(
            Code::partition_mismatch,
            "the new partition is not the image's length",
        ));
    }
    let want = match &info.hash {
        Some(h) if !h.is_empty() => h,
        _ => {
            return Err(err(
                Code::required_field_missing,
                "the new partition hash is missing",
            ));
        }
    };
    if want[..] != sha256::hash_bytes(new)[..] {
        return Err(err(
            Code::hash_mismatch,
            "the new image does not match the manifest's hash",
        ));
    }
    for op in &partition.operations {
        if op.r#type != TYPE_REPLACE && op.r#type != TYPE_REPLACE_FRAMES && op.r#type != TYPE_ZERO {
            return Err(err(
                Code::unsupported_field,
                "a full-image payload uses only REPLACE, REPLACE_FRAMES and ZERO",
            ));
        }
    }
    return Ok(());
}

fn require_coverage(manifest: &Manifest, len: u64) -> Result<(), PayloadError> {
    let partition = match manifest.partitions.first() {
        None => return Err(err(Code::partition_mismatch, "no partition")),
        Some(p) => p,
    };
    let mut cursor: u64 = 0;
    for op in &partition.operations {
        for s in spans_of(&op.dst_extents) {
            if s.start < cursor {
                return Err(err(
                    Code::extent_overlap,
                    "an operation overlaps bytes an earlier operation wrote",
                ));
            }
            if s.start > cursor {
                return Err(err(
                    Code::partition_mismatch,
                    "the operations leave a gap in the partition",
                ));
            }
            cursor = s.end();
        }
    }
    if cursor != len {
        return Err(err(
            Code::partition_mismatch,
            "the operations do not cover the partition",
        ));
    }
    return Ok(());
}

fn bind_geometry(
    manifest: &delta_update_payload::manifest::Manifest,
    old: &[u8],
    new: &[u8],
) -> Result<(), PayloadError> {
    let partition = match manifest.partitions.first() {
        None => return Err(err(Code::partition_mismatch, "no partition")),
        Some(p) => p,
    };
    let target = new.len() as u64;
    for op in &partition.operations {
        if spans_of(&op.dst_extents).iter().any(|s| s.end() > target) {
            return Err(err(
                Code::extent_out_of_range,
                "an operation writes past the partition",
            ));
        }
        let src = spans_of(&op.src_extents);
        if op.r#type == TYPE_SOURCECOPY && src.iter().any(|s| s.end() > old.len() as u64) {
            return Err(err(
                Code::extent_out_of_range,
                "a source copy reads past the old image",
            ));
        }
        if op.r#type == TYPE_TARGET_COPY && src.iter().any(|s| s.end() > target) {
            return Err(err(
                Code::extent_out_of_range,
                "a target copy reads past the partition",
            ));
        }
    }
    return Ok(());
}

fn bind_images(
    manifest: &delta_update_payload::manifest::Manifest,
    old: &[u8],
    new: &[u8],
) -> Result<(), PayloadError> {
    let partition = match manifest.partitions.first() {
        None => return Err(err(Code::partition_mismatch, "no partition")),
        Some(p) => p,
    };
    match bind_side(&partition.old_partition_info, old, "old") {
        Err(e) => return Err(e),
        Ok(()) => {}
    };
    return bind_side(&partition.new_partition_info, new, "new");
}

fn bind_side(
    info: &Option<delta_update_payload::manifest::PartInfo>,
    image: &[u8],
    side: &'static str,
) -> Result<(), PayloadError> {
    let info = match info {
        None => return Ok(()),
        Some(i) => i,
    };
    if let Some(size) = info.size {
        if size != image.len() as u64 {
            return Err(err(
                Code::partition_mismatch,
                if side == "old" {
                    "the old image is not the size the manifest records"
                } else {
                    "the new image is not the size the manifest records"
                },
            ));
        }
    }
    if let Some(want) = &info.hash {
        if !want.is_empty() && want[..] != sha256::hash_bytes(image)[..] {
            return Err(err(
                Code::hash_mismatch,
                if side == "old" {
                    "the old image does not match the manifest's hash"
                } else {
                    "the new image does not match the manifest's hash"
                },
            ));
        }
    }
    return Ok(());
}

fn spans_of(extents: &[delta_update_payload::manifest::Extent]) -> Vec<Span> {
    return extents
        .iter()
        .map(|e| Span {
            start: e.start_byte.unwrap_or(0),
            len: e.num_bytes.unwrap_or(0),
        })
        .collect();
}

fn replay(
    manifest: &delta_update_payload::manifest::Manifest,
    blobs: &[u8],
    chunk: usize,
    old: &[u8],
    new: &[u8],
) -> Result<(), PayloadError> {
    let partition = match manifest.partitions.first() {
        None => return Err(err(Code::partition_mismatch, "no partition")),
        Some(p) => p,
    };
    let starts = if new.len() > old.len() { STARTS } else { 1 };
    for which in 0..starts {
        let mut target = starting_contents(which, new.len());
        let common = old.len().min(new.len());
        target[..common].copy_from_slice(&old[..common]);
        for op in &partition.operations {
            match execute(op, blobs, chunk, old, &mut target) {
                Err(e) => return Err(e),
                Ok(()) => {}
            };
        }
        if target != new {
            return Err(err(
                Code::blob_mismatch,
                "replayed payload does not produce the new image",
            ));
        }
    }
    return Ok(());
}

fn execute(
    op: &Operation,
    blobs: &[u8],
    chunk: usize,
    old: &[u8],
    target: &mut [u8],
) -> Result<(), PayloadError> {
    let dst = spans_of(&op.dst_extents);
    let stream = match total(&dst) {
        None => return Err(err(Code::overflow, "destination total overflows")),
        Some(t) => t,
    };
    let offset = op.data_offset.unwrap_or(0) as usize;
    let length = op.data_length.unwrap_or(0) as usize;
    if op.r#type == TYPE_SOURCECOPY || op.r#type == TYPE_TARGET_COPY {
        let src = spans_of(&op.src_extents);
        let bytes = if op.r#type == TYPE_SOURCECOPY {
            gather(&src, old)
        } else {
            gather(&src, target)
        };
        let want = match &op.src_sha256_hash {
            None => return Err(err(Code::required_field_missing, "copy has no source hash")),
            Some(h) => h,
        };
        if want[..] != sha256::hash_bytes(&bytes)[..] {
            return Err(err(
                Code::source_mismatch,
                "copy source does not match its hash",
            ));
        }
        scatter(&dst, 0, &bytes, target);
        return Ok(());
    }
    if op.r#type == TYPE_ZERO {
        for s in &dst {
            target[s.start as usize..s.end() as usize].fill(0);
        }
        return Ok(());
    }
    let data = &blobs[offset..offset + length];
    match &op.data_sha256_hash {
        None => {
            return Err(err(
                Code::required_field_missing,
                "data operation has no hash",
            ));
        }
        Some(h) => {
            if h[..] != sha256::hash_bytes(data)[..] {
                return Err(err(Code::blob_mismatch, "operation data hash mismatch"));
            }
        }
    };
    if op.r#type == TYPE_REPLACE {
        scatter(&dst, 0, &data[..stream as usize], target);
        return Ok(());
    }
    let count = stream.div_ceil(chunk as u64);
    let table: &[u8] = match &op.frame_table {
        None => &[],
        Some(t) => &t[..],
    };
    let mut decoder = BlockDecoder::new();
    let mut frame_buf = match frame_buffer(frame_len(chunk, stream)) {
        Err(e) => return Err(e),
        Ok(b) => b,
    };
    let mut pos: usize = 0;
    let (map, entries_bytes): (&[u8], &[u8]) = if op.r#type == TYPE_REPLACE_SPARSE {
        match split_sparse(table, count) {
            Err(e) => return Err(e),
            Ok(v) => v,
        }
    } else {
        (&[], table)
    };
    let mut entries = Varints::new(entries_bytes);
    for i in 0..count {
        let a = i * chunk as u64;
        let len = if a + chunk as u64 <= stream {
            chunk
        } else {
            (stream - a) as usize
        };
        let mode = if op.r#type == TYPE_REPLACE_SPARSE {
            match mode_at(map, i) {
                None => {
                    return Err(err(
                        Code::block_map_invalid,
                        "block map does not cover the operation",
                    ));
                }
                Some(m) => m,
            }
        } else {
            Mode::Raw
        };
        let bytes: &[u8] = match mode {
            Mode::Keep => continue,
            Mode::Zero => {
                frame_buf[..len].fill(0);
                &frame_buf[..len]
            }
            Mode::Raw => {
                if op.r#type == TYPE_REPLACE_FRAMES {
                    let entry = match entries.next_entry() {
                        Err(e) => return Err(e),
                        Ok(v) => v,
                    };
                    if entry == 0 {
                        let raw = &data[pos..pos + len];
                        pos += len;
                        raw
                    } else {
                        let n = entry as usize;
                        match decoder.decode(&data[pos..pos + n], &mut frame_buf[..len]) {
                            Err(e) => return Err(e),
                            Ok(()) => {}
                        };
                        pos += n;
                        &frame_buf[..len]
                    }
                } else {
                    let raw = &data[pos..pos + len];
                    pos += len;
                    raw
                }
            }
            Mode::Zlib => {
                let n = match entries.next_entry() {
                    Err(e) => return Err(e),
                    Ok(v) => v as usize,
                };
                match decoder.decode(&data[pos..pos + n], &mut frame_buf[..len]) {
                    Err(e) => return Err(e),
                    Ok(()) => {}
                };
                pos += n;
                &frame_buf[..len]
            }
        };
        scatter(&dst, a, bytes, target);
    }
    return Ok(());
}

fn frame_len(chunk: usize, stream: u64) -> usize {
    return core::cmp::min(chunk as u64, stream) as usize;
}

fn frame_buffer(len: usize) -> Result<Vec<u8>, PayloadError> {
    let mut buf: Vec<u8> = Vec::new();
    match buf.try_reserve_exact(len) {
        Err(_) => {
            return Err(err(
                Code::resource_limit,
                "frame buffer cannot be allocated",
            ));
        }
        Ok(()) => {}
    };
    buf.resize(len, 0);
    return Ok(buf);
}

fn gather(spans: &[Span], image: &[u8]) -> Vec<u8> {
    let mut out: Vec<u8> = Vec::new();
    for s in spans {
        out.extend_from_slice(&image[s.start as usize..s.end() as usize]);
    }
    return out;
}

fn scatter(dst: &[Span], from: u64, bytes: &[u8], target: &mut [u8]) {
    let pieces = slice_stream(dst, from, from + bytes.len() as u64);
    let mut at = 0usize;
    for p in pieces {
        let n = p.len as usize;
        target[p.start as usize..p.start as usize + n].copy_from_slice(&bytes[at..at + n]);
        at += n;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gather_and_scatter_follow_stream_order() {
        let dst = vec![Span { start: 5, len: 2 }, Span { start: 0, len: 3 }];
        let mut target = vec![9u8; 8];
        scatter(&dst, 0, &[1, 2, 3, 4, 5], &mut target);
        assert_eq!(target, vec![3, 4, 5, 9, 9, 1, 2, 9]);
        assert_eq!(gather(&dst, &target), vec![1, 2, 3, 4, 5]);
    }

    #[test]
    fn a_frame_buffer_is_the_largest_chunk_the_operation_needs() {
        assert_eq!(frame_len(1 << 30, 100), 100);
        assert_eq!(frame_len(4096, 9096), 4096);
        assert_eq!(frame_len(4096, 904), 904);
        assert_eq!(frame_buffer(904).map(|b| b.len()).ok(), Some(904));
    }

    #[test]
    fn scatter_from_the_middle_of_a_stream_starts_at_that_position() {
        let dst = vec![Span { start: 0, len: 2 }, Span { start: 10, len: 3 }];
        let mut target = vec![0u8; 12];
        scatter(&dst, 1, &[7, 8], &mut target);
        assert_eq!(target[0], 0);
        assert_eq!(target[1], 7);
        assert_eq!(target[10], 8);
        assert_eq!(target[11], 0);
    }
}
