mod support;

use delta_update_generator::SearchConfig;
use delta_update_generator::Stop;
use delta_update_generator::build_payload_with;
use delta_update_generator::builder;
use delta_update_generator::check::check_payload;
use delta_update_generator::control::Clock;
use delta_update_generator::control::Control;
use delta_update_generator::control::Halt;
use delta_update_generator::evaluate::Problem;
use delta_update_generator::evaluate::evaluate;
use delta_update_generator::mapper::Mapping;
use delta_update_generator::memory::Images;
use delta_update_generator::program::Kind;
use delta_update_generator::program::Op;
use delta_update_generator::program::Span;
use delta_update_generator::program::raw_baseline;
use delta_update_generator::seal;
use delta_update_generator::search::run_with;
use delta_update_payload::error::Code;
use delta_update_payload::error::PayloadError;
use delta_update_payload::layout;
use delta_update_payload::manifest::Extent;
use delta_update_payload::manifest::Manifest;
use delta_update_payload::manifest::Operation;
use delta_update_payload::manifest::PartitionUpdate;
use delta_update_payload::manifest::TYPE_REPLACE_SPARSE;
use delta_update_payload::manifest::TYPE_SOURCECOPY;
use delta_update_payload::manifest::TYPE_TARGET_COPY;
use delta_update_payload::manifest::TYPE_ZERO;
use delta_update_payload::profile::EMBEDDED;
use delta_update_payload::profile::HOST;
use delta_update_payload::profile::ResourceProfile;
use delta_update_payload::rsa::PubKey;
use delta_update_payload::rsa::parse_privkey_pem;
use delta_update_payload::rsa::parse_pubkey_pem;
use delta_update_payload::sha256;
use delta_update_payload::sign;
use delta_update_payload::verify::verify_payload;
use prost::Message;
use std::cell::Cell;
use std::time::Instant;

const PRIV: &[u8] = include_bytes!("../../delta-update-payload/tests/fixtures/private.pem");
const PRIV_4096: &[u8] =
    include_bytes!("../../delta-update-payload/tests/fixtures/private4096.pem");
const PUB: &[u8] = include_bytes!("../../delta-update-payload/tests/fixtures/public.pem");

const CHUNK: u32 = 64;

fn success<T>(result: Result<T, PayloadError>) -> T {
    return result.unwrap_or_else(|e| panic!("{}", e.msg));
}

fn done<T>(result: Result<T, Halt>) -> T {
    return match result {
        Ok(v) => v,
        Err(Halt::Failed(e)) => panic!("{}", e.msg),
        Err(Halt::Deadline) => panic!("an unbounded step cannot time out"),
    };
}

fn public_key() -> PubKey {
    return success(parse_pubkey_pem(PUB));
}

fn problem_for<'a>(old: &'a [u8], new: &'a [u8], chunk: usize) -> Problem<'a> {
    return success(Problem::new(old, new, chunk, "A/B", &EMBEDDED, 256));
}

fn noise(len: usize, seed: u64) -> Vec<u8> {
    let mut x = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    let mut out = Vec::with_capacity(len);
    for _ in 0..len {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        out.push((x >> 24) as u8);
    }
    return out;
}

fn by_generations(seed: u64, gens: u64) -> SearchConfig {
    return SearchConfig {
        release_version: 0,
        max_search_seconds: 600.0,
        stagnation_seconds: 3600.0,
        min_improvement_bytes: 1,
        seed: seed,
        max_generations: Some(gens),
        device: support::device(),
        readable_target: true,
    };
}

fn span(start: u64, len: u64) -> Span {
    return Span {
        start: start,
        len: len,
    };
}

fn complement(spans: &[(u64, u64)], len: u64) -> Vec<(u64, u64)> {
    let mut sorted = spans.to_vec();
    sorted.sort();
    let mut out = Vec::new();
    let mut at = 0u64;
    for (start, n) in sorted {
        if start > at {
            out.push((at, start - at));
        }
        at = start + n;
    }
    if at < len {
        out.push((at, len - at));
    }
    return out;
}

