//! Metadata policy: the single, device-independent validation of a decoded
//! manifest.
//!
//! Every rule that can be decided from the payload alone lives here, and it
//! runs before the first byte is written. Replay then executes a [`Validated`]
//! plan, which can only be constructed by [`validate`], so replay never
//! discovers a metadata-only failure halfway through an update.
//!
//! Operations are byte ranges and execute in declared order. Destinations may
//! lie anywhere in the target, in any order and in any overlap with other
//! operations, because later writes win. The one rule on ranges is that a
//! copy's source may not intersect its own destination (`docs/format.md` §5).
//!
//! Device-dependent rules (the size of the actual storage, and that every range
//! lies inside it) belong to the client, which binds a `Validated` plan to a
//! device before any access. The generator runs this same pass as a self-check,
//! so it cannot emit a payload the client rejects.
//!
//! Failures here are *metadata errors*: the payload is refused with zero
//! writes. Corruption of blob bytes is a different category, detected per
//! operation during replay (see `docs/client.md`).

use crate::encoding::Mode;
use crate::encoding::check_packed_components;
use crate::encoding::chunk_count;
use crate::encoding::frame_table_blob_len;
use crate::encoding::mode_at;
use crate::encoding::sparse_blob_len;
use crate::encoding::split_sparse;
use crate::error::Code;
use crate::error::PayloadError;
use crate::error::err;
use crate::layout::PROJECT_MINOR;
use crate::manifest::Extent;
use crate::manifest::Manifest;
use crate::manifest::Operation;
use crate::manifest::PartInfo;
use crate::manifest::TYPE_REPLACE;
use crate::manifest::TYPE_REPLACE_FRAMES;
use crate::manifest::TYPE_REPLACE_SPARSE;
use crate::manifest::TYPE_SOURCECOPY;
use crate::manifest::TYPE_TARGET_COPY;
use crate::manifest::TYPE_ZERO;
use crate::profile::ResourceProfile;
use crate::sha256;

/// The payload's geometry, as the metadata states it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Geometry {
    pub minor_version: u32,
    /// The codec chunk size: the manifest's `block_size`. Storage is addressed in
    /// bytes, so this sets only how blobs are cut into frames.
    pub chunk_size: u64,
    pub operations: usize,
    /// New-image size in bytes, when recorded; the target may be larger.
    pub size: Option<u64>,
    /// One past the highest byte any operation addresses in the target: every
    /// destination byte, and the source bytes of an in-target copy. The device
    /// must hold at least this many bytes.
    pub end: u64,
    /// Total blob bytes, which equals `signatures_offset` when signed.
    pub blob_total: u64,
    /// Whether any operation copies from the old image, so the client must bind
    /// an image before its first write.
    pub needs_source: bool,
    /// Whether any operation copies within the target, so the target must be
    /// readable.
    pub reads_target: bool,
}

/// A manifest that has passed [`validate`]. Its only constructor is
/// [`validate`], so replay cannot be handed unchecked metadata.
pub struct Validated {
    manifest: Manifest,
    geometry: Geometry,
    contract: Contract,
}

impl Validated {
    pub fn manifest(&self) -> &Manifest {
        return &self.manifest;
    }

    pub fn geometry(&self) -> &Geometry {
        return &self.geometry;
    }

    /// What the destination must hold before the first write. Derived once, by
    /// [`validate`], and reused by every apply and reset.
    pub fn contract(&self) -> Contract {
        return self.contract;
    }
}

/// Validate a decoded manifest under `profile`. Consumes the manifest and
/// returns it inside a [`Validated`] on success.
pub fn validate(manifest: Manifest, profile: &ResourceProfile) -> Result<Validated, PayloadError> {
    let geometry = match check(&manifest, profile) {
        Err(e) => return Err(e),
        Ok(g) => g,
    };
    let contract = match derive_contract(&manifest, &geometry) {
        Err(e) => return Err(e),
        Ok(c) => c,
    };
    return Ok(Validated {
        manifest: manifest,
        geometry: geometry,
        contract: contract,
    });
}

/// What a validated program needs from the destination before its first write.
///
/// The contract is derived from the operations, never asserted by the payload, so a
/// program cannot claim independence while leaving bytes to the destination
/// (`docs/client.md` §11).
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Contract {
    /// Some new-image byte is left to the destination's current contents, or a copy
    /// reads the target. The destination must be the old image before the first write.
    BaseDependent,
    /// Every byte of the new-image prefix is written, and no byte of the target is
    /// read. The result is the same whatever the destination held before.
    DestinationIndependent,
}

fn derive_contract(m: &Manifest, g: &Geometry) -> Result<Contract, PayloadError> {
    let size = match g.size {
        None => return Ok(Contract::BaseDependent),
        Some(s) => s,
    };
    if g.reads_target {
        return Ok(Contract::BaseDependent);
    }
    let ops = &m.partitions[0].operations;
    let mut wanted: usize = 0;
    for op in ops {
        wanted = wanted.saturating_add(op.dst_extents.len());
    }
    let mut covered: alloc::vec::Vec<(u64, u64)> = alloc::vec::Vec::new();
    match covered.try_reserve_exact(wanted) {
        Err(_) => {
            return Err(err(
                Code::resource_limit,
                "contract workspace cannot be allocated",
            ));
        }
        Ok(()) => {}
    };
    for op in ops {
        let writes_all = match op.r#type {
            TYPE_SOURCECOPY | TYPE_REPLACE | TYPE_ZERO | TYPE_REPLACE_FRAMES => true,
            TYPE_REPLACE_SPARSE => sparse_keeps_no_chunk(op, g.chunk_size),
            _ => false,
        };
        if !writes_all {
            continue;
        }
        for ext in &op.dst_extents {
            let (start, n) = extent_of(ext);
            covered.push((start, start + n));
        }
    }
    covered.sort_unstable();
    let mut reached: u64 = 0;
    for (start, end) in covered {
        if start > reached {
            return Ok(Contract::BaseDependent);
        }
        reached = reached.max(end);
    }
    if reached < size {
        return Ok(Contract::BaseDependent);
    }
    return Ok(Contract::DestinationIndependent);
}

fn sparse_keeps_no_chunk(op: &Operation, chunk: u64) -> bool {
    let total = op.dst_extents.iter().fold(0u64, |a, e| a.saturating_add(extent_of(e).1));
    let count = match chunk_count(total, chunk) {
        None => return false,
        Some(n) => n,
    };
    let packed: &[u8] = match &op.frame_table {
        None => &[],
        Some(t) => &t[..],
    };
    let (map, _) = match split_sparse(packed, count) {
        Err(_) => return false,
        Ok(v) => v,
    };
    let mut i: u64 = 0;
    while i < count {
        match mode_at(map, i) {
            Some(Mode::Keep) | None => return false,
            Some(_) => {}
        };
        i += 1;
    }
    return true;
}

