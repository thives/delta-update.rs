use delta_update_generator::build_full_payload;
use delta_update_generator::check::check_full_payload;
use delta_update_generator::seal;
use delta_update_payload::error::Code;
use delta_update_payload::layout;
use delta_update_payload::manifest::Extent;
use delta_update_payload::manifest::Manifest;
use delta_update_payload::manifest::Operation;
use delta_update_payload::manifest::PartInfo;
use delta_update_payload::manifest::PartitionUpdate;
use delta_update_payload::manifest::TYPE_REPLACE;
use delta_update_payload::manifest::TYPE_REPLACE_FRAMES;
use delta_update_payload::manifest::TYPE_REPLACE_SPARSE;
use delta_update_payload::manifest::TYPE_SOURCECOPY;
use delta_update_payload::manifest::TYPE_ZERO;
use delta_update_payload::plan;
use delta_update_payload::profile::EMBEDDED;
use delta_update_payload::profile::HOST;
use delta_update_payload::profile::ResourceProfile;
use delta_update_payload::rsa;
use delta_update_payload::sha256;
use delta_update_payload::sign;
use delta_update_payload::verify::verify_payload;
use prost::Message;

const PRIV: &[u8] = include_bytes!("../../delta-update-payload/tests/fixtures/private.pem");
const PUB: &[u8] = include_bytes!("../../delta-update-payload/tests/fixtures/public.pem");
const PRIV_4096: &[u8] =
    include_bytes!("../../delta-update-payload/tests/fixtures/private4096.pem");

const BLOCK: usize = 4096;

fn public_key() -> rsa::PubKey {
    return rsa::parse_pubkey_pem(PUB).unwrap_or_else(|e| panic!("{}", e.msg));
}

fn build(new: &[u8], profile: &ResourceProfile) -> Vec<u8> {
    return build_full_payload(new, b"A/B", PRIV, BLOCK, profile)
        .unwrap_or_else(|e| panic!("{}", e.msg));
}

fn manifest_of(payload: &[u8], profile: &ResourceProfile) -> Manifest {
    let authenticated =
        verify_payload(payload, &[public_key()], profile).unwrap_or_else(|e| panic!("{}", e.msg));
    return authenticated.into_parsed().manifest;
}

fn ops_of(manifest: &Manifest) -> &[Operation] {
    return &manifest.partitions[0].operations;
}

