//! Bounded pre-decode validation of untrusted protobuf bytes.
//!
//! Prost allocates as it decodes, so the decoder must never see an input whose
//! size or shape it has not already approved. This pass walks the wire format
//! for the fixed set of messages the middleware accepts and, without
//! allocating, enforces three things:
//!
//! 1. Count and size limits from the [`ResourceProfile`].
//! 2. Wire-level rules: known fields carry their declared wire type, field
//!    numbers are valid, required fields are present, and nonempty SHA-256
//!    fields are exactly 32 bytes.
//! 3. Unsupported structures are rejected rather than decoded and ignored.
//! 4. A singular byte, string or message field appears at most once. Prost
//!    would replace or merge into the value it already holds, and the model
//!    does not charge that. Repeated scalars are accepted, since prost keeps the
//!    last value and allocates nothing for them.
//!
//! The scan also returns the modeled decode peak as a [`Usage`], which is
//! checked against the profile budget before prost runs. The model charges
//! every vector at its final capacity, its largest transient, and each
//! allocation's overhead; see `docs/memory.md`.
//!
//! Recursion is structurally bounded: the walk follows the schema from
//! `DeltaArchiveManifest` to `PartitionUpdate` to `InstallOperation` to
//! `Extent`, a fixed depth of four. Unknown fields are skipped by their
//! declared length. Groups are rejected, which is stricter than prost.

use crate::error::err;
use crate::error::Code;
use crate::error::PayloadError;
use crate::profile::ResourceProfile;
use crate::profile::Usage;
use crate::profile::EXTENT_COST;
use crate::profile::MANIFEST_COST;
use crate::profile::MAX_STRING_BYTES;
use crate::profile::OP_COST;
use crate::profile::PARTITION_COST;
use crate::profile::SIGNATURES_COST;
use crate::profile::SIGNATURE_COST;
use crate::profile::SPAN_COST;
use crate::sha256::SHA256_SIZE;

const WIRE_VARINT: u8 = 0;
const WIRE_LEN: u8 = 2;
const WIRE_FIXED32: u8 = 5;
const MAX_FIELD: u64 = (1 << 29) - 1;

const BAD_WIRE: &str = "malformed protobuf";

fn bad_wire() -> PayloadError {
    return err(Code::bad_protobuf, BAD_WIRE);
}

fn unsupported(msg: &'static str) -> PayloadError {
    return err(Code::unsupported_field, msg);
}

fn limit(msg: &'static str) -> PayloadError {
    return err(Code::resource_limit, msg);
}

const DUPLICATE: &str = "duplicate singular field is not supported";

#[derive(Default)]
struct Seen(u128);

impl Seen {
    fn once(&mut self, field: u64) -> Result<(), PayloadError> {
        if self.has(field) {
            return Err(err(Code::bad_protobuf, DUPLICATE));
        }
        self.note(field);
        return Ok(());
    }

    fn note(&mut self, field: u64) {
        debug_assert!(field < 128);
        self.0 |= 1 << field;
    }

    fn has(&self, field: u64) -> bool {
        debug_assert!(field < 128);
        return self.0 & (1 << field) != 0;
    }
}

pub(crate) struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    pub(crate) fn new(buf: &'a [u8]) -> Reader<'a> {
        return Reader { buf: buf, pos: 0 };
    }

    fn byte(&mut self) -> Result<u8, PayloadError> {
        if self.pos >= self.buf.len() {
            return Err(bad_wire());
        }
        let b = self.buf[self.pos];
        self.pos += 1;
        return Ok(b);
    }

    fn varint(&mut self) -> Result<u64, PayloadError> {
        let mut v: u64 = 0;
        let mut i = 0;
        while i < 10 {
            let b = self.byte()?;
            if i == 9 && b > 1 {
                return Err(bad_wire());
            }
            v |= ((b & 0x7f) as u64) << (7 * i);
            if b & 0x80 == 0 {
                return Ok(v);
            }
            i += 1;
        }
        return Err(bad_wire());
    }

    pub(crate) fn key(&mut self) -> Result<Option<(u64, u8)>, PayloadError> {
        if self.pos >= self.buf.len() {
            return Ok(None);
        }
        let k = self.varint()?;
        let field = k >> 3;
        if field == 0 || field > MAX_FIELD {
            return Err(bad_wire());
        }
        return Ok(Some((field, (k & 7) as u8)));
    }

    pub(crate) fn bytes(&mut self) -> Result<&'a [u8], PayloadError> {
        let n = self.varint()?;
        let remaining = (self.buf.len() - self.pos) as u64;
        if n > remaining {
            return Err(bad_wire());
        }
        let start = self.pos;
        self.pos += n as usize;
        return Ok(&self.buf[start..self.pos]);
    }

    fn fixed32(&mut self) -> Result<(), PayloadError> {
        let mut i = 0;
        while i < 4 {
            self.byte()?;
            i += 1;
        }
        return Ok(());
    }

    pub(crate) fn skip(&mut self, wire: u8) -> Result<(), PayloadError> {
        match wire {
            WIRE_VARINT => {
                self.varint()?;
            }
            1 => {
                let mut i = 0;
                while i < 8 {
                    self.byte()?;
                    i += 1;
                }
            }
            WIRE_LEN => {
                self.bytes()?;
            }
            WIRE_FIXED32 => self.fixed32()?,
            _ => return Err(bad_wire()),
        }
        return Ok(());
    }
}