fn check(m: &Manifest, profile: &ResourceProfile) -> Result<Geometry, PayloadError> {
    let minor = m.minor_version.unwrap_or(0);
    if minor != PROJECT_MINOR {
        return Err(err(
            Code::unsupported_minor_version,
            "payload revision is not the byte-range revision",
        ));
    }
    if m.partitions.len() != 1 {
        return Err(err(
            Code::partition_mismatch,
            "this middleware applies exactly one partition",
        ));
    }
    let chunk = match m.block_size {
        None => 4096,
        Some(b) => b as u64,
    };
    if chunk == 0 || chunk > (profile.max_block_size as u64) {
        return Err(err(
            Code::unsupported_block_size,
            "block size outside the profile",
        ));
    }
    if let Some(sig_size) = m.signatures_size {
        if sig_size > (profile.max_signature_message as u64) {
            return Err(err(
                Code::resource_limit,
                "payload signature exceeds the profile",
            ));
        }
    }

    let pu = &m.partitions[0];
    let size = partition_size(&pu.new_partition_info);

    let walked = match check_operations(m, chunk, size, profile) {
        Err(e) => return Err(e),
        Ok(w) => w,
    };
    if let Some(total) = size {
        if walked.end > total {
            return Err(err(
                Code::extent_out_of_range,
                "an operation addresses bytes past the partition",
            ));
        }
    }

    if let (Some(off), Some(_)) = (m.signatures_offset, m.signatures_size) {
        if off != walked.blob_total {
            return Err(err(
                Code::overflow,
                "manifest signatures_offset disagrees with the operation blob total",
            ));
        }
    }

    return Ok(Geometry {
        minor_version: minor,
        chunk_size: chunk,
        operations: m.partitions[0].operations.len(),
        size: size,
        end: walked.end,
        blob_total: walked.blob_total,
        needs_source: walked.needs_source,
        reads_target: walked.reads_target,
    });
}

fn partition_size(info: &Option<PartInfo>) -> Option<u64> {
    return match info {
        None => None,
        Some(pi) => pi.size,
    };
}

struct Walked {
    end: u64,
    blob_total: u64,
    needs_source: bool,
    reads_target: bool,
}

fn check_operations(
    m: &Manifest,
    chunk: u64,
    size: Option<u64>,
    profile: &ResourceProfile,
) -> Result<Walked, PayloadError> {
    let ops = &m.partitions[0].operations;
    let mut cursor: u64 = 0;
    let mut walked = Walked {
        end: 0,
        blob_total: 0,
        needs_source: false,
        reads_target: false,
    };
    for op in ops {
        let kind = op.r#type;
        let known = kind == TYPE_REPLACE
            || kind == TYPE_ZERO
            || kind == TYPE_SOURCECOPY
            || kind == TYPE_REPLACE_FRAMES
            || kind == TYPE_REPLACE_SPARSE
            || kind == TYPE_TARGET_COPY;
        if !known {
            return Err(err(
                Code::unknown_operation,
                "operation type not supported by this middleware",
            ));
        }
        let is_copy = kind == TYPE_SOURCECOPY || kind == TYPE_TARGET_COPY;
        if !is_copy && (!op.src_extents.is_empty() || op.src_length.is_some()) {
            return Err(err(
                Code::unsupported_field,
                "source data is only valid on the copy operations",
            ));
        }
        let uses_table = kind == TYPE_REPLACE_FRAMES || kind == TYPE_REPLACE_SPARSE;
        if !uses_table && op.frame_table.is_some() {
            return Err(err(
                Code::unsupported_field,
                "frame table is only valid on REPLACE_FRAMES and REPLACE_SPARSE",
            ));
        }

        let op_offset = match op.data_offset {
            None => 0,
            Some(v) => v,
        };
        let op_len = match op.data_length {
            None => 0,
            Some(v) => v,
        };
        let op_end = match op_offset.checked_add(op_len) {
            None => {
                return Err(err(
                    Code::extent_out_of_range,
                    "operation blob range overflows",
                ));
            }
            Some(e) => e,
        };
        if op_offset != cursor {
            return Err(err(
                Code::extent_out_of_range,
                "operation blobs must be contiguous and ordered",
            ));
        }
        cursor = op_end;

        let (total, dst_end) = match check_extents(&op.dst_extents) {
            Err(e) => return Err(e),
            Ok(v) => v,
        };
        walked.end = walked.end.max(dst_end);
        if let Some(dst_length) = op.dst_length {
            if dst_length != total {
                return Err(err(
                    Code::blob_mismatch,
                    "dst_length does not match the destination extents",
                ));
            }
        }

        if kind == TYPE_REPLACE {
            match check_replace(op, total, op_len) {
                Err(e) => return Err(e),
                Ok(()) => {}
            };
        } else if kind == TYPE_ZERO {
            match check_zero(op, op_len) {
                Err(e) => return Err(e),
                Ok(()) => {}
            };
        } else if kind == TYPE_REPLACE_FRAMES {
            match check_frames(op, total, chunk, op_len, profile) {
                Err(e) => return Err(e),
                Ok(()) => {}
            };
        } else if kind == TYPE_REPLACE_SPARSE {
            match check_sparse(op, total, chunk, op_len, profile) {
                Err(e) => return Err(e),
                Ok(()) => {}
            };
        } else {
            let source_size = if kind == TYPE_SOURCECOPY {
                partition_size(&m.partitions[0].old_partition_info)
            } else {
                size
            };
            let src_end = match check_copy(op, total, op_len, source_size, kind == TYPE_TARGET_COPY)
            {
                Err(e) => return Err(e),
                Ok(v) => v,
            };
            if kind == TYPE_SOURCECOPY {
                walked.needs_source = true;
            } else {
                walked.reads_target = true;
                walked.end = walked.end.max(src_end);
            }
        }
    }
    walked.blob_total = cursor;
    return Ok(walked);
}

fn check_extents(extents: &[Extent]) -> Result<(u64, u64), PayloadError> {
    if extents.is_empty() {
        return Err(err(Code::extent_out_of_range, "operation has no extents"));
    }
    let mut total: u64 = 0;
    let mut furthest: u64 = 0;
    for ext in extents {
        if ext.start_byte.is_none() || ext.num_bytes.is_none() {
            return Err(err(
                Code::required_field_missing,
                "extent omits start_byte or num_bytes",
            ));
        }
        let (start, n) = extent_of(ext);
        if n == 0 {
            return Err(err(Code::extent_out_of_range, "zero-length extent"));
        }
        let end = match start.checked_add(n) {
            None => return Err(err(Code::extent_out_of_range, "extent overflows")),
            Some(e) => e,
        };
        total = match total.checked_add(n) {
            None => return Err(err(Code::extent_out_of_range, "extent total overflows")),
            Some(t) => t,
        };
        furthest = furthest.max(end);
    }
    return Ok((total, furthest));
}