fn gather(spans: &[(u64, u64)], image: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    for (start, n) in spans {
        out.extend_from_slice(&image[*start as usize..(*start + *n) as usize]);
    }
    return out;
}

#[test]
fn a_copy_of_63_byte_segments_is_reachable_by_the_search() {
    let len = 4096u64;
    let old = noise(len as usize, 41);
    let mut new = old.clone();
    let mut copies: Vec<((u64, u64), (u64, u64))> = Vec::new();
    for i in 0..20u64 {
        let dst = (i * 200 + 10, 63u64);
        let src = (i * 97 + 7, 63u64);
        new[dst.0 as usize..(dst.0 + 63) as usize]
            .copy_from_slice(&old[src.0 as usize..(src.0 + 63) as usize]);
        copies.push((dst, src));
    }
    let dst_spans: Vec<(u64, u64)> = copies.iter().map(|(d, _)| *d).collect();
    let src_spans: Vec<(u64, u64)> = copies.iter().map(|(_, s)| *s).collect();
    let mut copy = builder::Op::new(builder::Kind::SourceCopy, dst_spans.clone());
    copy.src = src_spans.clone();
    copy.src_hash = sha256::hash_bytes(&gather(&src_spans, &old));
    let rest = complement(&dst_spans, len);
    let mut sparse = builder::Op::new(builder::Kind::Sparse, rest.clone());
    sparse.data = gather(&rest, &new);
    sparse.keep = vec![true; (sparse.data.len() + 63) / 64];
    let reference = success(builder::build(&builder::Spec {
        chunk: 64,
        partition: len,
        old_hash: None,
        ops: vec![copy, sparse],
        sign: Some(PRIV),
    }));
    success(check_payload(
        &reference,
        public_key(),
        &old,
        &new,
        &EMBEDDED,
    ));

    let payload = success(build_payload_with(
        &old,
        &new,
        b"A/B",
        PRIV,
        64,
        &EMBEDDED,
        &by_generations(7, 150),
    ));
    let parsed = success(layout::parse_unverified(&payload, &EMBEDDED)).into_parsed();
    let short_copy = parsed.manifest.partitions[0].operations.iter().any(|op| {
        op.r#type == TYPE_SOURCECOPY
            && op
                .src_extents
                .iter()
                .any(|e| matches!(e.num_bytes, Some(n) if n < 64))
    });
    assert!(
        short_copy,
        "the search payload has no source copy shorter than a probe"
    );
    let mut raw_replace = builder::Op::new(builder::Kind::Replace, vec![(0, len)]);
    raw_replace.data = new.clone();
    let raw = success(builder::build(&builder::Spec {
        chunk: 64,
        partition: len,
        old_hash: None,
        ops: vec![raw_replace],
        sign: Some(PRIV),
    }));
    assert!(
        payload.len() < raw.len(),
        "search payload is {} bytes; the raw plan is {} bytes",
        payload.len(),
        raw.len()
    );
}

#[test]
fn a_sparse_operation_that_keeps_every_chunk_is_admitted() {
    let old: Vec<u8> = (0..256u32).map(|k| (k * 13 % 251) as u8).collect();
    let new = old.clone();
    let problem = problem_for(&old, &new, 64);
    let program = vec![Op::new(Kind::Sparse, vec![span(0, 256)])];
    let found = done(evaluate(&problem, program, &Control::unbounded()));
    assert!(
        found.is_some(),
        "a sparse operation that keeps every chunk must be admissible"
    );
}

#[test]
fn a_signing_key_the_profile_cannot_admit_is_refused_before_search() {
    let old = noise(512, 7);
    let mut new = old.clone();
    new[100..200].fill(9);
    let profile = delta_update_payload::profile::ResourceProfile {
        max_signature_data: 256,
        ..EMBEDDED
    };
    let started = Instant::now();
    let refused = build_payload_with(
        &old,
        &new,
        b"A/B",
        PRIV_4096,
        64,
        &profile,
        &by_generations(1, 1000),
    );
    assert_eq!(refused.err().map(|e| e.code), Some(Code::resource_limit));
    assert!(
        started.elapsed().as_secs_f64() < 10.0,
        "the refusal came after a search"
    );
}