fn expect(wire: u8, want: u8) -> Result<(), PayloadError> {
    if wire != want {
        return Err(bad_wire());
    }
    return Ok(());
}

fn string_field(usage: &mut Usage, s: &[u8]) -> Result<(), PayloadError> {
    if s.len() > MAX_STRING_BYTES {
        return Err(limit("string field longer than the profile allows"));
    }
    usage.bytes(s.len());
    return Ok(());
}

fn hash_field(usage: &mut Usage, h: &[u8]) -> Result<(), PayloadError> {
    if h.is_empty() {
        return Ok(());
    }
    if h.len() != SHA256_SIZE {
        return Err(err(Code::hash_length, "SHA-256 field must be 32 bytes"));
    }
    usage.bytes(h.len());
    return Ok(());
}

/// Scan a `DeltaArchiveManifest`. Returns the modeled decode peak, which has
/// already been checked against the profile's decoded-memory budget.
pub fn scan_manifest(bytes: &[u8], profile: &ResourceProfile) -> Result<Usage, PayloadError> {
    if bytes.len() > profile.max_encoded_manifest {
        return Err(err(Code::manifest_too_big, "manifest too big"));
    }
    let mut usage = Usage::with_base(MANIFEST_COST);
    let mut ops_total: usize = 0;
    let mut extents_total: usize = 0;
    let mut partitions: usize = 0;
    let mut seen = Seen::default();
    let mut r = Reader::new(bytes);
    while let Some((field, wire)) = r.key()? {
        match field {
            3 | 4 | 5 | 12 | 14 | 16 => {
                expect(wire, WIRE_VARINT)?;
                r.varint()?;
            }
            13 => {
                expect(wire, WIRE_LEN)?;
                partitions += 1;
                if partitions > profile.max_partitions {
                    return Err(limit("too many partitions"));
                }
                scan_partition(
                    r.bytes()?,
                    profile,
                    &mut usage,
                    &mut ops_total,
                    &mut extents_total,
                )?;
            }
            18 => {
                expect(wire, WIRE_LEN)?;
                seen.once(field)?;
                string_field(&mut usage, r.bytes()?)?;
            }
            100 => {
                expect(wire, WIRE_VARINT)?;
                seen.once(field)?;
                r.varint()?;
            }
            15 | 17 => return Err(unsupported("manifest field not supported")),
            _ => r.skip(wire)?,
        }
    }
    usage.vector(partitions, PARTITION_COST);
    usage.temporary(extents_total, SPAN_COST);
    if usage.accounted() > profile.max_decoded_bytes {
        return Err(limit("manifest exceeds the decoded-memory budget"));
    }
    return Ok(usage);
}

fn scan_partition(
    bytes: &[u8],
    profile: &ResourceProfile,
    usage: &mut Usage,
    ops_total: &mut usize,
    extents_total: &mut usize,
) -> Result<(), PayloadError> {
    let mut seen = Seen::default();
    let mut ops_here: usize = 0;
    let mut r = Reader::new(bytes);
    while let Some((field, wire)) = r.key()? {
        match field {
            1 => {
                expect(wire, WIRE_LEN)?;
                seen.once(field)?;
                let name = r.bytes()?;
                if name.is_empty() {
                    return Err(err(
                        Code::required_field_missing,
                        "PartitionUpdate.partition_name required",
                    ));
                }
                string_field(usage, name)?;
            }
            2 => {
                expect(wire, WIRE_VARINT)?;
                if r.varint()? != 0 {
                    return Err(unsupported("postinstall is not supported"));
                }
            }
            3 | 5 | 10 | 11 | 12 | 13 | 14 | 15 | 18 => {
                return Err(unsupported("partition field not supported"));
            }
            4 | 17 => {
                expect(wire, WIRE_LEN)?;
                seen.once(field)?;
                string_field(usage, r.bytes()?)?;
            }
            6 | 7 => {
                expect(wire, WIRE_LEN)?;
                seen.once(field)?;
                scan_partition_info(r.bytes()?, usage)?;
            }
            8 => {
                expect(wire, WIRE_LEN)?;
                ops_here += 1;
                *ops_total += 1;
                if *ops_total > profile.max_operations {
                    return Err(limit("too many operations"));
                }
                scan_operation(r.bytes()?, profile, usage, extents_total)?;
            }
            9 | 16 | 19 | 20 => {
                expect(wire, WIRE_VARINT)?;
                r.varint()?;
            }
            _ => r.skip(wire)?,
        }
    }
    if !seen.has(1) {
        return Err(err(
            Code::required_field_missing,
            "PartitionUpdate.partition_name required",
        ));
    }
    usage.vector(ops_here, OP_COST);
    return Ok(());
}

