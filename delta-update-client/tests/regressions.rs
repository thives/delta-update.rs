use std::cell::Cell;

use delta_update_client::apply::NoProgress;
use delta_update_client::apply::Trust;
use delta_update_client::checkpoint::MemoryStore;
use delta_update_client::checkpoint::TargetId;
use delta_update_client::policy::Policy;
use delta_update_client::sink::Readable;
use delta_update_client::sink::Sink;
use delta_update_client::sink::check_range;
use delta_update_client::source::Source;
use delta_update_generator::SearchConfig;
use delta_update_generator::build_payload_with;
use delta_update_payload::error::Code;
use delta_update_payload::error::PayloadError;
use delta_update_payload::profile::EMBEDDED;
use delta_update_payload::rsa::PubKey;
use delta_update_payload::rsa::parse_pubkey_pem;
use delta_update_payload::sha256;

const BS: usize = 8;
const BLOCKS: usize = 4;
const TARGET: TargetId = [3; 32];

const PRIVATE: &[u8] = include_bytes!("../../delta-update-payload/tests/fixtures/private.pem");
const PUBLIC: &[u8] = include_bytes!("../../delta-update-payload/tests/fixtures/public.pem");

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

fn put_varint(out: &mut Vec<u8>, field: u64, v: u64) {
    varint(out, field << 3);
    varint(out, v);
}

fn put_bytes(out: &mut Vec<u8>, field: u64, data: &[u8]) {
    varint(out, (field << 3) | 2);
    varint(out, data.len() as u64);
    out.extend_from_slice(data);
}

fn extent(start: u64, num: u64) -> Vec<u8> {
    let mut e = Vec::new();
    put_varint(&mut e, 1, start.wrapping_mul(BS as u64));
    put_varint(&mut e, 2, num.wrapping_mul(BS as u64));
    e
}

fn digest(data: &[u8]) -> Vec<u8> {
    return sha256::hash_bytes(data).to_vec();
}

fn operation(
    typ: Option<u64>,
    offset: u64,
    length: u64,
    dst: &[(u64, u64)],
    hash: Option<&[u8]>,
) -> Vec<u8> {
    let mut o = Vec::new();
    if let Some(t) = typ {
        put_varint(&mut o, 1, t);
    }
    put_varint(&mut o, 2, offset);
    put_varint(&mut o, 3, length);
    for (s, n) in dst {
        put_bytes(&mut o, 6, &extent(*s, *n));
    }
    if let Some(h) = hash {
        put_bytes(&mut o, 8, h);
    }
    o
}

fn manifest(ops: &[Vec<u8>]) -> Vec<u8> {
    let mut part = Vec::new();
    put_bytes(&mut part, 1, b"A/B");
    for op in ops {
        put_bytes(&mut part, 8, op);
    }
    let mut m = Vec::new();
    put_varint(&mut m, 3, BS as u64);
    put_varint(&mut m, 12, 17);
    put_bytes(&mut m, 13, &part);
    m
}

fn payload(manifest: &[u8], blobs: &[u8]) -> Vec<u8> {
    let mut p = Vec::new();
    p.extend_from_slice(b"CrAU");
    p.extend_from_slice(&2u64.to_be_bytes());
    p.extend_from_slice(&(manifest.len() as u64).to_be_bytes());
    p.extend_from_slice(&0u32.to_be_bytes());
    p.extend_from_slice(manifest);
    p.extend_from_slice(blobs);
    p
}

struct Mem {
    data: Vec<u8>,
    writes: usize,
    reads: Cell<usize>,
}

impl Mem {
    fn new() -> Mem {
        Mem {
            data: vec![0xee; BS * BLOCKS],
            writes: 0,
            reads: Cell::new(0),
        }
    }
}

impl Sink for Mem {
    fn capacity(&self) -> u64 {
        return self.data.len() as u64;
    }

    fn write_at(&mut self, offset: u64, data: &[u8]) -> Result<(), PayloadError> {
        check_range(offset, data.len(), self.capacity())?;
        self.writes += 1;
        let off = offset as usize;
        self.data[off..(off + data.len())].copy_from_slice(data);
        Ok(())
    }