pub(crate) fn extent_of(ext: &Extent) -> (u64, u64) {
    let start = match ext.start_byte {
        None => 0,
        Some(v) => v,
    };
    let n = match ext.num_bytes {
        None => 0,
        Some(v) => v,
    };
    return (start, n);
}

fn check_replace(op: &Operation, total: u64, op_len: u64) -> Result<(), PayloadError> {
    if op_len != total {
        return Err(err(
            Code::blob_mismatch,
            "REPLACE data size does not match extents",
        ));
    }
    return require_data_hash(op);
}

fn check_zero(op: &Operation, op_len: u64) -> Result<(), PayloadError> {
    if op_len != 0 {
        return Err(err(
            Code::blob_mismatch,
            "ZERO operation must have empty data",
        ));
    }
    if let Some(h) = &op.data_sha256_hash {
        if !h.is_empty() && !is_empty_blob_digest(h) {
            return Err(err(
                Code::blob_mismatch,
                "ZERO operation hash is not the empty-blob digest",
            ));
        }
    }
    return Ok(());
}

fn check_copy(
    op: &Operation,
    total: u64,
    op_len: u64,
    size: Option<u64>,
    in_target: bool,
) -> Result<u64, PayloadError> {
    let (src_total, src_end) = match check_extents(&op.src_extents) {
        Err(e) => return Err(e),
        Ok(v) => v,
    };
    if op_len != 0 {
        return Err(err(
            Code::blob_mismatch,
            "a copy carries no blob",
        ));
    }
    if let Some(h) = &op.data_sha256_hash {
        if !h.is_empty() {
            return Err(err(
                Code::unsupported_field,
                "a copy carries no data hash",
            ));
        }
    }
    if src_total != total {
        return Err(err(
            Code::blob_mismatch,
            "source and destination byte counts differ",
        ));
    }
    if let Some(len) = op.src_length {
        if len != total {
            return Err(err(
                Code::blob_mismatch,
                "src_length does not match the source extents",
            ));
        }
    }
    if !has_digest(&op.src_sha256_hash) {
        return Err(err(
            Code::required_field_missing,
            "a copy requires a source hash",
        ));
    }
    if let Some(total_bytes) = size {
        if src_end > total_bytes {
            return Err(err(
                Code::extent_out_of_range,
                "source extent is outside the partition",
            ));
        }
    }
    if in_target {
        let overlap = match extents_intersect(&op.src_extents, &op.dst_extents) {
            Err(e) => return Err(e),
            Ok(v) => v,
        };
        if overlap {
            return Err(err(
                Code::copy_overlap,
                "an in-target copy's source intersects its destination",
            ));
        }
    }
    return Ok(src_end);
}

fn extents_intersect(a: &[Extent], b: &[Extent]) -> Result<bool, PayloadError> {
    let mut spans: alloc::vec::Vec<(u64, u64)> = alloc::vec::Vec::new();
    match spans.try_reserve_exact(a.len() + b.len()) {
        Err(_) => {
            return Err(err(
                Code::resource_limit,
                "copy overlap workspace cannot be allocated",
            ));
        }
        Ok(()) => {}
    };
    for ext in a {
        let (start, n) = extent_of(ext);
        spans.push((start, start + n));
    }
    let split = spans.len();
    for ext in b {
        let (start, n) = extent_of(ext);
        spans.push((start, start + n));
    }
    let (left, right) = spans.split_at_mut(split);
    left.sort_unstable();
    right.sort_unstable();
    let mut i: usize = 0;
    let mut j: usize = 0;
    let mut left_reach: u64 = 0;
    let mut right_reach: u64 = 0;
    while i < left.len() || j < right.len() {
        let take_left = j == right.len() || (i < left.len() && left[i].0 <= right[j].0);
        if take_left {
            let (start, end) = left[i];
            if right_reach > start {
                return Ok(true);
            }
            left_reach = left_reach.max(end);
            i += 1;
        } else {
            let (start, end) = right[j];
            if left_reach > start {
                return Ok(true);
            }
            right_reach = right_reach.max(end);
            j += 1;
        }
    }
    return Ok(false);
}

fn check_frames(
    op: &Operation,
    total: u64,
    chunk: u64,
    op_len: u64,
    profile: &ResourceProfile,
) -> Result<(), PayloadError> {
    match require_data_hash(op) {
        Err(e) => return Err(e),
        Ok(()) => {}
    };
    let chunks = match chunk_count(total, chunk) {
        None => return Err(err(Code::overflow, "operation chunk count overflows")),
        Some(n) => n,
    };
    let table: &[u8] = match &op.frame_table {
        None => &[],
        Some(t) => &t[..],
    };
    match check_packed_components(false, chunks, table.len(), profile) {
        Err(e) => return Err(e),
        Ok(()) => {}
    };
    let blob = match frame_table_blob_len(table, total, chunk) {
        Err(e) => return Err(e),
        Ok(t) => t,
    };
    if blob != op_len {
        return Err(err(
            Code::blob_mismatch,
            "frame table does not describe the operation's blob",
        ));
    }
    return Ok(());
}

fn check_sparse(
    op: &Operation,
    total: u64,
    chunk: u64,
    op_len: u64,
    profile: &ResourceProfile,
) -> Result<(), PayloadError> {
    match require_data_hash(op) {
        Err(e) => return Err(e),
        Ok(()) => {}
    };
    let chunks = match chunk_count(total, chunk) {
        None => return Err(err(Code::overflow, "operation chunk count overflows")),
        Some(n) => n,
    };
    let packed: &[u8] = match &op.frame_table {
        None => &[],
        Some(t) => &t[..],
    };
    match check_packed_components(true, chunks, packed.len(), profile) {
        Err(e) => return Err(e),
        Ok(()) => {}
    };
    let blob = match sparse_blob_len(packed, total, chunk) {
        Err(e) => return Err(e),
        Ok(t) => t,
    };
    if blob != op_len {
        return Err(err(
            Code::blob_mismatch,
            "block map and frame table do not describe the operation's blob",
        ));
    }
    if blob == 0 {
        let digest: &[u8] = match &op.data_sha256_hash {
            None => &[],
            Some(h) => &h[..],
        };
        if !is_empty_blob_digest(digest) {
            return Err(err(
                Code::blob_mismatch,
                "sparse operation with an empty blob does not carry the empty-blob digest",
            ));
        }
    }
    return Ok(());
}

fn is_empty_blob_digest(h: &[u8]) -> bool {
    return h == &sha256::hash_bytes(&[])[..];
}