#[test]
fn a_frame_cache_never_serves_another_images_bytes() {
    let program = vec![Op {
        kind: Kind::Frames,
        dst: vec![span(0, 512)],
        src: Vec::new(),
        level: Some(6),
    }];
    let old = vec![0u8; 512];
    let new_a = vec![0xAAu8; 512];
    let new_b = vec![0x55u8; 512];
    let images_a = Images {
        old: &old,
        new: &new_a,
    };
    let images_b = Images {
        old: &old,
        new: &new_b,
    };
    let _first = done(Mapping::new(images_a, 64).map(&program, &Control::unbounded(), true));
    let mapped = done(Mapping::new(images_b, 64).map(&program, &Control::unbounded(), true));
    let blobs = mapped.blobs.unwrap_or_default();
    let table = mapped.ops[0].frame_table.clone().unwrap_or_default();
    let mut entries = delta_update_payload::encoding::Varints::new(&table);
    let mut at = 0usize;
    let mut decoded: Vec<u8> = Vec::new();
    for _ in 0..8 {
        let n = success(entries.next_entry()) as usize;
        decoded.extend(
            miniz_oxide::inflate::decompress_to_vec_zlib(&blobs[at..at + n]).unwrap_or_default(),
        );
        at += n;
    }
    assert_eq!(
        decoded, new_b,
        "the second image's blob does not decode to its own bytes"
    );
}

#[test]
fn the_checker_refuses_an_old_image_that_does_not_match_the_manifest() {
    let old_a = noise(512, 8);
    let mut new = old_a.clone();
    new[10..90].fill(3);
    let old_b: Vec<u8> = old_a.iter().map(|b| b.wrapping_add(1)).collect();
    let mut replace = builder::Op::new(builder::Kind::Replace, vec![(0, 512)]);
    replace.data = new.clone();
    let payload = success(builder::build(&builder::Spec {
        chunk: 64,
        partition: 512,
        old_hash: Some(sha256::hash_bytes(&old_a)),
        ops: vec![replace],
        sign: Some(PRIV),
    }));
    success(check_payload(
        &payload,
        public_key(),
        &old_a,
        &new,
        &EMBEDDED,
    ));
    let refused = check_payload(&payload, public_key(), &old_b, &new, &EMBEDDED);
    assert_eq!(refused.err().map(|e| e.code), Some(Code::hash_mismatch));
}

#[test]
fn the_checker_refuses_a_partition_size_that_differs_from_the_images() {
    let old = noise(512, 9);
    let mut new = old.clone();
    new[0..40].fill(4);
    let mut replace = builder::Op::new(builder::Kind::Replace, vec![(0, 512)]);
    replace.data = new.clone();
    let payload = success(builder::build(&builder::Spec {
        chunk: 64,
        partition: 513,
        old_hash: None,
        ops: vec![replace],
        sign: Some(PRIV),
    }));
    let refused = check_payload(&payload, public_key(), &old, &new, &EMBEDDED);
    assert_eq!(
        refused.err().map(|e| e.code),
        Some(Code::partition_mismatch)
    );
}

struct Reads(Cell<u64>);

impl Clock for Reads {
    fn seconds(&self) -> f64 {
        self.0.set(self.0.get() + 1);
        return 0.0;
    }
}

