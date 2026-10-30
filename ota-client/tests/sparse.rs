



mod support;

use ota_client::policy::Policy;
use ota_client::apply::NoProgress;
use ota_client::apply::Trust;
use ota_client::apply::prepare;
use ota_client::checkpoint::MemoryStore;
use ota_client::checkpoint::TargetId;
use ota_client::stream;
use ota_generator::SearchConfig;
use ota_generator::build_payload_with;
use ota_generator::builder;
use ota_payload::encoding::Mode;
use ota_payload::encoding::Varints;
use ota_payload::encoding::mode_at;
use ota_payload::encoding::push_varint;
use ota_payload::error::Code;
use ota_payload::layout;
use ota_payload::manifest::Operation;
use ota_payload::manifest::PartitionUpdate;
use ota_payload::manifest::TYPE_REPLACE;
use ota_payload::manifest::TYPE_REPLACE_SPARSE;
use ota_payload::profile::EMBEDDED;
use ota_payload::sha256;
use support::*;

const TARGET: TargetId = [0x22; 32];




fn alternating() -> (Vec<u8>, Vec<u8>) {
    let old = image((0..16).map(|b| noise(b + 1000)).collect());
    let new = image(
        (0..16)
            .map(|b| if b % 2 == 0 { text(b) } else { noise(b + 1000) })
            .collect(),
    );
    return (old, new);
}



fn coalesced(old: &[u8], new: &[u8]) -> Vec<u8> {
    return build_payload_with(old, new, b"A/B", PRIVATE, BS, &coalescing_profile(), &SearchConfig::quick())
        .map_err(|e| e.msg)
        .unwrap();
}





fn one_raw_replacement(old: &[u8], new: &[u8]) -> Vec<u8> {
    let mut replace = builder::Op::new(builder::Kind::Replace, vec![(0, new.len() as u64)]);
    replace.data = new.to_vec();
    return builder::build(&builder::Spec {
        chunk: 1,
        partition: new.len() as u64,
        old_hash: Some(sha256::hash_bytes(old)),
        ops: vec![replace],
        sign: Some(PRIVATE),
    })
    .map_err(|e| e.msg)
    .unwrap();
}


fn check_both(payload: &[u8], old: &[u8], new: &[u8]) {
    check_both_at(payload, old, new);
}



fn check_both_at(payload: &[u8], old: &[u8], new: &[u8]) {
    let mut buffered = Disk::new(old.to_vec());
    apply_buffered(payload, &mut buffered).map_err(|e| e.msg).unwrap();
    assert!(buffered.data == new, "buffered path diverged from the target");
    let mut streamed = Disk::new(old.to_vec());
    apply_streamed(payload, 1000, &mut streamed).map_err(|e| e.msg).unwrap();
    assert!(streamed.data == new, "streamed path diverged from the target");
}


fn map_of(modes: &[u8]) -> Vec<u8> {
    let mut map = vec![0u8; (modes.len() + 3) / 4];
    for (k, m) in modes.iter().enumerate() {
        map[k / 4] |= m << (2 * (k % 4));
    }
    return map;
}




fn stored_per_block(op: &Operation) -> Vec<usize> {
    let nb = op.dst_extents[0].num_bytes.unwrap_or(0) / BS as u64;
    let packed: &[u8] = op.frame_table.as_deref().unwrap_or(&[]);
    let map_bytes = ((nb + 3) / 4) as usize;
    let map = &packed[..map_bytes];
    let mut entries = Varints::new(&packed[map_bytes..]);
    let mut out = Vec::new();
    for b in 0..nb {
        let len = match mode_at(map, b).unwrap() {
            Mode::Keep | Mode::Zero => 0,
            Mode::Raw => BS,
            Mode::Zlib => entries.next_entry().unwrap() as usize,
        };
        out.push(len);
    }
    return out;
}