fn has_digest(h: &Option<alloc::vec::Vec<u8>>) -> bool {
    return match h {
        None => false,
        Some(v) => !v.is_empty(),
    };
}

fn require_data_hash(op: &Operation) -> Result<(), PayloadError> {
    if !has_digest(&op.data_sha256_hash) {
        return Err(err(
            Code::required_field_missing,
            "operations with data require a data hash",
        ));
    }
    return Ok(());
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::PartitionUpdate;
    use crate::profile::EMBEDDED;
    use crate::encoding::push_varint;
    use alloc::string::ToString;
    use alloc::vec;
    use alloc::vec::Vec;

    const BS: u64 = 4096;
    const REV: u32 = PROJECT_MINOR;

    fn ext(start: u64, n: u64) -> Extent {
        return Extent {
            start_byte: Some(start),
            num_bytes: Some(n),
        };
    }

    fn hash_of(data: &[u8]) -> Vec<u8> {
        return sha256::hash_bytes(data).to_vec();
    }

    fn replace(off: u64, len: u64, dst: Vec<Extent>) -> Operation {
        return Operation {
            r#type: TYPE_REPLACE,
            data_offset: Some(off),
            data_length: Some(len),
            dst_extents: dst,
            data_sha256_hash: Some(hash_of(b"blob")),
            ..Default::default()
        };
    }

    fn zero(off: u64, dst: Vec<Extent>) -> Operation {
        return Operation {
            r#type: TYPE_ZERO,
            data_offset: Some(off),
            data_length: Some(0),
            dst_extents: dst,
            ..Default::default()
        };
    }

    fn manifest(ops: Vec<Operation>) -> Manifest {
        let part = PartitionUpdate {
            partition_name: "A/B".to_string(),
            operations: ops,
            ..Default::default()
        };
        return Manifest {
            block_size: Some(BS as u32),
            minor_version: Some(REV),
            partitions: vec![part],
            ..Default::default()
        };
    }

    fn code_of(m: Manifest) -> Code {
        match validate(m, &EMBEDDED) {
            Ok(_) => panic!("expected a metadata error"),
            Err(e) => return e.code,
        }
    }

    fn table_of(values: &[u64]) -> Vec<u8> {
        let mut table = Vec::new();
        for v in values {
            push_varint(&mut table, *v);
        }
        return table;
    }

    fn frames_op(table: Vec<u8>, data_len: u64, dst: Vec<Extent>) -> Operation {
        return Operation {
            r#type: TYPE_REPLACE_FRAMES,
            data_offset: Some(0),
            data_length: Some(data_len),
            dst_extents: dst,
            frame_table: Some(table),
            data_sha256_hash: Some(hash_of(b"blob")),
            ..Default::default()
        };
    }

    fn sparse_op(packed: Vec<u8>, data_len: u64, dst: Vec<Extent>) -> Operation {
        return Operation {
            r#type: TYPE_REPLACE_SPARSE,
            data_offset: Some(0),
            data_length: Some(data_len),
            dst_extents: dst,
            frame_table: Some(packed),
            data_sha256_hash: Some(hash_of(b"blob")),
            ..Default::default()
        };
    }

    fn map_of(modes: &[u8]) -> Vec<u8> {
        let mut map = vec![0u8; modes.len().div_ceil(4)];
        for (i, m) in modes.iter().enumerate() {
            map[i / 4] |= m << (2 * (i % 4));
        }
        return map;
    }

    fn copy_op(kind: i32, src: Vec<Extent>, dst: Vec<Extent>) -> Operation {
        return Operation {
            r#type: kind,
            data_offset: Some(0),
            data_length: Some(0),
            src_extents: src,
            dst_extents: dst,
            src_sha256_hash: Some(hash_of(b"source")),
            ..Default::default()
        };
    }

    #[test]
    fn accepts_byte_ranges_out_of_order_and_overlapping_and_reports_the_geometry() {
        let m = manifest(vec![
            replace(0, 2 * BS + 100, vec![ext(3 * BS + 7, 100), ext(BS, 2 * BS)]),
            zero(2 * BS + 100, vec![ext(BS + 5, 10)]),
        ]);
        let v = validate(m, &EMBEDDED).unwrap();
        assert_eq!(v.geometry().end, 3 * BS + 107);
        assert_eq!(v.geometry().blob_total, 2 * BS + 100);
        assert_eq!(v.geometry().operations, 2);
        assert_eq!(v.geometry().chunk_size, BS);
        assert!(!v.geometry().needs_source);
        assert!(!v.geometry().reads_target);
    }

    #[test]
    fn only_known_operation_types_are_accepted() {
        let mut op = replace(0, BS, vec![ext(0, BS)]);
        op.r#type = 8;
        assert_eq!(code_of(manifest(vec![op])), Code::unknown_operation);
        let mut op = replace(0, BS, vec![ext(0, BS)]);
        op.r#type = 12;
        assert_eq!(code_of(manifest(vec![op])), Code::unknown_operation);
    }

    #[test]
    fn replace_requires_exact_length_and_a_data_hash() {
        let short = replace(0, BS - 1, vec![ext(0, BS)]);
        assert_eq!(code_of(manifest(vec![short])), Code::blob_mismatch);
        let mut bare = replace(0, BS, vec![ext(0, BS)]);
        bare.data_sha256_hash = None;
        assert_eq!(
            code_of(manifest(vec![bare])),
            Code::required_field_missing
        );
    }

    #[test]
    fn zero_requires_empty_data() {
        let mut z = zero(0, vec![ext(0, BS)]);
        z.data_length = Some(BS);
        assert_eq!(code_of(manifest(vec![z])), Code::blob_mismatch);
    }

    #[test]
    fn extent_arithmetic_overflow_is_an_error_not_a_panic() {
        let wrap = replace(0, BS, vec![ext(u64::MAX - 1, 2)]);
        assert_eq!(code_of(manifest(vec![wrap])), Code::extent_out_of_range);
        let total = replace(0, BS, vec![ext(0, u64::MAX), ext(0, 1)]);
        assert_eq!(code_of(manifest(vec![total])), Code::extent_out_of_range);
    }

    #[test]
    fn zero_length_and_empty_extent_lists_are_refused() {
        let empty = replace(0, 0, vec![]);
        assert_eq!(code_of(manifest(vec![empty])), Code::extent_out_of_range);
        let zero_len = replace(0, 0, vec![ext(0, 0)]);
        assert_eq!(code_of(manifest(vec![zero_len])), Code::extent_out_of_range);
    }

    #[test]
    fn every_range_bounds_the_partition_not_only_the_last_one() {
        let mut m = manifest(vec![replace(0, 2 * BS, vec![ext(10 * BS, BS), ext(0, BS)])]);
        m.partitions[0].old_partition_info = Some(PartInfo {
            size: Some(4 * BS),
            hash: None,
        });
        m.partitions[0].new_partition_info = Some(PartInfo {
            size: Some(4 * BS),
            hash: None,
        });
        assert_eq!(code_of(m), Code::extent_out_of_range);
    }

    #[test]
    fn blobs_must_be_contiguous_and_match_the_signature_offset() {
        let gap = vec![replace(BS, BS, vec![ext(0, BS)])];
        assert_eq!(code_of(manifest(gap)), Code::extent_out_of_range);
        let mut m = manifest(vec![replace(0, BS, vec![ext(0, BS)])]);
        m.signatures_offset = Some(BS + 1);
        m.signatures_size = Some(8);
        assert_eq!(code_of(m), Code::overflow);
    }

    #[test]
    fn source_data_is_refused_on_replace_and_zero() {
        let mut op = replace(0, BS, vec![ext(0, BS)]);
        op.src_extents = vec![ext(0, BS)];
        assert_eq!(code_of(manifest(vec![op])), Code::unsupported_field);
    }

    #[test]
    fn partition_sizes_are_independent_and_need_not_be_block_aligned() {
        let mut m = manifest(vec![replace(0, BS, vec![ext(0, BS)])]);
        m.partitions[0].old_partition_info = Some(PartInfo {
            size: Some(4 * BS + 1),
            hash: None,
        });
        m.partitions[0].new_partition_info = Some(PartInfo {
            size: Some(4 * BS + 1),
            hash: None,
        });
        assert!(validate(m, &EMBEDDED).is_ok());

        let mut m = manifest(vec![replace(0, BS, vec![ext(0, BS)])]);
        m.partitions[0].old_partition_info = Some(PartInfo {
            size: Some(4 * BS),
            hash: None,
        });
        m.partitions[0].new_partition_info = Some(PartInfo {
            size: Some(8 * BS),
            hash: None,
        });
        assert_eq!(
            validate(m, &EMBEDDED).unwrap().geometry().size,
            Some(8 * BS)
        );
    }

    #[test]
    fn copies_use_their_own_source_geometry_and_destinations_use_only_new_size() {
        for (old, new) in [(8 * BS, 2 * BS), (2 * BS, 8 * BS)] {
            let mut m = manifest(vec![copy_op(
                TYPE_SOURCECOPY,
                vec![ext(old - BS, BS)],
                vec![ext(new - BS, BS)],
            )]);
            m.partitions[0].old_partition_info = Some(PartInfo {
                size: Some(old),
                hash: None,
            });
            m.partitions[0].new_partition_info = Some(PartInfo {
                size: Some(new),
                hash: None,
            });
            let g = *validate(m.clone(), &EMBEDDED).unwrap().geometry();
            assert_eq!(g.size, Some(new));
            assert_eq!(g.end, new);
            m.partitions[0].operations[0].src_extents = vec![ext(old, BS)];
            assert_eq!(code_of(m.clone()), Code::extent_out_of_range);
            m.partitions[0].operations[0].src_extents = vec![ext(0, BS)];
            m.partitions[0].operations[0].dst_extents = vec![ext(new, BS)];
            assert_eq!(code_of(m), Code::extent_out_of_range);
        }
        let mut m = manifest(vec![copy_op(
            TYPE_SOURCECOPY,
            vec![ext(3 * BS, BS)],
            vec![ext(0, BS)],
        )]);
        m.partitions[0].old_partition_info = Some(PartInfo {
            size: Some(8 * BS),
            hash: None,
        });
        m.partitions[0].new_partition_info = Some(PartInfo {
            size: Some(2 * BS),
            hash: None,
        });
        m.partitions[0].operations[0].r#type = TYPE_TARGET_COPY;
        assert_eq!(code_of(m), Code::extent_out_of_range);
    }

    #[test]
    fn an_old_size_alone_does_not_bound_the_target() {
        let mut m = manifest(vec![zero(0, vec![ext(7 * BS, BS)])]);
        m.partitions[0].old_partition_info = Some(PartInfo {
            size: Some(BS),
            hash: None,
        });
        m.partitions[0].new_partition_info = None;
        let g = *validate(m, &EMBEDDED).unwrap().geometry();
        assert_eq!(g.size, None);
        assert_eq!(g.end, 8 * BS);
    }

    #[test]
    fn revision_must_be_the_byte_range_revision() {
        for minor in [4, 16, 18] {
            let mut m = manifest(vec![zero(0, vec![ext(0, BS)])]);
            m.minor_version = Some(minor);
            assert_eq!(code_of(m), Code::unsupported_minor_version);
        }
        let mut m = manifest(vec![replace(0, BS, vec![ext(0, BS)])]);
        m.minor_version = None;
        assert_eq!(code_of(m), Code::unsupported_minor_version);
    }

    #[test]
    fn block_size_is_the_codec_chunk_and_is_bounded() {
        let mut m = manifest(vec![replace(0, BS, vec![ext(0, BS)])]);
        m.block_size = Some(0);
        assert_eq!(code_of(m), Code::unsupported_block_size);
        let mut m = manifest(vec![replace(0, 65536, vec![ext(0, 65536)])]);
        m.block_size = Some(65536);
        assert_eq!(code_of(m), Code::unsupported_block_size);
    }

    #[test]
    fn zero_hash_is_absent_empty_or_the_empty_blob_digest() {
        let empty = sha256::hash_bytes(&[]);
        for hash in [None, Some(Vec::new()), Some(empty.to_vec())] {
            let mut z = zero(0, vec![ext(0, BS)]);
            z.data_sha256_hash = hash;
            assert!(validate(manifest(vec![z]), &EMBEDDED).is_ok());
        }
        let mut z = zero(0, vec![ext(0, BS)]);
        z.data_sha256_hash = Some(vec![0; 32]);
        assert_eq!(code_of(manifest(vec![z])), Code::blob_mismatch);
        let mut z = zero(0, vec![ext(0, BS)]);
        z.data_sha256_hash = Some(sha256::hash_bytes(b"x").to_vec());
        assert_eq!(code_of(manifest(vec![z])), Code::blob_mismatch);
    }

    #[test]
    fn frames_accept_a_table_over_full_and_partial_chunks() {
        let total = 2 * BS + BS / 2;
        let table = table_of(&[0, 100, 0]);
        let blob = BS + 100 + BS / 2;
        let m = manifest(vec![frames_op(table, blob, vec![ext(0, total)])]);
        let v = validate(m, &EMBEDDED).unwrap();
        assert_eq!(v.geometry().blob_total, blob);
        assert_eq!(v.geometry().end, total);
    }

    #[test]
    fn frames_need_a_data_hash() {
        let mut op = frames_op(table_of(&[0]), BS, vec![ext(0, BS)]);
        op.data_sha256_hash = None;
        assert_eq!(code_of(manifest(vec![op])), Code::required_field_missing);
    }

    #[test]
    fn frame_table_must_describe_exactly_the_blob_and_each_chunk() {
        let short = frames_op(table_of(&[0]), 2 * BS, vec![ext(0, 2 * BS)]);
        assert_eq!(code_of(manifest(vec![short])), Code::frame_table_invalid);
        let big = frames_op(table_of(&[BS]), BS, vec![ext(0, BS)]);
        assert_eq!(code_of(manifest(vec![big])), Code::frame_table_invalid);
        let partial = frames_op(table_of(&[0, 100]), BS + 100, vec![ext(0, BS + 100)]);
        assert_eq!(code_of(manifest(vec![partial])), Code::frame_table_invalid);
        let wrong = frames_op(table_of(&[0]), BS - 1, vec![ext(0, BS)]);
        assert_eq!(code_of(manifest(vec![wrong])), Code::blob_mismatch);
    }

    #[test]
    fn a_frame_table_is_refused_on_operations_that_do_not_use_one() {
        let mut op = replace(0, BS, vec![ext(0, BS)]);
        op.frame_table = Some(table_of(&[0]));
        assert_eq!(code_of(manifest(vec![op])), Code::unsupported_field);
    }

    #[test]
    fn sparse_accepts_a_map_over_partial_and_full_chunks() {
        let total = 4 * BS + 300;
        let map = map_of(&[0, 1, 2, 3, 3]);
        let mut packed = map;
        push_varint(&mut packed, 100);
        push_varint(&mut packed, 50);
        let blob = BS + 100 + 50;
        let m = manifest(vec![sparse_op(packed, blob, vec![ext(0, total)])]);
        assert!(validate(m, &EMBEDDED).is_ok());
    }

    #[test]
    fn sparse_map_must_have_the_canonical_length_and_clear_unused_bits() {
        let packed = map_of(&[0, 0, 0, 0]);
        let m = manifest(vec![sparse_op(packed, 0, vec![ext(0, 5 * BS)])]);
        assert_eq!(code_of(m), Code::block_map_invalid);
        let empty = sha256::hash_bytes(&[]).to_vec();
        let mut op = sparse_op(map_of(&[0, 0, 0]), 0, vec![ext(0, 3 * BS)]);
        op.data_sha256_hash = Some(empty.clone());
        assert!(validate(manifest(vec![op]), &EMBEDDED).is_ok());
        let mut packed = map_of(&[0, 0, 0]);
        packed[0] |= 0x40;
        let mut op = sparse_op(packed, 0, vec![ext(0, 3 * BS)]);
        op.data_sha256_hash = Some(empty);
        assert_eq!(code_of(manifest(vec![op])), Code::block_map_invalid);
    }

    #[test]
    fn sparse_table_must_have_one_entry_per_zlib_chunk_and_no_more() {
        let mut short = map_of(&[3, 3]);
        push_varint(&mut short, 10);
        let op = sparse_op(short, 10, vec![ext(0, 2 * BS)]);
        assert_eq!(code_of(manifest(vec![op])), Code::frame_table_invalid);
        let mut long = map_of(&[3]);
        push_varint(&mut long, 10);
        push_varint(&mut long, 10);
        let op = sparse_op(long, 10, vec![ext(0, BS)]);
        assert_eq!(code_of(manifest(vec![op])), Code::frame_table_invalid);
    }

    #[test]
    fn sparse_with_an_empty_blob_needs_the_empty_digest() {
        let packed = map_of(&[0, 1]);
        let mut op = sparse_op(packed.clone(), 0, vec![ext(0, 2 * BS)]);
        op.data_sha256_hash = Some(sha256::hash_bytes(&[]).to_vec());
        assert!(validate(manifest(vec![op]), &EMBEDDED).is_ok());
        let op = sparse_op(packed, 0, vec![ext(0, 2 * BS)]);
        assert_eq!(code_of(manifest(vec![op])), Code::blob_mismatch);
    }

    #[test]
    fn source_copy_accepts_a_source_and_marks_the_plan_as_needing_one() {
        let op = copy_op(TYPE_SOURCECOPY, vec![ext(BS, BS)], vec![ext(0, BS)]);
        let v = validate(manifest(vec![op]), &EMBEDDED).unwrap();
        assert!(v.geometry().needs_source);
        assert!(!v.geometry().reads_target);
    }

    #[test]
    fn copies_need_equal_byte_counts_a_source_hash_and_no_blob() {
        let short = copy_op(TYPE_SOURCECOPY, vec![ext(0, BS)], vec![ext(0, BS + 1)]);
        assert_eq!(code_of(manifest(vec![short])), Code::blob_mismatch);
        let mut bare = copy_op(TYPE_SOURCECOPY, vec![ext(0, BS)], vec![ext(BS, BS)]);
        bare.src_sha256_hash = None;
        assert_eq!(code_of(manifest(vec![bare])), Code::required_field_missing);
        let mut blob = copy_op(TYPE_SOURCECOPY, vec![ext(0, BS)], vec![ext(BS, BS)]);
        blob.data_length = Some(BS);
        assert_eq!(code_of(manifest(vec![blob])), Code::blob_mismatch);
    }

    #[test]
    fn copy_source_must_lie_inside_the_recorded_partition() {
        let mut m = manifest(vec![copy_op(TYPE_SOURCECOPY, vec![ext(3 * BS, BS)], vec![ext(0, BS)])]);
        m.partitions[0].old_partition_info = Some(PartInfo {
            size: Some(4 * BS),
            hash: None,
        });
        m.partitions[0].new_partition_info = Some(PartInfo {
            size: Some(4 * BS),
            hash: None,
        });
        assert!(validate(m.clone(), &EMBEDDED).is_ok());
        m.partitions[0].operations[0].src_extents = vec![ext(3 * BS + 1, BS)];
        assert_eq!(code_of(m), Code::extent_out_of_range);
    }

    #[test]
    fn in_target_copy_is_accepted_and_needs_a_readable_target() {
        let op = copy_op(
            TYPE_TARGET_COPY,
            vec![ext(BS, BS), ext(4 * BS, 100)],
            vec![ext(0, BS), ext(3 * BS, 100)],
        );
        let v = validate(manifest(vec![op]), &EMBEDDED).unwrap();
        assert!(v.geometry().reads_target);
        assert!(!v.geometry().needs_source);
        assert_eq!(v.geometry().end, 4 * BS + 100);
    }

    #[test]
    fn in_target_copy_refuses_a_source_that_intersects_its_destination() {
        let shifted = copy_op(TYPE_TARGET_COPY, vec![ext(0, BS)], vec![ext(1, BS)]);
        assert_eq!(code_of(manifest(vec![shifted])), Code::copy_overlap);
        let cycle = copy_op(
            TYPE_TARGET_COPY,
            vec![ext(0, BS), ext(BS, BS)],
            vec![ext(BS, BS), ext(0, BS)],
        );
        assert_eq!(code_of(manifest(vec![cycle])), Code::copy_overlap);
    }

    #[test]
    fn in_target_copy_checks_the_whole_range_sets() {
        let op = copy_op(
            TYPE_TARGET_COPY,
            vec![ext(0, BS), ext(5 * BS, BS)],
            vec![ext(2 * BS, BS), ext(5 * BS + 10, BS)],
        );
        assert_eq!(code_of(manifest(vec![op])), Code::copy_overlap);
    }

    fn meets(a: &[Extent], b: &[Extent]) -> bool {
        return extents_intersect(a, b).ok().unwrap();
    }

    fn pairwise(a: &[Extent], b: &[Extent]) -> bool {
        for x in a {
            let (xs, xn) = extent_of(x);
            for y in b {
                let (ys, yn) = extent_of(y);
                if xs < ys + yn && ys < xs + xn {
                    return true;
                }
            }
        }
        return false;
    }

    #[test]
    fn disjoint_and_adjacent_ranges_do_not_meet() {
        assert!(!meets(&[ext(0, BS)], &[ext(2 * BS, BS)]));
        assert!(!meets(&[ext(0, BS)], &[ext(BS, BS)]));
        assert!(!meets(&[ext(BS, BS)], &[ext(0, BS)]));
    }

    #[test]
    fn one_shared_byte_meets() {
        assert!(meets(&[ext(0, BS + 1)], &[ext(BS, BS)]));
        assert!(meets(&[ext(BS, BS)], &[ext(0, BS + 1)]));
    }

    #[test]
    fn a_range_that_contains_another_meets_it() {
        assert!(meets(&[ext(0, 4 * BS)], &[ext(2 * BS, BS)]));
    }

    #[test]
    fn unordered_ranges_are_sorted_before_they_are_compared() {
        let a = [ext(5 * BS, BS), ext(0, BS)];
        let b = [ext(3 * BS, BS), ext(BS, BS)];
        assert!(!meets(&a, &b));
        let b_hit = [ext(3 * BS, BS), ext(BS, BS), ext(6 * BS - 1, 1)];
        assert!(meets(&a, &b_hit));
    }

    #[test]
    fn overlap_within_one_list_is_allowed_and_does_not_hide_a_meeting() {
        let a = [ext(0, 2 * BS), ext(BS, 2 * BS)];
        let b = [ext(3 * BS, BS), ext(3 * BS + BS / 2, BS)];
        assert!(!meets(&a, &b));
        assert!(meets(&a, &[ext(3 * BS, BS), ext(2 * BS + 1, 1)]));
    }

    #[test]
    fn the_sweep_agrees_with_the_pairwise_definition_on_generated_lists() {
        let mut state: u64 = 0x9E37_79B9_7F4A_7C15;
        let mut draw = |bound: u64| -> u64 {
            state = state
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            return (state >> 33) % bound;
        };
        for _ in 0..2_000 {
            let mut lists: [Vec<Extent>; 2] = [Vec::new(), Vec::new()];
            for list in lists.iter_mut() {
                let count = 1 + draw(6);
                for _ in 0..count {
                    let start = draw(64);
                    let len = 1 + draw(12);
                    list.push(ext(start, len));
                }
            }
            let (a, b) = (&lists[0], &lists[1]);
            let want = pairwise(a, b);
            assert_eq!(meets(a, b), want, "a = {a:?}, b = {b:?}");
            assert_eq!(meets(b, a), want, "a = {a:?}, b = {b:?}");
        }
    }

    #[test]
    fn the_extent_heavy_copy_is_checked_across_the_whole_lists() {
        let n = (EMBEDDED.max_total_extents / 2) as u64;
        let src: Vec<Extent> = (0..n).rev().map(|k| ext(2 * k * BS, BS)).collect();
        let dst: Vec<Extent> = (0..n).rev().map(|k| ext((2 * k + 1) * BS, BS)).collect();
        let clear = copy_op(TYPE_TARGET_COPY, src.clone(), dst.clone());
        assert!(validate(manifest(vec![clear]), &EMBEDDED).is_ok());
        let mut hit = src;
        hit[0] = ext(BS, BS);
        let op = copy_op(TYPE_TARGET_COPY, hit, dst);
        assert_eq!(code_of(manifest(vec![op])), Code::copy_overlap);
    }

    #[test]
    fn an_extent_must_state_both_fields_in_either_list() {
        let no_start = Extent {
            start_byte: None,
            num_bytes: Some(BS),
        };
        let no_length = Extent {
            start_byte: Some(0),
            num_bytes: None,
        };
        let dst_no_start = replace(0, BS, vec![no_start.clone()]);
        assert_eq!(code_of(manifest(vec![dst_no_start])), Code::required_field_missing);
        let dst_no_length = replace(0, BS, vec![no_length.clone()]);
        assert_eq!(code_of(manifest(vec![dst_no_length])), Code::required_field_missing);
        let src_no_start = copy_op(TYPE_TARGET_COPY, vec![no_start], vec![ext(2 * BS, BS)]);
        assert_eq!(code_of(manifest(vec![src_no_start])), Code::required_field_missing);
        let dst_no_len = copy_op(TYPE_TARGET_COPY, vec![ext(0, BS)], vec![no_length]);
        assert_eq!(code_of(manifest(vec![dst_no_len])), Code::required_field_missing);
    }

    #[test]
    fn an_explicit_zero_start_is_a_present_start() {
        let zero = Extent {
            start_byte: Some(0),
            num_bytes: Some(BS),
        };
        let v = validate(manifest(vec![replace(0, BS, vec![zero])]), &EMBEDDED).unwrap();
        assert_eq!(v.geometry().end, BS);
    }

    #[test]
    fn in_target_copy_source_counts_toward_the_device_bound() {
        let mut m = manifest(vec![copy_op(
            TYPE_TARGET_COPY,
            vec![ext(9 * BS, BS)],
            vec![ext(0, BS)],
        )]);
        m.partitions[0].new_partition_info = Some(PartInfo {
            size: Some(4 * BS),
            hash: None,
        });
        assert_eq!(code_of(m), Code::extent_out_of_range);
    }

    #[test]
    fn later_operations_may_rewrite_an_earlier_source() {
        let copy = copy_op(TYPE_TARGET_COPY, vec![ext(BS, BS)], vec![ext(0, BS)]);
        let mut rewrite = replace(0, BS, vec![ext(BS, BS)]);
        rewrite.data_offset = Some(0);
        assert!(validate(manifest(vec![copy, rewrite]), &EMBEDDED).is_ok());
    }

    #[test]
    fn operations_at_their_packed_limits_pass_and_one_byte_more_is_refused() {
        let chunks = EMBEDDED.max_frame_table_bytes as u64;
        let table = vec![0u8; chunks as usize];
        let blob = chunks * BS;
        let at = frames_op(table, blob, vec![ext(0, chunks * BS)]);
        assert!(validate(manifest(vec![at]), &EMBEDDED).is_ok());
        let over = vec![0u8; chunks as usize + 1];
        let over_op = frames_op(over, blob + BS, vec![ext(0, (chunks + 1) * BS)]);
        assert_eq!(code_of(manifest(vec![over_op])), Code::resource_limit);

        let map_chunks = (EMBEDDED.max_map_bytes as u64 + 1) * 4;
        let map = vec![0u8; EMBEDDED.max_map_bytes + 1];
        let op = sparse_op(map, 0, vec![ext(0, map_chunks * BS)]);
        assert_eq!(code_of(manifest(vec![op])), Code::resource_limit);
    }

    fn sized(ops: Vec<Operation>, size: u64) -> Manifest {
        let mut m = manifest(ops);
        m.partitions[0].new_partition_info = Some(PartInfo {
            size: Some(size),
            hash: Some(hash_of(b"new")),
        });
        return m;
    }

    fn contract_of(m: Manifest) -> Contract {
        return validate(m, &EMBEDDED).ok().unwrap().contract();
    }

    #[test]
    fn replace_and_zero_covering_the_prefix_are_destination_independent() {
        let ops = vec![
            replace(0, BS, vec![ext(0, BS)]),
            zero(BS, vec![ext(BS, BS)]),
        ];
        assert_eq!(contract_of(sized(ops, 2 * BS)), Contract::DestinationIndependent);
    }

    #[test]
    fn a_gap_in_the_prefix_is_base_dependent() {
        let ops = vec![replace(0, BS, vec![ext(0, BS)])];
        assert_eq!(contract_of(sized(ops, 2 * BS)), Contract::BaseDependent);
    }

    #[test]
    fn coverage_may_arrive_out_of_order_and_overlap() {
        let ops = vec![
            replace(0, BS, vec![ext(BS, BS)]),
            replace(BS, 2 * BS, vec![ext(0, BS), ext(2 * BS, BS)]),
        ];
        assert_eq!(contract_of(sized(ops, 3 * BS)), Contract::DestinationIndependent);
    }

    #[test]
    fn a_partial_last_chunk_must_still_be_covered() {
        let ops = vec![replace(0, BS, vec![ext(0, BS)])];
        assert_eq!(contract_of(sized(ops, BS + 1)), Contract::BaseDependent);
    }

    #[test]
    fn an_in_target_copy_is_base_dependent_even_over_full_coverage() {
        let ops = vec![
            copy_op(TYPE_TARGET_COPY, vec![ext(0, BS)], vec![ext(BS, BS)]),
            replace(0, BS, vec![ext(0, BS)]),
        ];
        assert_eq!(contract_of(sized(ops, 2 * BS)), Contract::BaseDependent);
    }

    #[test]
    fn a_source_copy_covering_the_prefix_is_destination_independent() {
        let ops = vec![copy_op(TYPE_SOURCECOPY, vec![ext(0, 2 * BS)], vec![ext(0, 2 * BS)])];
        assert_eq!(contract_of(sized(ops, 2 * BS)), Contract::DestinationIndependent);
    }

    #[test]
    fn a_sparse_operation_that_keeps_a_chunk_leaves_it_to_the_destination() {
        let mut op = sparse_op(map_of(&[0, 1]), 0, vec![ext(0, 2 * BS)]);
        op.data_sha256_hash = Some(hash_of(&[]));
        let ops = vec![op];
        assert_eq!(contract_of(sized(ops, 2 * BS)), Contract::BaseDependent);
    }

    #[test]
    fn a_sparse_operation_with_no_kept_chunk_covers_its_prefix() {
        let mut op = sparse_op(map_of(&[1, 1]), 0, vec![ext(0, 2 * BS)]);
        op.data_sha256_hash = Some(hash_of(&[]));
        let ops = vec![op];
        assert_eq!(contract_of(sized(ops, 2 * BS)), Contract::DestinationIndependent);
    }

    #[test]
    fn a_payload_without_a_signed_size_is_base_dependent() {
        let ops = vec![replace(0, BS, vec![ext(0, BS)])];
        assert_eq!(contract_of(manifest(ops)), Contract::BaseDependent);
    }

    #[test]
    fn an_empty_program_covers_only_an_empty_image() {
        assert_eq!(contract_of(sized(vec![], 2 * BS)), Contract::BaseDependent);
        assert_eq!(contract_of(sized(vec![], 0)), Contract::DestinationIndependent);
    }

    #[test]
    fn adjacent_extents_across_and_within_operations_leave_no_gap() {
        let ops = vec![
            replace(0, BS, vec![ext(0, BS)]),
            replace(BS, BS, vec![ext(BS, BS)]),
            replace(2 * BS, 2 * BS, vec![ext(2 * BS, BS), ext(3 * BS, BS)]),
        ];
        assert_eq!(contract_of(sized(ops, 4 * BS)), Contract::DestinationIndependent);
    }

    #[test]
    fn a_one_byte_gap_between_extents_is_base_dependent() {
        let ops = vec![replace(0, 2 * BS - 1, vec![ext(0, BS), ext(BS + 1, BS - 1)])];
        assert_eq!(contract_of(sized(ops, 2 * BS)), Contract::BaseDependent);
    }

    #[test]
    fn overlapping_extents_within_one_operation_cover_their_union() {
        let ops = vec![replace(0, 3 * BS, vec![ext(0, 2 * BS), ext(BS, BS)])];
        assert_eq!(contract_of(sized(ops, 2 * BS)), Contract::DestinationIndependent);
    }

    #[test]
    fn an_extent_heavy_program_is_classified_by_the_union_of_all_its_extents() {
        let blocks = EMBEDDED.max_total_extents as u64;
        let full: Vec<Extent> = (0..blocks).rev().map(|k| ext(k * BS, BS)).collect();
        let op = replace(0, blocks * BS, full.clone());
        assert_eq!(
            contract_of(sized(vec![op], blocks * BS)),
            Contract::DestinationIndependent
        );
        let holed: Vec<Extent> = full.into_iter().filter(|e| e.start_byte != Some(77 * BS)).collect();
        let n = holed.len() as u64;
        let op = replace(0, n * BS, holed);
        assert_eq!(contract_of(sized(vec![op], blocks * BS)), Contract::BaseDependent);
    }
}