    fn flush(&mut self) -> Result<(), PayloadError> {
        Ok(())
    }
}

impl Readable for Mem {
    fn read_at(&self, offset: u64, out: &mut [u8]) -> Result<(), PayloadError> {
        check_range(offset, out.len(), self.capacity())?;
        self.reads.set(self.reads.get() + 1);
        let off = offset as usize;
        out.copy_from_slice(&self.data[off..(off + out.len())]);
        Ok(())
    }
}

struct Bytes {
    data: Vec<u8>,
    pos: usize,
}

impl Source for Bytes {
    fn read(&mut self, out: &mut [u8]) -> Result<usize, PayloadError> {
        let n = out.len().min(self.data.len() - self.pos);
        out[..n].copy_from_slice(&self.data[self.pos..self.pos + n]);
        self.pos += n;
        Ok(n)
    }
}

fn apply_unverified(bytes: &[u8], dev: &mut Mem) -> Result<(), PayloadError> {
    let prepared = delta_update_client::apply::prepare(bytes, Trust::Unverified, &EMBEDDED)?;
    return prepared
        .apply_to_device(
            dev,
            &mut MemoryStore::new(),
            &TARGET,
            Policy::Development,
            &NoProgress {},
        )
        .map(|_| ());
}

fn apply_stream_unverified(src: &mut Bytes, dev: &mut Mem) -> Result<(), PayloadError> {
    let whole = src.data.clone();
    let prepared = delta_update_client::stream::prepare(&mut *src, Trust::Unverified, &EMBEDDED)?;
    let open = |offset: u64| Bytes {
        data: whole.clone(),
        pos: offset as usize,
    };
    return prepared
        .apply(
            open,
            dev,
            &mut MemoryStore::new(),
            &TARGET,
            Policy::Development,
            &NoProgress {},
        )
        .map(|_| ());
}

fn first_replace() -> Vec<u8> {
    operation(Some(0), 0, BS as u64, &[(0, 1)], Some(&digest(&blob(1))))
}

fn blob(fill: u8) -> Vec<u8> {
    vec![fill; BS]
}

fn apply_both_ways(
    bytes: &[u8],
) -> (
    Result<(), PayloadError>,
    usize,
    Result<(), PayloadError>,
    usize,
) {
    let mut dev = Mem::new();
    let buffered = apply_unverified(bytes, &mut dev);
    let buffered_writes = dev.writes;

    let mut dev = Mem::new();
    let mut src = Bytes {
        data: bytes.to_vec(),
        pos: 0,
    };
    let streamed = apply_stream_unverified(&mut src, &mut dev);
    (buffered, buffered_writes, streamed, dev.writes)
}

#[test]
fn late_unsupported_operation_writes_nothing() {
    let ops = [
        first_replace(),
        operation(Some(8), BS as u64, BS as u64, &[(1, 1)], None),
    ];
    let bytes = payload(&manifest(&ops), &[blob(1), blob(2)].concat());
    let (buffered, buffered_writes, streamed, streamed_writes) = apply_both_ways(&bytes);
    assert!(buffered.is_err(), "buffered path must reject the operation");
    assert_eq!(buffered_writes, 0, "buffered path wrote before rejecting");
    assert!(
        streamed.is_err(),
        "streaming path must reject the operation"
    );
    assert_eq!(streamed_writes, 0, "streaming path wrote before rejecting");
}

#[test]
fn later_overlapping_operation_wins_on_both_pipelines() {
    let ops = [
        first_replace(),
        operation(
            Some(0),
            BS as u64,
            BS as u64,
            &[(0, 1)],
            Some(&digest(&blob(2))),
        ),
    ];
    let bytes = payload(&manifest(&ops), &[blob(1), blob(2)].concat());
    let mut buffered = Mem::new();
    apply_unverified(&bytes, &mut buffered).unwrap();
    assert_eq!(&buffered.data[..BS], &blob(2)[..]);
    assert_eq!(&buffered.data[BS..], &vec![0xee; BS * (BLOCKS - 1)][..]);
    let mut streamed = Mem::new();
    let mut src = Bytes {
        data: bytes.clone(),
        pos: 0,
    };
    apply_stream_unverified(&mut src, &mut streamed).unwrap();
    assert_eq!(streamed.data, buffered.data);
}