fn scan_partition_info(bytes: &[u8], usage: &mut Usage) -> Result<(), PayloadError> {
    let mut seen = Seen::default();
    let mut r = Reader::new(bytes);
    while let Some((field, wire)) = r.key()? {
        match field {
            1 => {
                expect(wire, WIRE_VARINT)?;
                r.varint()?;
            }
            2 => {
                expect(wire, WIRE_LEN)?;
                seen.once(field)?;
                hash_field(usage, r.bytes()?)?;
            }
            _ => r.skip(wire)?,
        }
    }
    return Ok(());
}

fn scan_operation(
    bytes: &[u8],
    profile: &ResourceProfile,
    usage: &mut Usage,
    extents_total: &mut usize,
) -> Result<(), PayloadError> {
    let mut src_count: usize = 0;
    let mut dst_count: usize = 0;
    let mut seen = Seen::default();
    let mut r = Reader::new(bytes);
    while let Some((field, wire)) = r.key()? {
        match field {
            1 => {
                expect(wire, WIRE_VARINT)?;
                r.varint()?;
                seen.note(field);
            }
            2 | 3 | 5 | 7 => {
                expect(wire, WIRE_VARINT)?;
                r.varint()?;
            }
            4 | 6 => {
                expect(wire, WIRE_LEN)?;
                scan_extent(r.bytes()?)?;
                if field == 4 {
                    src_count += 1;
                } else {
                    dst_count += 1;
                }
                *extents_total += 1;
                if *extents_total > profile.max_total_extents {
                    return Err(limit("too many extents"));
                }
                if src_count + dst_count > profile.max_extents_per_op {
                    return Err(limit("too many extents in one operation"));
                }
            }
            8 | 9 => {
                expect(wire, WIRE_LEN)?;
                seen.once(field)?;
                hash_field(usage, r.bytes()?)?;
            }
            100 => {
                expect(wire, WIRE_LEN)?;
                seen.once(field)?;
                let packed = r.bytes()?;
                let bound = profile
                    .max_frame_table_bytes
                    .saturating_add(profile.max_map_bytes);
                if packed.len() > bound {
                    return Err(limit("frame table exceeds the profile"));
                }
                usage.bytes(packed.len());
            }
            _ => r.skip(wire)?,
        }
    }
    if !seen.has(1) {
        return Err(err(
            Code::required_field_missing,
            "InstallOperation.type required",
        ));
    }
    usage.vector(src_count, EXTENT_COST);
    usage.vector(dst_count, EXTENT_COST);
    return Ok(());
}

fn scan_extent(bytes: &[u8]) -> Result<(), PayloadError> {
    let mut seen = Seen::default();
    let mut r = Reader::new(bytes);
    while let Some((field, wire)) = r.key()? {
        match field {
            1 | 2 => {
                expect(wire, WIRE_VARINT)?;
                seen.note(field);
                r.varint()?;
            }
            _ => r.skip(wire)?,
        }
    }
    if !seen.has(1) || !seen.has(2) {
        return Err(err(
            Code::required_field_missing,
            "Extent.start_byte and num_bytes are required",
        ));
    }
    return Ok(());
}

/// Scan a `Signatures` message: its size, entry count, per-entry data size, and
/// wire shape. The size comes first, so an oversized message is refused before
/// a single byte of it is interpreted. Every signature decoder goes through
/// here, so the profile's message limit holds on every path. Returns the
/// modeled decode peak, checked against the same budget as the manifest.
pub fn scan_signatures(bytes: &[u8], profile: &ResourceProfile) -> Result<Usage, PayloadError> {
    if bytes.len() > profile.max_signature_message {
        return Err(limit("signature message exceeds the profile"));
    }
    let mut usage = Usage::with_base(SIGNATURES_COST);
    let mut count: usize = 0;
    let mut r = Reader::new(bytes);
    while let Some((field, wire)) = r.key()? {
        match field {
            1 => {
                expect(wire, WIRE_LEN)?;
                count += 1;
                if count > profile.max_signatures {
                    return Err(limit("too many signature entries"));
                }
                scan_signature(r.bytes()?, profile, &mut usage)?;
            }
            _ => r.skip(wire)?,
        }
    }
    usage.vector(count, SIGNATURE_COST);
    if usage.accounted() > profile.max_decoded_bytes {
        return Err(limit("signature message exceeds the decoded-memory budget"));
    }
    return Ok(usage);
}