#[test]
fn a_zero_budget_returns_the_raw_seed_with_a_bounded_number_of_clock_reads() {
    let old = noise(1 << 18, 30);
    let mut new = old.clone();
    new[1000..2000].fill(7);
    let problem = problem_for(&old, &new, 1);
    let zero = SearchConfig {
        release_version: 0,
        max_search_seconds: 0.0,
        ..by_generations(1, 10)
    };
    let reads = Reads(Cell::new(0));
    let found = match run_with(&problem, &zero, &reads, None) {
        Err(e) => panic!("{}", e.msg),
        Ok(f) => f,
    };
    assert_eq!(found.stop, Stop::Deadline);
    assert_eq!(found.generations, 0);
    assert!(
        found.finalists.is_empty() && found.device.is_none(),
        "a zero budget opened no device"
    );
    assert!(
        reads.0.get() <= 3,
        "a zero budget read the clock {} times",
        reads.0.get()
    );
    let payload = success(build_payload_with(
        &old, &new, b"A/B", PRIV, 1, &HOST, &zero,
    ));
    let parsed = success(layout::parse_unverified(&payload, &HOST)).into_parsed();
    assert_eq!(
        parsed.manifest.partitions[0].operations.len(),
        1,
        "the seed is one operation"
    );
}

#[test]
fn a_zero_budget_does_not_compare_identical_images_to_find_them_unchanged() {
    let old = noise(4096, 31);
    let problem = problem_for(&old, &old, 64);
    let zero = SearchConfig {
        release_version: 0,
        max_search_seconds: 0.0,
        ..by_generations(1, 10)
    };
    let found = match run_with(&problem, &zero, &Reads(Cell::new(0)), None) {
        Err(e) => panic!("{}", e.msg),
        Ok(f) => f,
    };
    assert_eq!(found.stop, Stop::Deadline);
}

#[test]
fn a_profile_with_no_admissible_candidate_says_so_within_the_budget() {
    let old = noise(512, 32);
    let mut new = old.clone();
    new[0..8].fill(1);
    let profile = ResourceProfile {
        max_encoded_manifest: 1,
        ..EMBEDDED
    };
    for seconds in [0.0, 2.0] {
        let config = SearchConfig {
            release_version: 0,
            max_search_seconds: seconds,
            ..by_generations(1, 1000)
        };
        let refused = build_payload_with(&old, &new, b"A/B", PRIV, 64, &profile, &config);
        assert_eq!(
            refused.err().map(|e| e.msg),
            Some("no admissible candidate found within the budget"),
            "budget {} s",
            seconds
        );
    }
}