fn sparse_op(payload: &[u8]) -> Operation {
    let ops = operations(payload);
    let found: Vec<&Operation> = ops.iter().filter(|op| op.r#type == TYPE_REPLACE_SPARSE).collect();
    assert_eq!(found.len(), 1, "expected exactly one sparse operation");
    return found[0].clone();
}

#[test]
fn alternating_changed_and_kept_blocks_stay_one_sparse_operation() {
    let (old, new) = alternating();
    let payload = coalesced(&old, &new);
    let ops = operations(&payload);
    assert_eq!(ops.len(), 1, "the whole span is one operation");
    assert_eq!(ops[0].r#type, TYPE_REPLACE_SPARSE);
    check_both(&payload, &old, &new);
}

#[test]
fn kept_blocks_are_never_written() {


    let (old, new) = alternating();
    let payload = coalesced(&old, &new);
    let keys = keys();
    let prepared = prepare(&payload, Trust::Verified(&keys), &coalescing_profile())
        .map_err(|e| e.msg)
        .unwrap();
    let mut disk = Disk::new(old.clone());
    prepared
        .apply_to_device(&mut disk, &mut MemoryStore::new(), &TARGET, Policy::Development, &NoProgress {})
        .map_err(|e| e.msg)
        .unwrap();
    assert!(disk.data == new);

    assert_eq!(disk.writes, 8);

    let prepared = prepare(&payload, Trust::Verified(&keys), &coalescing_profile())
        .map_err(|e| e.msg)
        .unwrap();
    let mut sink = Disk::new(old.clone());
    assert_eq!(
        prepared
            .apply_to_sink(&mut sink, &mut MemoryStore::new(), &TARGET, &NoProgress {})
            .err()
            .map(|e| e.code),
        Some(Code::target_not_readable)
    );
    assert_eq!(sink.writes, 0);
}

#[test]
fn zero_blocks_inside_a_span_need_no_blob_bytes() {
    let old = image((0..8).map(|b| noise(b + 2000)).collect());
    let new = image(
        (0..8)
            .map(|b| match b {
                0 | 2 => text(b),
                4 => vec![0u8; BS],
                _ => noise(b + 2000),
            })
            .collect(),
    );
    let payload = coalesced(&old, &new);
    let op = sparse_op(&payload);
    let packed: &[u8] = op.frame_table.as_deref().unwrap_or(&[]);
    let map = &packed[..2];
    assert_eq!(mode_at(map, 4), Some(Mode::Zero));
    assert_eq!(mode_at(map, 1), Some(Mode::Keep));
    check_both(&payload, &old, &new);
}

#[test]
fn resume_inside_a_span_skips_kept_and_matching_blocks() {
    let (old, new) = alternating();
    let payload = coalesced(&old, &new);
    let op = sparse_op(&payload);
    let lens = stored_per_block(&op);
    let parsed = layout::parse_unverified(&payload, &EMBEDDED).map_err(|e| e.msg).unwrap();


    let blobs = parsed.parsed().info.blobs_start as usize;
    let cut = blobs + lens[..6].iter().sum::<usize>() + 5;
    let mut session = Session::new();
    let mut disk = Disk::new(old.clone());
    let first = session.streamed(&payload[..cut], 512, &mut disk);
    assert!(first.is_err(), "a cut stream must not complete");
    assert_eq!(disk.writes, 3);


    session.streamed(&payload, 512, &mut disk).map_err(|e| e.msg).unwrap();
    assert!(disk.data == new);
    assert_eq!(disk.writes, 3 + 8);
}

#[test]
fn reapplying_a_completed_sparse_operation_writes_nothing() {
    let (old, new) = alternating();
    let payload = coalesced(&old, &new);
    let mut session = Session::new();
    let mut disk = Disk::new(old.clone());
    session.streamed(&payload, 4096, &mut disk).map_err(|e| e.msg).unwrap();
    assert!(disk.data == new);
    let writes = disk.writes;
    session.streamed(&payload, 4096, &mut disk).map_err(|e| e.msg).unwrap();
    assert_eq!(disk.writes, writes);
}

#[test]
fn a_malformed_block_map_is_refused_before_any_access() {

    let mut packed = vec![0x10u8];
    push_varint(&mut packed, 0);
    let op = Operation {
        r#type: TYPE_REPLACE_SPARSE,
        data_offset: Some(0),
        data_length: Some(0),
        dst_extents: vec![blocks(0, 2)],
        frame_table: Some(packed),
        data_sha256_hash: Some(sha256::hash_bytes(&[]).to_vec()),
        ..Default::default()
    };
    let payload = crafted_payload(op, &[]);
    match prepare(&payload, Trust::Unverified, &EMBEDDED) {
        Ok(_) => panic!("a malformed block map was accepted"),
        Err(e) => assert_eq!(e.code, Code::block_map_invalid),
    };
}

#[test]
fn a_malformed_frame_inside_a_span_leaves_earlier_blocks_written() {


    let frame = vec![0x55u8; 100];
    let mut packed = map_of(&[1, 3, 0]);
    push_varint(&mut packed, frame.len() as u64);
    let op = Operation {
        r#type: TYPE_REPLACE_SPARSE,
        data_offset: Some(0),
        data_length: Some(frame.len() as u64),
        dst_extents: vec![blocks(0, 3)],
        frame_table: Some(packed),
        data_sha256_hash: Some(sha256::hash_bytes(&frame).to_vec()),
        ..Default::default()
    };
    let payload = crafted_payload(op, &frame);
    let prepared = match prepare(&payload, Trust::Unverified, &EMBEDDED) {
        Err(e) => panic!("structure refused: {}", e.msg),
        Ok(p) => p,
    };
    let mut disk = Disk::new(vec![0xaa; 3 * BS]);
    let got = match prepared.apply_to_device(&mut disk, &mut MemoryStore::new(), &TARGET, Policy::Development, &NoProgress {}) {
        Ok(_) => panic!("a malformed frame was applied"),
        Err(e) => e.code,
    };
    assert_eq!(got, Code::frame_malformed);


    assert_eq!(disk.writes, 1);
    assert!(disk.data[..BS].iter().all(|x| *x == 0));
    assert!(disk.data[(2 * BS)..] == vec![0xaa; BS][..]);
}



#[test]
fn a_one_byte_block_raw_replacement_with_zero_and_nonzero_bytes_applies_through_both_pipelines() {


    let old = vec![1u8; 64];
    let new: Vec<u8> = (0..64).map(|i| if i % 2 == 0 { 2 } else { 0 }).collect();
    let payload = one_raw_replacement(&old, &new);
    let ops = operations(&payload);
    assert_eq!(ops.len(), 1, "one raw replacement covers the whole run");
    assert_eq!(ops[0].r#type, TYPE_REPLACE);
    check_both_at(&payload, &old, &new);
}




fn empty_sparse(modes: &[u8], hash: Option<Vec<u8>>) -> Operation {
    return Operation {
        r#type: TYPE_REPLACE_SPARSE,
        data_offset: Some(0),
        data_length: Some(0),
        dst_extents: vec![blocks(0, modes.len() as u64)],
        frame_table: Some(map_of(modes)),
        data_sha256_hash: hash,
        ..Default::default()
    };
}

#[test]
fn signed_empty_sparse_blob_is_refused_before_any_access_unless_the_digest_is_empty() {
    let keys = keys();
    let wrong = sha256::hash_bytes(b"not the empty blob").to_vec();
    let refused = [
        (Some(wrong), Code::blob_mismatch),
        (Some(vec![0u8; 32]), Code::blob_mismatch),
        (None, Code::required_field_missing),
        (Some(Vec::new()), Code::required_field_missing),
    ];
    for modes in [&[0u8, 0][..], &[1, 1][..], &[0, 1][..]] {
        for (hash, want) in refused.clone() {

            let payload = signed_crafted_payload(empty_sparse(modes, hash), &[]);
            let buffered = prepare(&payload, Trust::Verified(&keys), &EMBEDDED).err();
            assert_eq!(buffered.map(|e| e.code), Some(want), "buffered, modes {:?}", modes);
            let src = Chunked {
                data: payload.clone(),
                pos: 0,
                chunk: 512,
            };
            let streamed = stream::prepare(src, Trust::Verified(&keys), &EMBEDDED).err();
            assert_eq!(streamed.map(|e| e.code), Some(want), "streamed, modes {:?}", modes);
        }
        let digest = sha256::hash_bytes(&[]).to_vec();
        let payload = signed_crafted_payload(empty_sparse(modes, Some(digest)), &[]);
        let mut disk = Disk::new(vec![0xaa; modes.len() * BS]);
        apply_buffered(&payload, &mut disk).map_err(|e| e.msg).unwrap();
        let mut disk = Disk::new(vec![0xaa; modes.len() * BS]);
        apply_streamed(&payload, 512, &mut disk).map_err(|e| e.msg).unwrap();
    }
}

#[test]
fn malformed_sparse_after_a_valid_operation_fails_preparation() {
    let blob = vec![7u8; BS];
    let valid = Operation {
        r#type: TYPE_REPLACE,
        data_offset: Some(0),
        data_length: Some(BS as u64),
        dst_extents: vec![blocks(0, 1)],
        data_sha256_hash: Some(sha256::hash_bytes(&blob).to_vec()),
        ..Default::default()
    };

    let mut bad = empty_sparse(&[0, 0], Some(sha256::hash_bytes(b"x").to_vec()));
    bad.data_offset = Some(BS as u64);
    bad.dst_extents[0].start_byte = Some(BS as u64);
    let part = PartitionUpdate {
        partition_name: "A/B".into(),
        operations: vec![valid, bad],
        ..Default::default()
    };
    let payload = crafted_partition_payload(part, &blob);
    let refused = prepare(&payload, Trust::Unverified, &EMBEDDED).err();
    assert_eq!(refused.map(|e| e.code), Some(Code::blob_mismatch));
}



#[test]
fn oversized_frame_table_and_block_map_are_refused_at_preparation() {

    let table = vec![0u8; EMBEDDED.max_frame_table_bytes + 1];
    let frames = Operation {
        r#type: ota_payload::manifest::TYPE_REPLACE_FRAMES,
        data_offset: Some(0),
        data_length: Some(0),
        dst_extents: vec![blocks(0, 1)],
        frame_table: Some(table),
        data_sha256_hash: Some(sha256::hash_bytes(&[]).to_vec()),
        ..Default::default()
    };
    let payload = crafted_payload(frames, &[]);
    let refused = prepare(&payload, Trust::Unverified, &EMBEDDED).err();
    assert_eq!(refused.map(|e| e.code), Some(Code::resource_limit));


    let map = vec![0u8; EMBEDDED.max_map_bytes + 1];
    let sparse = Operation {
        r#type: TYPE_REPLACE_SPARSE,
        data_offset: Some(0),
        data_length: Some(0),
        dst_extents: vec![blocks(0, 4100)],
        frame_table: Some(map),
        data_sha256_hash: Some(sha256::hash_bytes(&[]).to_vec()),
        ..Default::default()
    };
    let payload = crafted_payload(sparse, &[]);
    let refused = prepare(&payload, Trust::Unverified, &EMBEDDED).err();
    assert_eq!(refused.map(|e| e.code), Some(Code::resource_limit));
}