fn scan_signature(
    bytes: &[u8],
    profile: &ResourceProfile,
    usage: &mut Usage,
) -> Result<(), PayloadError> {
    let mut seen = Seen::default();
    let mut r = Reader::new(bytes);
    while let Some((field, wire)) = r.key()? {
        match field {
            1 => {
                expect(wire, WIRE_VARINT)?;
                r.varint()?;
            }
            2 => {
                expect(wire, WIRE_LEN)?;
                seen.once(field)?;
                let data = r.bytes()?;
                if data.len() > profile.max_signature_data {
                    return Err(limit("signature data exceeds the profile"));
                }
                usage.bytes(data.len());
            }
            3 => {
                expect(wire, WIRE_FIXED32)?;
                r.skip(WIRE_FIXED32)?;
            }
            _ => r.skip(wire)?,
        }
    }
    return Ok(());
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::TYPE_REPLACE;
    use crate::manifest::TYPE_REPLACE_FRAMES;
    use crate::manifest::TYPE_REPLACE_SPARSE;
    use crate::manifest::TYPE_TARGET_COPY;
    use crate::profile::vec_slots;
    use crate::profile::ALLOC_OVERHEAD;
    use crate::profile::EMBEDDED;
    use alloc::vec;
    use alloc::vec::Vec;

    fn put_varint(out: &mut Vec<u8>, field: u64, v: u64) {
        varint(out, field << 3);
        varint(out, v);
    }

    fn put_bytes(out: &mut Vec<u8>, field: u64, data: &[u8]) {
        varint(out, (field << 3) | 2);
        varint(out, data.len() as u64);
        out.extend_from_slice(data);
    }

    fn varint(out: &mut Vec<u8>, mut v: u64) {
        loop {
            let b = (v & 0x7f) as u8;
            v >>= 7;
            if v == 0 {
                out.push(b);
                return;
            }
            out.push(b | 0x80);
        }
    }

    fn extent() -> Vec<u8> {
        let mut e = Vec::new();
        put_varint(&mut e, 1, 0);
        put_varint(&mut e, 2, 1);
        return e;
    }

    fn operation(typed: bool, dst: usize, hash: Option<&[u8]>) -> Vec<u8> {
        let mut o = Vec::new();
        if typed {
            put_varint(&mut o, 1, 0);
        }
        let e = extent();
        let mut i = 0;
        while i < dst {
            put_bytes(&mut o, 6, &e);
            i += 1;
        }
        if let Some(h) = hash {
            put_bytes(&mut o, 8, h);
        }
        return o;
    }

    fn manifest(ops: &[Vec<u8>]) -> Vec<u8> {
        let mut part = Vec::new();
        put_bytes(&mut part, 1, b"A/B");
        for op in ops {
            put_bytes(&mut part, 8, op);
        }
        let mut m = Vec::new();
        put_varint(&mut m, 3, 4096);
        put_bytes(&mut m, 13, &part);
        return m;
    }

    fn uniform(ops: usize, dst: usize) -> Vec<u8> {
        let op = operation(true, dst, None);
        let list: Vec<Vec<u8>> = (0..ops).map(|_| op.clone()).collect();
        return manifest(&list);
    }

    fn scan(bytes: &[u8]) -> Result<Usage, PayloadError> {
        return scan_manifest(bytes, &EMBEDDED);
    }

    fn unbudgeted() -> ResourceProfile {
        return ResourceProfile {
            max_decoded_bytes: usize::MAX,
            ..EMBEDDED
        };
    }

    fn code_of(result: Result<Usage, PayloadError>) -> Code {
        match result {
            Ok(_) => panic!("expected an error"),
            Err(e) => return e.code,
        }
    }

    #[test]
    fn oversized_encoded_manifest_is_refused_before_scanning() {
        let mut tiny = EMBEDDED;
        tiny.max_encoded_manifest = 8;
        let m = manifest(&[operation(true, 1, None)]);
        assert!(m.len() > 8);
        assert_eq!(
            scan_manifest(&m, &tiny).err().map(|e| e.code),
            Some(Code::manifest_too_big)
        );
    }

    #[test]
    fn vec_slots_follow_push_growth_at_each_boundary() {
        let table = [
            (0, 0),
            (1, 4),
            (4, 4),
            (5, 8),
            (8, 8),
            (9, 16),
            (16, 16),
            (17, 32),
            (32, 32),
            (33, 64),
            (64, 64),
            (65, 128),
        ];
        for (n, slots) in table {
            assert_eq!(vec_slots(n), slots, "slots for {} elements", n);
        }
        assert_eq!(vec_slots(usize::MAX), usize::MAX);
    }

    #[test]
    fn accounting_charges_every_buffer_and_its_overhead() {
        let one = scan(&manifest(&[operation(true, 1, Some(&[7; 32]))])).unwrap();
        assert_eq!(one.accounted(), 256 + 2048 + 768 + 128 + 160 + 56);
        assert_eq!(one.transient_bytes, 56);

        let four = scan(&uniform(4, 1)).unwrap();
        assert_eq!(
            four.accounted(),
            256 + 2048 + 4 * 192 + 4 * 128 + 24 + 32 + 24 + 4 * 24 + 32 + (88 - 32)
        );
        assert_eq!(four.transient_bytes, 4 * SPAN_COST + ALLOC_OVERHEAD);

        let five = scan(&uniform(5, 1)).unwrap();
        assert_eq!(five.transient_bytes, 4 * 192 + ALLOC_OVERHEAD);
        assert_eq!(five.accounted() - four.accounted(), 1680 - 56);
    }

    #[test]
    fn byte_fields_are_charged_at_their_minimum_capacity() {
        let mut u = Usage::with_base(0);
        u.bytes(0);
        assert_eq!(u.heap_bytes, 0);
        assert_eq!(u.transient_bytes, 0);
        u.bytes(1);
        assert_eq!(u.heap_bytes, 8 + ALLOC_OVERHEAD);
        u.bytes(9);
        assert_eq!(u.heap_bytes, 8 + 9 + 2 * ALLOC_OVERHEAD);
        assert_eq!(u.transient_bytes, 9 + ALLOC_OVERHEAD);
    }

    #[test]
    fn growth_is_a_transient_only_above_the_minimum_capacity() {
        let mut u = Usage::with_base(0);
        u.vector(4, 100);
        assert_eq!(u.transient_bytes, 0);
        u.vector(5, 100);
        assert_eq!(u.transient_bytes, 4 * 100 + ALLOC_OVERHEAD);
        let mut u = Usage::with_base(0);
        u.vector(9, 100);
        assert_eq!(u.transient_bytes, 8 * 100 + ALLOC_OVERHEAD);
    }

    #[test]
    fn only_the_largest_transient_is_charged() {
        let mut u = Usage::with_base(0);
        u.vector(5, 100);
        u.bytes(16);
        assert_eq!(u.transient_bytes, 4 * 100 + ALLOC_OVERHEAD);
    }

    #[test]
    fn accounted_is_monotone_in_every_count() {
        let profile = unbudgeted();
        let mut last = 0;
        for ops in 1..=EMBEDDED.max_operations {
            let cost = scan_manifest(&uniform(ops, 1), &profile)
                .unwrap()
                .accounted();
            assert!(
                cost >= last,
                "{} operations cost less than {}",
                ops,
                ops - 1
            );
            last = cost;
        }
        let mut last = 0;
        for dst in 1..=EMBEDDED.max_total_extents {
            let cost = scan_manifest(&uniform(1, dst), &profile)
                .unwrap()
                .accounted();
            assert!(cost >= last, "{} extents cost less than {}", dst, dst - 1);
            last = cost;
        }
    }

    #[test]
    fn operation_count_over_the_profile_is_a_resource_limit() {
        let op = operation(true, 1, None);
        let ops: Vec<Vec<u8>> = (0..EMBEDDED.max_operations + 1)
            .map(|_| op.clone())
            .collect();
        assert_eq!(code_of(scan(&manifest(&ops))), Code::resource_limit);
    }

    #[test]
    fn extents_per_operation_and_in_total_are_bounded() {
        let wide = operation(true, EMBEDDED.max_extents_per_op + 1, None);
        assert_eq!(code_of(scan(&manifest(&[wide]))), Code::resource_limit);
        let op = operation(true, EMBEDDED.max_total_extents / 2 + 1, None);
        let ops = vec![op.clone(), op];
        assert_eq!(code_of(scan(&manifest(&ops))), Code::resource_limit);
    }

    #[test]
    fn count_limits_are_not_all_admissible_together() {
        let per_op = EMBEDDED.max_total_extents / EMBEDDED.max_operations;
        let op = {
            let mut o = operation(true, per_op, Some(&[9; 32]));
            put_bytes(&mut o, 9, &[7; 32]);
            o
        };
        let ops: Vec<Vec<u8>> = (0..EMBEDDED.max_operations).map(|_| op.clone()).collect();
        let bytes = manifest(&ops);
        assert_eq!(code_of(scan(&bytes)), Code::resource_limit);
        let modeled = scan_manifest(&bytes, &unbudgeted()).unwrap().accounted();
        assert!(modeled > EMBEDDED.max_decoded_bytes);
    }

    #[test]
    fn the_contract_workspace_is_a_transient_that_counts_against_the_budget() {
        let bytes = uniform(2, 64);
        let usage = scan_manifest(&bytes, &unbudgeted()).unwrap();
        assert_eq!(usage.transient_bytes, 128 * SPAN_COST + ALLOC_OVERHEAD);
        let accounted = usage.accounted();
        let under = ResourceProfile {
            max_decoded_bytes: accounted - 1,
            ..EMBEDDED
        };
        assert_eq!(
            scan_manifest(&bytes, &under).err().map(|e| e.code),
            Some(Code::resource_limit)
        );
        let exact = ResourceProfile {
            max_decoded_bytes: accounted,
            ..EMBEDDED
        };
        assert!(scan_manifest(&bytes, &exact).is_ok());
    }

    #[test]
    fn hashes_must_be_empty_or_exactly_sha256_length() {
        for len in [1usize, 31, 33, 64] {
            let m = manifest(&[operation(true, 1, Some(&vec![0; len]))]);
            assert_eq!(code_of(scan(&m)), Code::hash_length, "{}-byte hash", len);
        }
        for hash in [None, Some(&[][..]), Some(&[0u8; 32][..])] {
            assert!(scan(&manifest(&[operation(true, 1, hash)])).is_ok());
        }
    }

    #[test]
    fn missing_operation_type_is_rejected_and_explicit_zero_is_accepted() {
        let missing = manifest(&[operation(false, 1, None)]);
        assert_eq!(code_of(scan(&missing)), Code::required_field_missing);
        let explicit = manifest(&[operation(true, 1, None)]);
        assert!(scan(&explicit).is_ok());
    }

    #[test]
    fn unsupported_structures_are_rejected_not_ignored() {
        let mut m = manifest(&[operation(true, 1, None)]);
        put_bytes(&mut m, 17, &[0x0a, 0x00]);
        assert_eq!(code_of(scan(&m)), Code::unsupported_field);
    }

    #[test]
    fn malformed_wire_is_rejected_before_decoding() {
        for bad in [
            &[0x80u8][..],
            &[0x00, 0x00][..],
            &[0x0b][..],
            &[0x12, 0x05, 0x00][..],
        ] {
            assert_eq!(code_of(scan(bad)), Code::bad_protobuf, "input {:?}", bad);
        }
    }

    #[test]
    fn unknown_scalar_fields_are_skipped() {
        let mut m = manifest(&[operation(true, 1, None)]);
        put_varint(&mut m, 40, 123);
        assert!(scan(&m).is_ok());
    }

    fn pad_to(msg: &mut Vec<u8>, total: usize) {
        let room = total - msg.len();
        let mut len = room;
        while len > 0 && 1 + varint_len(len as u64) + len != room {
            len -= 1;
        }
        msg.push((15 << 3) | 2);
        varint(msg, len as u64);
        msg.extend(vec![0u8; len]);
        assert_eq!(msg.len(), total);
    }

    fn varint_len(mut v: u64) -> usize {
        let mut n = 1;
        while v >= 0x80 {
            v >>= 7;
            n += 1;
        }
        n
    }

    #[test]
    fn signature_message_size_is_bounded_before_scanning() {
        let limit = EMBEDDED.max_signature_message;
        let mut at_limit = Vec::new();
        put_bytes(&mut at_limit, 1, &[]);
        pad_to(&mut at_limit, limit);
        assert!(scan_signatures(&at_limit, &EMBEDDED).is_ok());

        let mut over = at_limit.clone();
        over.resize(limit + 1, 0x80);
        assert_eq!(
            scan_signatures(&over, &EMBEDDED).err().map(|e| e.code),
            Some(Code::resource_limit)
        );
    }

    fn entry_with_data(lens: &[usize]) -> Vec<u8> {
        let mut e = Vec::new();
        for &n in lens {
            put_bytes(&mut e, 2, &vec![0xab; n]);
        }
        return e;
    }

    fn signatures_of(body: &[u8]) -> Vec<u8> {
        let mut m = Vec::new();
        put_bytes(&mut m, 1, body);
        return m;
    }

    fn with_partition(part: &[u8]) -> Vec<u8> {
        let mut m = Vec::new();
        put_varint(&mut m, 3, 4096);
        put_bytes(&mut m, 13, part);
        return m;
    }

    #[test]
    fn reported_signature_data_sequence_is_refused_in_prescan() {
        let msg = signatures_of(&entry_with_data(&[256, 257]));
        assert_eq!(
            code_of(scan_signatures(&msg, &EMBEDDED)),
            Code::bad_protobuf
        );
    }

    #[test]
    fn duplicate_signature_data_is_refused_at_every_replacement_size() {
        let sizes = [0usize, 1, 7, 8, 9, 255, 256, 257, 511, 512];
        for &first in &sizes {
            for &second in &sizes {
                let msg = signatures_of(&entry_with_data(&[first, second]));
                assert_eq!(
                    code_of(scan_signatures(&msg, &EMBEDDED)),
                    Code::bad_protobuf,
                    "{} then {} bytes",
                    first,
                    second
                );
            }
        }
    }

    #[test]
    fn duplicate_strings_are_refused() {
        let mut m = manifest(&[operation(true, 1, None)]);
        put_bytes(&mut m, 18, b"2026-01");
        put_bytes(&mut m, 18, b"2026-02");
        assert_eq!(code_of(scan(&m)), Code::bad_protobuf);
        for field in [1u64, 4, 17] {
            let mut part = Vec::new();
            put_bytes(&mut part, 1, b"A/B");
            put_bytes(&mut part, field, b"ext4");
            put_bytes(&mut part, field, b"ext4");
            assert_eq!(
                code_of(scan(&with_partition(&part))),
                Code::bad_protobuf,
                "partition field {}",
                field
            );
        }
    }

    #[test]
    fn duplicate_hashes_are_refused() {
        for field in [8u64, 9] {
            let mut op = operation(true, 1, None);
            put_bytes(&mut op, field, &[7; 32]);
            put_bytes(&mut op, field, &[7; 32]);
            assert_eq!(
                code_of(scan(&manifest(&[op]))),
                Code::bad_protobuf,
                "operation field {}",
                field
            );
        }
        let mut info = Vec::new();
        put_bytes(&mut info, 2, &[3; 32]);
        put_bytes(&mut info, 2, &[3; 32]);
        let mut part = Vec::new();
        put_bytes(&mut part, 1, b"A/B");
        put_bytes(&mut part, 6, &info);
        assert_eq!(code_of(scan(&with_partition(&part))), Code::bad_protobuf);
    }

    #[test]
    fn duplicate_submessages_are_refused() {
        let mut info = Vec::new();
        put_bytes(&mut info, 2, &[3; 32]);
        for field in [6u64, 7] {
            let mut part = Vec::new();
            put_bytes(&mut part, 1, b"A/B");
            put_bytes(&mut part, field, &info);
            put_bytes(&mut part, field, &info);
            assert_eq!(
                code_of(scan(&with_partition(&part))),
                Code::bad_protobuf,
                "partition submessage {}",
                field
            );
            let mut part = Vec::new();
            put_bytes(&mut part, 1, b"A/B");
            put_bytes(&mut part, field, &[]);
            put_bytes(&mut part, field, &[]);
            assert_eq!(
                code_of(scan(&with_partition(&part))),
                Code::bad_protobuf,
                "empty partition submessage {}",
                field
            );
        }
    }

    #[test]
    fn repeated_and_unknown_fields_remain_accepted() {
        let op = operation(true, 3, Some(&[7; 32]));
        let mut m = manifest(&vec![op; 5]);
        put_varint(&mut m, 40, 1);
        put_varint(&mut m, 40, 2);
        put_bytes(&mut m, 41, b"x");
        put_bytes(&mut m, 41, b"y");
        assert!(scan(&m).is_ok());
        let mut sigs = Vec::new();
        for _ in 0..3 {
            put_bytes(&mut sigs, 1, &entry_with_data(&[32]));
        }
        assert!(scan_signatures(&sigs, &EMBEDDED).is_ok());
        let mut entry = entry_with_data(&[32]);
        put_varint(&mut entry, 9, 1);
        put_varint(&mut entry, 9, 2);
        assert!(scan_signatures(&signatures_of(&entry), &EMBEDDED).is_ok());
        let mut scalars = manifest(&[operation(true, 1, None)]);
        put_varint(&mut scalars, 3, 8192);
        assert!(scan(&scalars).is_ok());
    }

    #[test]
    fn signature_entries_and_data_are_bounded() {
        let mut too_many = Vec::new();
        let mut i = 0;
        while i < EMBEDDED.max_signatures + 1 {
            put_bytes(&mut too_many, 1, &[]);
            i += 1;
        }
        assert_eq!(
            scan_signatures(&too_many, &EMBEDDED).err().map(|e| e.code),
            Some(Code::resource_limit)
        );
        let mut big = Vec::new();
        let mut entry = Vec::new();
        put_bytes(&mut entry, 2, &vec![0; EMBEDDED.max_signature_data + 1]);
        put_bytes(&mut big, 1, &entry);
        assert_eq!(
            scan_signatures(&big, &EMBEDDED).err().map(|e| e.code),
            Some(Code::resource_limit)
        );
    }

    #[test]
    fn frame_table_is_bounded_and_a_repeat_is_refused() {
        let bound = EMBEDDED.max_frame_table_bytes + EMBEDDED.max_map_bytes;
        let mut op = operation(true, 1, None);
        put_bytes(&mut op, 100, &vec![0u8; bound]);
        assert!(scan(&manifest(&[op])).is_ok());
        let mut op = operation(true, 1, None);
        put_bytes(&mut op, 100, &vec![0u8; bound + 1]);
        assert_eq!(code_of(scan(&manifest(&[op]))), Code::resource_limit);
        let mut dup = operation(true, 1, None);
        put_bytes(&mut dup, 100, &[0]);
        put_bytes(&mut dup, 100, &[0]);
        assert_eq!(code_of(scan(&manifest(&[dup]))), Code::bad_protobuf);
    }

    fn packed_op(typ: i32, packed: &[u8], table_first: bool) -> Vec<u8> {
        let mut kind = Vec::new();
        put_varint(&mut kind, 1, typ as u64);
        let mut table = Vec::new();
        put_bytes(&mut table, 100, packed);
        let mut extent = Vec::new();
        put_varint(&mut extent, 1, 0);
        put_varint(&mut extent, 2, 4096);
        let mut op = Vec::new();
        if table_first {
            op.extend_from_slice(&table);
            op.extend_from_slice(&kind);
        } else {
            op.extend_from_slice(&kind);
            op.extend_from_slice(&table);
        }
        put_bytes(&mut op, 6, &extent);
        return op;
    }

    fn packed_verdict(typ: i32, packed: &[u8], table_first: bool) -> Option<Code> {
        let op = packed_op(typ, packed, table_first);
        return scan(&manifest(&[op])).err().map(|e| e.code);
    }

    #[test]
    fn packed_field_is_bounded_by_its_combined_limit() {
        let limit = EMBEDDED.max_map_bytes + EMBEDDED.max_frame_table_bytes;
        let at = vec![0u8; limit];
        assert_eq!(packed_verdict(TYPE_REPLACE_FRAMES, &at, false), None);
        assert_eq!(packed_verdict(TYPE_REPLACE_SPARSE, &at, false), None);
        let over = vec![0u8; limit + 1];
        assert_eq!(
            packed_verdict(TYPE_REPLACE_FRAMES, &over, false),
            Some(Code::resource_limit)
        );
        assert_eq!(
            packed_verdict(TYPE_REPLACE_SPARSE, &over, false),
            Some(Code::resource_limit)
        );
    }

    #[test]
    fn protobuf_field_order_does_not_change_the_verdict() {
        let limit = EMBEDDED.max_map_bytes + EMBEDDED.max_frame_table_bytes;
        for len in [0, 1, limit, limit + 1] {
            let packed = vec![0u8; len];
            for typ in [TYPE_REPLACE_FRAMES, TYPE_REPLACE_SPARSE] {
                assert_eq!(
                    packed_verdict(typ, &packed, false),
                    packed_verdict(typ, &packed, true),
                    "{} bytes",
                    len
                );
            }
        }
    }

    #[test]
    fn extent_fields_are_shape_checked_and_not_summed() {
        let mut op = packed_op(TYPE_REPLACE_FRAMES, &[0], false);
        let mut extent = Vec::new();
        put_varint(&mut extent, 1, 0);
        put_varint(&mut extent, 2, u64::MAX);
        put_bytes(&mut op, 6, &extent);
        put_bytes(&mut op, 6, &extent);
        assert!(scan(&manifest(&[op])).is_ok());
    }

    fn op_with_extent(typ: u64, field: u64, extent: &[u8]) -> Vec<u8> {
        let mut o = Vec::new();
        put_varint(&mut o, 1, typ);
        put_bytes(&mut o, field, extent);
        return o;
    }

    fn extent_with(start: Option<u64>, len: Option<u64>) -> Vec<u8> {
        let mut e = Vec::new();
        if let Some(s) = start {
            put_varint(&mut e, 1, s);
        }
        if let Some(n) = len {
            put_varint(&mut e, 2, n);
        }
        return e;
    }

    #[test]
    fn an_extent_must_state_both_fields_and_a_written_zero_start_is_present() {
        let missing = [
            extent_with(None, Some(4096)),
            extent_with(Some(0), None),
            extent_with(None, None),
        ];
        for e in &missing {
            let dst = op_with_extent(TYPE_REPLACE as u64, 6, e);
            assert_eq!(code_of(scan(&manifest(&[dst]))), Code::required_field_missing);
            let src = op_with_extent(TYPE_TARGET_COPY as u64, 4, e);
            assert_eq!(code_of(scan(&manifest(&[src]))), Code::required_field_missing);
        }
        let zero = extent_with(Some(0), Some(4096));
        assert!(scan(&manifest(&[op_with_extent(TYPE_REPLACE as u64, 6, &zero)])).is_ok());
    }
}