#[test]
fn a_profile_that_admits_zero_but_not_raw_replacement_still_builds() {
    let old = noise(64, 33);
    let new = vec![0u8; 64];
    let profile = ResourceProfile {
        max_encoded_manifest: 109,
        ..EMBEDDED
    };
    let payload = success(build_payload_with(
        &old,
        &new,
        b"A/B",
        PRIV,
        64,
        &profile,
        &by_generations(1, 40),
    ));
    let frame = success(layout::frame(&payload, &profile));
    assert_eq!(
        frame.manifest.len(),
        109,
        "the ZERO manifest is the 109-byte case"
    );
    let parsed = success(layout::parse_unverified(&payload, &profile)).into_parsed();
    assert!(
        parsed.manifest.partitions[0]
            .operations
            .iter()
            .all(|op| op.r#type == TYPE_ZERO)
    );
    success(check_payload(&payload, public_key(), &old, &new, &profile));
}

struct Ticks {
    reads: Cell<u64>,
    step: f64,
}

impl Clock for Ticks {
    fn seconds(&self) -> f64 {
        let r = self.reads.get() + 1;
        self.reads.set(r);
        return r as f64 * self.step;
    }
}

fn one_byte_update() -> (Vec<u8>, Vec<u8>, ResourceProfile) {
    let old = noise(8192, 50);
    let mut new = old.clone();
    new[17] ^= 0xa5;
    let tight = ResourceProfile {
        max_operations: 1,
        max_encoded_manifest: 146,
        ..EMBEDDED
    };
    return (old, new, tight);
}

#[test]
fn a_one_byte_update_builds_under_a_profile_that_refuses_the_raw_replacement() {
    let (old, new, tight) = one_byte_update();
    let problem = success(Problem::new(&old, &new, 64, "A/B", &tight, 256));
    let unbounded = Control::unbounded();
    assert!(
        done(evaluate(&problem, raw_baseline(8192), &unbounded)).is_none(),
        "the raw replacement must not fit"
    );
    let one_byte = vec![Op::new(Kind::Replace, vec![span(17, 1)])];
    assert!(
        done(evaluate(&problem, one_byte, &unbounded)).is_some(),
        "the changed byte must fit"
    );

    let config = by_generations(1, 40);
    let built = success(delta_update_generator::build_report(
        &old, &new, b"A/B", PRIV, 64, &tight, &config,
    ));
    success(check_payload(
        &built.payload,
        public_key(),
        &old,
        &new,
        &tight,
    ));
    success(verify_payload(&built.payload, &[public_key()], &tight));
    let raw = built.raw_admitted.unwrap_or(0);
    assert!(
        built.payload.len() as u64 <= raw,
        "the payload is {} bytes against a raw {}",
        built.payload.len(),
        raw
    );
}

#[test]
fn a_deadline_inside_the_search_returns_only_programs_the_profile_admits() {
    let (old, new, tight) = one_byte_update();
    let problem = success(Problem::new(&old, &new, 64, "A/B", &tight, 256));
    for k in 0..12u64 {
        let clock = Ticks {
            reads: Cell::new(0),
            step: 1.0,
        };
        let config = SearchConfig {
            release_version: 0,
            max_search_seconds: k as f64 + 0.5,
            ..by_generations(1, 40)
        };
        let found = success(run_with(&problem, &config, &clock, Some(1)));
        for f in &found.finalists {
            let again = done(evaluate(&problem, f.program.clone(), &Control::unbounded()));
            assert_eq!(
                again.map(|e| e.size),
                Some(f.raw_size),
                "deadline at read {}",
                k + 1
            );
        }
    }
}

#[test]
fn a_zero_budget_does_not_scan_for_the_one_byte_repair() {
    let (old, new, tight) = one_byte_update();
    let problem = success(Problem::new(&old, &new, 64, "A/B", &tight, 256));
    let zero = SearchConfig {
        release_version: 0,
        max_search_seconds: 0.0,
        ..by_generations(1, 40)
    };
    let reads = Reads(Cell::new(0));
    let found = success(run_with(&problem, &zero, &reads, None));
    assert!(found.finalists.is_empty() && found.device.is_none());
    assert!(
        reads.0.get() <= 3,
        "a zero budget read the clock {} times",
        reads.0.get()
    );
    let refused = build_payload_with(&old, &new, b"A/B", PRIV, 64, &tight, &zero);
    assert_eq!(
        refused.err().map(|e| e.msg),
        Some("no admissible candidate found within the budget")
    );
}

#[test]
fn a_profile_without_signature_slots_is_refused_before_search() {
    let old = noise(512, 34);
    let mut new = old.clone();
    new[100..200].fill(9);
    let profile = ResourceProfile {
        max_signatures: 0,
        ..EMBEDDED
    };
    let config = SearchConfig {
        release_version: 0,
        max_search_seconds: 4.0,
        stagnation_seconds: 3600.0,
        max_generations: None,
        ..by_generations(1, 1000)
    };
    let started = Instant::now();
    let refused = build_payload_with(&old, &new, b"A/B", PRIV, 64, &profile, &config);
    assert_eq!(refused.err().map(|e| e.code), Some(Code::resource_limit));
    assert!(
        started.elapsed().as_secs_f64() < 2.0,
        "the refusal came after the search"
    );
}

fn unsized_payload(ops: Vec<Operation>, blobs: &[u8]) -> Vec<u8> {
    let key = success(parse_privkey_pem(PRIV));
    let sig_size = sign::signature_message_size(key.size);
    let manifest = Manifest {
        block_size: Some(CHUNK),
        signatures_offset: Some(blobs.len() as u64),
        signatures_size: Some(sig_size as u64),
        minor_version: Some(layout::PROJECT_MINOR),
        partitions: vec![PartitionUpdate {
            partition_name: "A/B".to_string(),
            operations: ops,
            ..Default::default()
        }],
        ..Default::default()
    };
    return success(seal(&manifest.encode_to_vec(), blobs, &key, sig_size));
}

fn extent(start: u64, len: u64) -> Extent {
    return Extent {
        start_byte: Some(start),
        num_bytes: Some(len),
    };
}

fn operation(kind: i32, dst: Vec<Extent>, src: Vec<Extent>) -> Operation {
    return Operation {
        r#type: kind,
        data_offset: Some(0),
        data_length: Some(0),
        dst_extents: dst,
        src_extents: src,
        ..Default::default()
    };
}

#[test]
fn the_checker_refuses_a_zero_beyond_an_image_whose_size_is_not_recorded() {
    let old = noise(512, 21);
    let payload = unsized_payload(
        vec![operation(TYPE_ZERO, vec![extent(508, 8)], vec![])],
        &[],
    );
    let refused = check_payload(&payload, public_key(), &old, &old, &EMBEDDED);
    assert_eq!(
        refused.err().map(|e| e.code),
        Some(Code::extent_out_of_range)
    );
}

#[test]
fn the_checker_refuses_a_kept_sparse_range_beyond_an_image_whose_size_is_not_recorded() {
    let old = noise(512, 22);
    let mut sparse = operation(TYPE_REPLACE_SPARSE, vec![extent(508, 8)], vec![]);
    sparse.frame_table = Some(vec![0x00]);
    sparse.data_sha256_hash = Some(sha256::hash_bytes(&[]).to_vec());
    let payload = unsized_payload(vec![sparse], &[]);
    let refused = check_payload(&payload, public_key(), &old, &old, &EMBEDDED);
    assert_eq!(
        refused.err().map(|e| e.code),
        Some(Code::extent_out_of_range)
    );
}

#[test]
fn the_checker_refuses_a_source_copy_beyond_an_old_image_whose_size_is_not_recorded() {
    let old = noise(512, 23);
    let mut copy = operation(TYPE_SOURCECOPY, vec![extent(0, 8)], vec![extent(508, 8)]);
    copy.src_sha256_hash = Some(vec![0u8; sha256::SHA256_SIZE]);
    let payload = unsized_payload(vec![copy], &[]);
    let refused = check_payload(&payload, public_key(), &old, &old, &EMBEDDED);
    assert_eq!(
        refused.err().map(|e| e.code),
        Some(Code::extent_out_of_range)
    );
}

#[test]
fn the_checker_refuses_a_target_copy_beyond_the_target_whose_size_is_not_recorded() {
    let old = noise(512, 26);
    let mut copy = operation(TYPE_TARGET_COPY, vec![extent(0, 8)], vec![extent(508, 8)]);
    copy.src_sha256_hash = Some(vec![0u8; sha256::SHA256_SIZE]);
    let payload = unsized_payload(vec![copy], &[]);
    let refused = check_payload(&payload, public_key(), &old, &old, &EMBEDDED);
    assert_eq!(
        refused.err().map(|e| e.code),
        Some(Code::extent_out_of_range)
    );
}

#[test]
fn a_payload_inside_its_images_is_accepted_without_recorded_sizes() {
    let old = noise(512, 24);
    let mut new = old.clone();
    new[0..8].fill(0);
    new[200..208].copy_from_slice(&old[100..108]);
    let zero = operation(TYPE_ZERO, vec![extent(0, 8)], vec![]);
    let mut copy = operation(TYPE_SOURCECOPY, vec![extent(200, 8)], vec![extent(100, 8)]);
    copy.src_sha256_hash = Some(sha256::hash_bytes(&old[100..108]).to_vec());
    let payload = unsized_payload(vec![zero, copy], &[]);
    success(check_payload(&payload, public_key(), &old, &new, &EMBEDDED));
}

#[test]
fn the_checker_refuses_unwritten_growth_without_recorded_sizes() {
    let old = noise(512, 25);
    let mut new = old.clone();
    new.resize(520, 0);
    new[..8].fill(0);
    let payload = unsized_payload(vec![operation(TYPE_ZERO, vec![extent(0, 8)], vec![])], &[]);
    let refused = check_payload(&payload, public_key(), &old, &new, &EMBEDDED);
    assert_eq!(refused.err().map(|e| e.code), Some(Code::blob_mismatch));
}