fn types_of(manifest: &Manifest) -> Vec<i32> {
    return ops_of(manifest).iter().map(|op| op.r#type).collect();
}

fn assert_covers(manifest: &Manifest, len: u64) {
    let mut cursor: u64 = 0;
    for op in ops_of(manifest) {
        for ext in &op.dst_extents {
            assert_eq!(
                ext.start_byte,
                Some(cursor),
                "extent starts where the last one ended"
            );
            cursor += ext.num_bytes.unwrap_or(0);
        }
    }
    assert_eq!(cursor, len, "the destinations cover the partition exactly");
}

fn assert_round_trips(new: &[u8], profile: &ResourceProfile) -> Vec<u8> {
    let payload = build(new, profile);
    let manifest = manifest_of(&payload, profile);
    assert_covers(&manifest, new.len() as u64);
    check_full_payload(&payload, public_key(), new, profile)
        .unwrap_or_else(|e| panic!("{:?} {}", e.code, e.msg));
    return payload;
}

fn zeros(n: usize) -> Vec<u8> {
    return vec![0u8; n];
}

fn text(n: usize) -> Vec<u8> {
    return b"the quick brown fox jumps over the lazy dog. "
        .iter()
        .cycle()
        .take(n)
        .cloned()
        .collect();
}

fn noise(n: usize, seed: u64) -> Vec<u8> {
    let mut x = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    let mut out = Vec::with_capacity(n);
    for _ in 0..n {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        out.push((x >> 24) as u8);
    }
    return out;
}

fn alternating_blocks(n: usize) -> Vec<u8> {
    let mut out: Vec<u8> = Vec::new();
    for k in 0..n {
        if k % 2 == 0 {
            out.extend(zeros(BLOCK));
        } else {
            out.extend(text(BLOCK));
        }
    }
    return out;
}

#[test]
fn metadata_describes_the_new_image_and_nothing_else() {
    let new = text(3 * BLOCK + 17);
    let payload = build(&new, &EMBEDDED);
    let manifest = manifest_of(&payload, &EMBEDDED);
    let partition = &manifest.partitions[0];
    assert!(
        partition.old_partition_info.is_none(),
        "no old partition is recorded"
    );
    let info = partition.new_partition_info.clone().unwrap_or_default();
    assert_eq!(info.size, Some(new.len() as u64));
    assert_eq!(info.hash, Some(sha256::hash_bytes(&new).to_vec()));
    assert_eq!(manifest.minor_version, Some(layout::PROJECT_MINOR));
    for t in types_of(&manifest) {
        assert!(
            t == TYPE_REPLACE || t == TYPE_REPLACE_FRAMES || t == TYPE_ZERO,
            "operation type {} is not a full-image operation",
            t
        );
    }
    let validated = plan::validate(manifest, &EMBEDDED).unwrap_or_else(|e| panic!("{}", e.msg));
    assert!(
        !validated.geometry().needs_source,
        "a full-image payload needs no old image"
    );
    assert!(
        !validated.geometry().reads_target,
        "a full-image payload reads no target"
    );
}

#[test]
fn every_image_shape_is_covered_once_in_order_and_checked() {
    let mut mixed = zeros(2 * BLOCK);
    mixed.extend(text(3 * BLOCK));
    mixed.extend(noise(BLOCK, 5));
    mixed.extend(zeros(BLOCK + 9));
    let shapes: Vec<Vec<u8>> = vec![
        zeros(5 * BLOCK),
        text(5 * BLOCK),
        noise(5 * BLOCK, 1),
        mixed,
        noise(1, 2),
        noise(2, 3),
        noise(BLOCK - 1, 4),
        noise(BLOCK + 1, 6),
        text(7 * BLOCK + 123),
    ];
    for new in &shapes {
        assert_round_trips(new, &EMBEDDED);
    }
}

#[test]
fn an_empty_image_is_a_valid_empty_full_payload() {
    let payload = assert_round_trips(&[], &EMBEDDED);
    assert!(ops_of(&manifest_of(&payload, &EMBEDDED)).is_empty());
}

#[test]
fn the_same_image_gives_the_same_bytes() {
    let new = text(9 * BLOCK + 5);
    assert_eq!(build(&new, &EMBEDDED), build(&new, &EMBEDDED));
    let mut other = new.clone();
    other[100] ^= 1;
    assert_ne!(build(&other, &EMBEDDED), build(&new, &EMBEDDED));
}

#[test]
fn an_all_zero_image_is_one_zero_operation() {
    let new = zeros(64 * BLOCK);
    let payload = assert_round_trips(&new, &EMBEDDED);
    assert_eq!(types_of(&manifest_of(&payload, &EMBEDDED)), vec![TYPE_ZERO]);
    assert!(
        payload.len() < 1024,
        "an all-zero image carries no data: {} bytes",
        payload.len()
    );
}

#[test]
fn a_compressible_image_is_framed_and_smaller_than_its_raw_form() {
    let new = text(64 * BLOCK);
    let payload = assert_round_trips(&new, &EMBEDDED);
    let types = types_of(&manifest_of(&payload, &EMBEDDED));
    assert!(
        types.contains(&TYPE_REPLACE_FRAMES),
        "text compresses, so its chunks are framed"
    );
    assert!(
        !types.contains(&TYPE_REPLACE),
        "no raw replacement is needed"
    );
    assert!(
        payload.len() < new.len() / 4,
        "{} bytes for {} of text",
        payload.len(),
        new.len()
    );
}

#[test]
fn an_incompressible_image_is_written_as_one_raw_replacement() {
    let new = noise(64 * BLOCK, 7);
    let payload = assert_round_trips(&new, &EMBEDDED);
    assert_eq!(
        types_of(&manifest_of(&payload, &EMBEDDED)),
        vec![TYPE_REPLACE]
    );
    assert!(
        payload.len() < new.len() + 1024,
        "{} bytes for {}",
        payload.len(),
        new.len()
    );
}

#[test]
fn a_mixed_image_uses_zero_runs_and_frames_together() {
    let mut new = zeros(8 * BLOCK);
    new.extend(text(16 * BLOCK));
    new.extend(noise(8 * BLOCK, 9));
    new.extend(zeros(4 * BLOCK));
    let payload = assert_round_trips(&new, &EMBEDDED);
    let types = types_of(&manifest_of(&payload, &EMBEDDED));
    assert!(types.contains(&TYPE_ZERO), "{:?}", types);
    assert!(types.contains(&TYPE_REPLACE_FRAMES), "{:?}", types);
    assert!(payload.len() < new.len(), "the zero runs are not written");
}

#[test]
fn partial_final_chunks_round_trip_at_every_offset_of_a_chunk() {
    for len in [BLOCK + 1, 2 * BLOCK - 1, 3 * BLOCK + 17] {
        assert_round_trips(&text(len), &EMBEDDED);
        assert_round_trips(&noise(len, len as u64), &EMBEDDED);
    }
}

#[test]
fn a_small_raw_image_round_trips_under_a_large_block_size() {
    let new = noise(100, 11);
    let payload = build_full_payload(&new, b"A/B", PRIV, 1 << 20, &HOST)
        .unwrap_or_else(|e| panic!("{}", e.msg));
    let manifest = manifest_of(&payload, &HOST);
    assert_eq!(types_of(&manifest), vec![TYPE_REPLACE]);
    assert_covers(&manifest, new.len() as u64);
    check_full_payload(&payload, public_key(), &new, &HOST)
        .unwrap_or_else(|e| panic!("{:?} {}", e.code, e.msg));
}

#[test]
fn a_framed_run_shorter_than_one_chunk_round_trips() {
    let mut new = zeros(BLOCK);
    new.extend(text(BLOCK / 2 + 3));
    let payload = assert_round_trips(&new, &EMBEDDED);
    assert_eq!(
        types_of(&manifest_of(&payload, &EMBEDDED)),
        vec![TYPE_ZERO, TYPE_REPLACE_FRAMES]
    );
}

#[test]
fn frame_tables_are_split_at_the_profile_limit() {
    let profile = ResourceProfile {
        max_frame_table_bytes: 16,
        ..EMBEDDED
    };
    let new = text(64 * 512);
    let payload = build_full_payload(&new, b"A/B", PRIV, 512, &profile)
        .unwrap_or_else(|e| panic!("{}", e.msg));
    let manifest = manifest_of(&payload, &profile);
    assert_covers(&manifest, new.len() as u64);
    let framed: Vec<&Operation> = ops_of(&manifest)
        .iter()
        .filter(|op| op.r#type == TYPE_REPLACE_FRAMES)
        .collect();
    assert!(
        framed.len() >= 4,
        "64 chunks at 16 entries an operation need at least 4 operations"
    );
    for op in framed {
        assert!(op.frame_table.clone().unwrap_or_default().len() <= 16);
    }
    check_full_payload(&payload, public_key(), &new, &profile)
        .unwrap_or_else(|e| panic!("{}", e.msg));
}

#[test]
fn a_framed_form_the_profile_cannot_hold_falls_back_to_raw() {
    let new = alternating_blocks(80);
    let payload = assert_round_trips(&new, &EMBEDDED);
    assert_eq!(
        types_of(&manifest_of(&payload, &EMBEDDED)),
        vec![TYPE_REPLACE]
    );
    let host = assert_round_trips(&new, &HOST);
    assert!(types_of(&manifest_of(&host, &HOST)).len() > 1);
    assert!(host.len() < payload.len());
}

#[test]
fn alternating_bytes_at_block_size_one_fall_back_to_one_raw_replacement() {
    let new: Vec<u8> = (0..4096usize)
        .map(|k| if k % 2 == 0 { 0 } else { 0xa5 })
        .collect();
    let payload = build_full_payload(&new, b"A/B", PRIV, 1, &EMBEDDED)
        .unwrap_or_else(|e| panic!("{}", e.msg));
    let manifest = manifest_of(&payload, &EMBEDDED);
    assert_eq!(types_of(&manifest), vec![TYPE_REPLACE]);
    assert_covers(&manifest, new.len() as u64);
    check_full_payload(&payload, public_key(), &new, &EMBEDDED)
        .unwrap_or_else(|e| panic!("{:?} {}", e.code, e.msg));
}

fn assert_four_blocks_framed_and_five_raw(profile: &ResourceProfile) {
    let framed = assert_round_trips(&alternating_blocks(4), profile);
    let expected = vec![
        TYPE_ZERO,
        TYPE_REPLACE_FRAMES,
        TYPE_ZERO,
        TYPE_REPLACE_FRAMES,
    ];
    assert_eq!(types_of(&manifest_of(&framed, profile)), expected);
    let raw = assert_round_trips(&alternating_blocks(5), profile);
    assert_eq!(types_of(&manifest_of(&raw, profile)), vec![TYPE_REPLACE]);
}

#[test]
fn operation_and_extent_limits_alone_decide_between_the_framed_and_raw_forms() {
    assert_four_blocks_framed_and_five_raw(&ResourceProfile {
        max_operations: 4,
        ..EMBEDDED
    });
    assert_four_blocks_framed_and_five_raw(&ResourceProfile {
        max_total_extents: 4,
        ..EMBEDDED
    });
}

#[test]
fn a_per_operation_extent_limit_below_one_refuses_both_forms() {
    let profile = ResourceProfile {
        max_extents_per_op: 0,
        ..EMBEDDED
    };
    let refused = build_full_payload(&text(BLOCK), b"A/B", PRIV, BLOCK, &profile);
    assert_eq!(refused.err().map(|e| e.code), Some(Code::resource_limit));
}

#[test]
fn a_profile_that_admits_no_candidate_is_a_resource_error_and_never_a_switch() {
    let new = text(4 * BLOCK);
    let small_manifest = ResourceProfile {
        max_encoded_manifest: 16,
        ..EMBEDDED
    };
    let refused = build_full_payload(&new, b"A/B", PRIV, BLOCK, &small_manifest);
    assert_eq!(refused.err().map(|e| e.code), Some(Code::resource_limit));
    let no_operations = ResourceProfile {
        max_operations: 0,
        ..EMBEDDED
    };
    let refused = build_full_payload(&new, b"A/B", PRIV, BLOCK, &no_operations);
    assert_eq!(refused.err().map(|e| e.code), Some(Code::resource_limit));
    assert!(build_full_payload(&[], b"A/B", PRIV, BLOCK, &no_operations).is_ok());
}

#[test]
fn a_sixteen_mib_image_fits_the_embedded_profile() {
    let raw = noise(16 << 20, 11);
    let payload = assert_round_trips(&raw, &EMBEDDED);
    assert_eq!(
        types_of(&manifest_of(&payload, &EMBEDDED)),
        vec![TYPE_REPLACE]
    );
    assert!(payload.len() < raw.len() + 1024);

    let compressible = text(16 << 20);
    let payload = assert_round_trips(&compressible, &EMBEDDED);
    let manifest = manifest_of(&payload, &EMBEDDED);
    assert!(ops_of(&manifest).len() <= EMBEDDED.max_operations);
    assert!(payload.len() < compressible.len() / 4);
}

#[test]
fn the_signature_slots_follow_the_key_size() {
    let new = text(8 * BLOCK);
    let private4096 = rsa::parse_privkey_pem(PRIV_4096).unwrap_or_else(|e| panic!("{}", e.msg));
    let public4096 = rsa::public_key_of(&private4096);
    let payload = build_full_payload(&new, b"A/B", PRIV_4096, BLOCK, &EMBEDDED)
        .unwrap_or_else(|e| panic!("{}", e.msg));
    let frame = layout::frame(&payload, &EMBEDDED).unwrap_or_else(|e| panic!("{}", e.msg));
    assert_eq!(
        frame.header.meta_sig_size as usize,
        sign::signature_message_size(private4096.size)
    );
    check_full_payload(&payload, public4096, &new, &EMBEDDED)
        .unwrap_or_else(|e| panic!("{}", e.msg));
}

#[test]
fn bad_inputs_are_refused_before_any_image_is_built() {
    let new = text(BLOCK);
    assert_eq!(
        build_full_payload(&new, b"", PRIV, BLOCK, &EMBEDDED)
            .err()
            .map(|e| e.code),
        Some(Code::required_field_missing)
    );
    assert_eq!(
        build_full_payload(&new, &[0xff], PRIV, BLOCK, &EMBEDDED)
            .err()
            .map(|e| e.code),
        Some(Code::parse)
    );
    assert_eq!(
        build_full_payload(&new, b"A/B", PRIV, 0, &EMBEDDED)
            .err()
            .map(|e| e.code),
        Some(Code::unsupported_block_size)
    );
    assert_eq!(
        build_full_payload(&new, b"A/B", PRIV, 8192, &EMBEDDED)
            .err()
            .map(|e| e.code),
        Some(Code::unsupported_block_size)
    );
    assert!(build_full_payload(&new, b"A/B", b"not a key", BLOCK, &EMBEDDED).is_err());
}

fn partition_of(new: &[u8], ops: Vec<Operation>) -> PartitionUpdate {
    return PartitionUpdate {
        partition_name: "A/B".to_string(),
        new_partition_info: Some(PartInfo {
            size: Some(new.len() as u64),
            hash: Some(sha256::hash_bytes(new).to_vec()),
        }),
        operations: ops,
        ..Default::default()
    };
}

fn replace(start: u64, blob: &[u8], cursor: u64) -> Operation {
    return Operation {
        r#type: TYPE_REPLACE,
        data_offset: Some(cursor),
        data_length: Some(blob.len() as u64),
        dst_extents: vec![Extent {
            start_byte: Some(start),
            num_bytes: Some(blob.len() as u64),
        }],
        data_sha256_hash: Some(sha256::hash_bytes(blob).to_vec()),
        ..Default::default()
    };
}

fn craft(partition: PartitionUpdate, blobs: &[u8]) -> Vec<u8> {
    let key = rsa::parse_privkey_pem(PRIV).unwrap_or_else(|e| panic!("{}", e.msg));
    let sig = sign::signature_message_size(key.size);
    let manifest = Manifest {
        block_size: Some(BLOCK as u32),
        signatures_offset: Some(blobs.len() as u64),
        signatures_size: Some(sig as u64),
        minor_version: Some(layout::PROJECT_MINOR),
        partitions: vec![partition],
        ..Default::default()
    };
    return seal(&manifest.encode_to_vec(), blobs, &key, sig)
        .unwrap_or_else(|e| panic!("{}", e.msg));
}

fn refused(payload: &[u8], new: &[u8]) -> Option<Code> {
    return check_full_payload(payload, public_key(), new, &EMBEDDED)
        .err()
        .map(|e| e.code);
}

#[test]
fn a_crafted_full_payload_that_covers_the_image_is_accepted() {
    let new: Vec<u8> = (0..16u8).collect();
    let payload = craft(partition_of(&new, vec![replace(0, &new, 0)]), &new);
    assert_eq!(refused(&payload, &new), None);
}

#[test]
fn a_payload_that_records_an_old_partition_is_not_a_full_image() {
    let new: Vec<u8> = (0..16u8).collect();
    let mut partition = partition_of(&new, vec![replace(0, &new, 0)]);
    partition.old_partition_info = Some(PartInfo {
        size: Some(16),
        hash: Some(sha256::hash_bytes(&[0u8; 16]).to_vec()),
    });
    assert_eq!(
        refused(&craft(partition, &new), &new),
        Some(Code::unsupported_field)
    );
}

#[test]
fn a_copy_is_refused_by_the_full_image_rules_though_the_client_accepts_it() {
    let new: Vec<u8> = (0..16u8).collect();
    let copy = Operation {
        r#type: TYPE_SOURCECOPY,
        dst_extents: vec![Extent {
            start_byte: Some(0),
            num_bytes: Some(8),
        }],
        src_extents: vec![Extent {
            start_byte: Some(8),
            num_bytes: Some(8),
        }],
        src_sha256_hash: Some(sha256::hash_bytes(&new[8..16]).to_vec()),
        ..Default::default()
    };
    let partition = partition_of(&new, vec![copy, replace(8, &new[8..16], 0)]);
    assert_eq!(
        refused(&craft(partition, &new[8..16]), &new),
        Some(Code::unsupported_field)
    );
}

#[test]
fn a_sparse_operation_is_refused_though_the_client_accepts_it() {
    let new: Vec<u8> = (0..16u8).collect();
    let sparse = Operation {
        r#type: TYPE_REPLACE_SPARSE,
        data_offset: Some(0),
        data_length: Some(0),
        dst_extents: vec![Extent {
            start_byte: Some(0),
            num_bytes: Some(16),
        }],
        data_sha256_hash: Some(sha256::hash_bytes(&[]).to_vec()),
        frame_table: Some(vec![0x00]),
        ..Default::default()
    };
    let partition = partition_of(&new, vec![sparse]);
    assert_eq!(
        refused(&craft(partition, &[]), &new),
        Some(Code::unsupported_field)
    );
}

#[test]
fn a_gap_is_refused_even_when_a_replay_from_zeros_would_match() {
    let mut new = zeros(8);
    new.extend(vec![5u8; 8]);
    let partition = partition_of(&new, vec![replace(8, &new[8..16], 0)]);
    assert_eq!(
        refused(&craft(partition, &new[8..16]), &new),
        Some(Code::partition_mismatch)
    );
}

#[test]
fn an_overlap_is_refused() {
    let new: Vec<u8> = (0..16u8).collect();
    let partition = partition_of(
        &new,
        vec![replace(0, &new[0..10], 0), replace(8, &new[8..16], 10)],
    );
    let blobs: Vec<u8> = new[0..10]
        .iter()
        .chain(new[8..16].iter())
        .cloned()
        .collect();
    assert_eq!(
        refused(&craft(partition, &blobs), &new),
        Some(Code::extent_overlap)
    );
}

#[test]
fn out_of_order_writes_are_refused_because_full_payloads_write_in_order() {
    let new: Vec<u8> = (0..16u8).collect();
    let partition = partition_of(
        &new,
        vec![replace(8, &new[8..16], 0), replace(0, &new[0..8], 8)],
    );
    let blobs: Vec<u8> = new[8..16].iter().chain(new[0..8].iter()).cloned().collect();
    assert_eq!(
        refused(&craft(partition, &blobs), &new),
        Some(Code::partition_mismatch)
    );
}

#[test]
fn a_new_image_of_another_length_or_content_is_refused() {
    let new: Vec<u8> = (0..16u8).collect();
    let payload = craft(partition_of(&new, vec![replace(0, &new, 0)]), &new);
    let longer: Vec<u8> = (0..17u8).collect();
    assert_eq!(refused(&payload, &longer), Some(Code::partition_mismatch));
    let mut other = new.clone();
    other[3] ^= 1;
    assert_eq!(refused(&payload, &other), Some(Code::hash_mismatch));
}

#[test]
fn a_new_partition_record_that_is_missing_is_refused() {
    let new: Vec<u8> = (0..16u8).collect();
    let mut partition = partition_of(&new, vec![replace(0, &new, 0)]);
    partition.new_partition_info = None;
    assert_eq!(
        refused(&craft(partition, &new), &new),
        Some(Code::required_field_missing)
    );
}

#[test]
fn blobs_that_do_not_produce_the_image_are_refused_by_the_replay() {
    let new: Vec<u8> = (0..16u8).collect();
    let wrong: Vec<u8> = vec![0xee; 16];
    let partition = partition_of(&new, vec![replace(0, &wrong, 0)]);
    assert_eq!(
        refused(&craft(partition, &wrong), &new),
        Some(Code::blob_mismatch)
    );
}