#[test]
fn overflowing_extents_are_errors_not_panics() {
    let cases = [
        operation(Some(0), 0, BS as u64, &[(u64::MAX - 1, 2)], None),
        operation(Some(0), 0, BS as u64, &[(0, u64::MAX)], None),
    ];
    for op in cases {
        let bytes = payload(&manifest(&[op]), &blob(3));
        let mut dev = Mem::new();
        let result = apply_unverified(&bytes, &mut dev);
        assert!(result.is_err());
        assert_eq!(dev.writes, 0);
    }
}

#[test]
fn missing_operation_type_is_rejected() {
    let op = operation(None, 0, BS as u64, &[(0, 1)], None);
    let bytes = payload(&manifest(&[op]), &blob(4));
    let (buffered, buffered_writes, _, _) = apply_both_ways(&bytes);
    assert!(
        buffered.is_err(),
        "missing type must not default to REPLACE"
    );
    assert_eq!(buffered_writes, 0);
}

#[test]
fn explicit_replace_type_zero_is_accepted() {
    let op = operation(Some(0), 0, BS as u64, &[(0, 1)], Some(&digest(&blob(5))));
    let bytes = payload(&manifest(&[op]), &blob(5));
    let (buffered, buffered_writes, streamed, _) = apply_both_ways(&bytes);
    assert!(buffered.is_ok());
    assert_eq!(buffered_writes, 1);
    assert!(streamed.is_ok());
}

#[test]
fn wrong_length_data_hash_is_rejected() {
    for len in [1usize, 31, 33] {
        let hash = vec![0u8; len];
        let op = operation(Some(0), 0, BS as u64, &[(0, 1)], Some(&hash));
        let bytes = payload(&manifest(&[op]), &blob(6));
        let mut dev = Mem::new();
        let result = apply_unverified(&bytes, &mut dev);
        assert!(result.is_err(), "hash of {} bytes was accepted", len);
        assert_eq!(dev.writes, 0);
    }
}

fn code_of<T>(result: Result<T, PayloadError>) -> Code {
    match result {
        Ok(_) => panic!("expected a refusal"),
        Err(e) => return e.code,
    }
}

#[test]
fn zero_operation_hash_must_be_absent_empty_or_the_empty_blob_digest() {
    let empty = sha256::hash_bytes(&[]);
    for hash in [None, Some(&[][..]), Some(&empty[..])] {
        let op = operation(Some(6), 0, 0, &[(0, 1)], hash);
        let bytes = payload(&manifest(&[op]), &[]);
        let (buffered, buffered_writes, streamed, streamed_writes) = apply_both_ways(&bytes);
        assert!(
            buffered.is_ok(),
            "buffered path refused an allowed ZERO hash"
        );
        assert_eq!(buffered_writes, 1);
        assert!(
            streamed.is_ok(),
            "streaming path refused an allowed ZERO hash"
        );
        assert_eq!(streamed_writes, 1);
    }
    let other = sha256::hash_bytes(b"not the empty blob");
    for wrong in [&[0u8; 32][..], &other[..]] {
        let op = operation(Some(6), 0, 0, &[(0, 1)], Some(wrong));
        let bytes = payload(&manifest(&[op]), &[]);
        let (buffered, buffered_writes, streamed, streamed_writes) = apply_both_ways(&bytes);
        assert_eq!(code_of(buffered), Code::blob_mismatch);
        assert_eq!(buffered_writes, 0);
        assert_eq!(code_of(streamed), Code::blob_mismatch);
        assert_eq!(streamed_writes, 0);
    }
}

fn signed_payload() -> Vec<u8> {
    let old = vec![0x11u8; BS * BLOCKS];
    let mut new = old.clone();
    new[BS..2 * BS].fill(0x22);
    return match build_payload_with(
        &old,
        &new,
        b"A/B",
        PRIVATE,
        BS,
        &EMBEDDED,
        &SearchConfig::quick(),
    ) {
        Err(e) => panic!("generator refused the fixture: {}", e.msg),
        Ok(v) => v,
    };
}

fn trusted() -> Vec<PubKey> {
    let key = match parse_pubkey_pem(PUBLIC) {
        Err(e) => panic!("cannot parse the fixture key: {}", e.msg),
        Ok(k) => k,
    };
    return vec![key];
}

#[test]
fn trailing_byte_after_a_signed_payload_is_refused_with_or_without_authentication() {
    let keys = trusted();
    let signed = signed_payload();
    let mut trailing = signed.clone();
    trailing.push(0);
    for trust in [Trust::Verified(&keys), Trust::Unverified] {
        let mut dev = Mem::new();
        dev.data = vec![0x11u8; BS * BLOCKS];
        let applied =
            delta_update_client::apply::prepare(&signed, trust, &EMBEDDED).and_then(|p| {
                p.apply_to_device(
                    &mut dev,
                    &mut MemoryStore::new(),
                    &TARGET,
                    Policy::Development,
                    &NoProgress {},
                )
            });
        assert!(applied.is_ok(), "fixture refused");

        let mut dev = Mem::new();
        let refused =
            delta_update_client::apply::prepare(&trailing, trust, &EMBEDDED).and_then(|p| {
                p.apply_to_device(
                    &mut dev,
                    &mut MemoryStore::new(),
                    &TARGET,
                    Policy::Development,
                    &NoProgress {},
                )
            });
        assert_eq!(code_of(refused), Code::parse);
        assert_eq!((dev.reads.get(), dev.writes), (0, 0));
    }
}

#[test]
fn streamed_trailing_byte_is_refused_only_at_end_of_stream() {
    let keys = trusted();
    let mut trailing = signed_payload();
    trailing.push(0);
    for trust in [Trust::Verified(&keys), Trust::Unverified] {
        let mut dev = Mem::new();
        dev.data = vec![0x11u8; BS * BLOCKS];
        let mut src = Bytes {
            data: trailing.clone(),
            pos: 0,
        };
        let prepared = delta_update_client::stream::prepare(&mut src, trust, &EMBEDDED);

        let result = prepared.and_then(|p| {
            let cursor = &mut src;
            let open = move |offset: u64| {
                cursor.pos = offset as usize;
                cursor
            };
            p.apply(
                open,
                &mut dev,
                &mut MemoryStore::new(),
                &TARGET,
                Policy::Development,
                &NoProgress {},
            )
        });
        assert_eq!(code_of(result), Code::parse);

        assert!(
            dev.writes > 0,
            "the stream should have written before the trailer"
        );
        assert_eq!(src.pos, trailing.len());
    }
}

fn signed_manifest(ops: &[Vec<u8>], sig_offset: u64, sig_size: u64) -> Vec<u8> {
    let mut m = manifest(ops);
    put_varint(&mut m, 4, sig_offset);
    put_varint(&mut m, 5, sig_size);
    return m;
}

#[test]
fn oversized_metadata_signature_declaration_is_refused_on_both_pipelines() {
    let mut bytes = payload(&manifest(&[first_replace()]), &blob(7));
    let declared = (EMBEDDED.max_signature_message + 1) as u32;
    bytes[20..24].copy_from_slice(&declared.to_be_bytes());
    let (buffered, buffered_writes, streamed, streamed_writes) = apply_both_ways(&bytes);
    assert_eq!(code_of(buffered), Code::resource_limit);
    assert_eq!(buffered_writes, 0);
    assert_eq!(code_of(streamed), Code::resource_limit);
    assert_eq!(streamed_writes, 0);
}

#[test]
fn oversized_payload_signature_declaration_is_refused_on_both_pipelines() {
    let oversize = EMBEDDED.max_signature_message + 1;
    let ops = [first_replace()];
    let manifest = signed_manifest(&ops, BS as u64, oversize as u64);
    let mut bytes = payload(&manifest, &blob(7));
    bytes.extend(vec![0u8; oversize]);
    let (buffered, buffered_writes, streamed, streamed_writes) = apply_both_ways(&bytes);
    assert_eq!(code_of(buffered), Code::resource_limit);
    assert_eq!(buffered_writes, 0);
    assert_eq!(code_of(streamed), Code::resource_limit);
    assert_eq!(streamed_writes, 0);
}
