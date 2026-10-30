



mod support;

use ota_client::policy::Policy;
use ota_client::apply::NoProgress;
use ota_client::apply::Trust;
use ota_client::apply::prepare;
use ota_client::checkpoint::MemoryStore;
use ota_client::checkpoint::TargetId;
use ota_generator::SearchConfig;
use ota_generator::build_payload_with;
use ota_generator::builder::Kind;
use ota_generator::builder::Op;
use ota_generator::builder::Spec;
use ota_generator::builder::build;
use ota_payload::encoding::push_varint;
use ota_payload::error::Code;
use ota_payload::layout;
use ota_payload::manifest::Operation;
use ota_payload::manifest::TYPE_REPLACE;
use ota_payload::manifest::TYPE_REPLACE_FRAMES;
use ota_payload::profile::EMBEDDED;
use ota_payload::sha256;
use support::*;

const TARGET: TargetId = [0x11; 32];

#[test]
fn compressible_blocks_are_replaced_by_frames_and_round_trip() {
    let old = image((0..8).map(|b| noise(b + 100)).collect());
    let new = image((0..8).map(|b| text(b)).collect());
    let payload = round_trip(&old, &new);
    let ops = operations(&payload);
    assert!(ops.iter().any(|op| op.r#type == TYPE_REPLACE_FRAMES));

    assert!(payload.len() < 8 * BS / 2, "payload is {} bytes", payload.len());
}

#[test]
fn incompressible_blocks_fall_back_to_raw_replace() {
    let old = image((0..4).map(|b| text(b)).collect());
    let new = image((0..4).map(|b| noise(b + 500)).collect());
    let payload = round_trip(&old, &new);
    let ops = operations(&payload);
    assert!(ops.iter().all(|op| op.r#type != TYPE_REPLACE_FRAMES));
    assert!(ops.iter().any(|op| op.r#type == TYPE_REPLACE));
}

#[test]
fn one_operation_can_mix_compressed_and_raw_frames() {



    let new = image(
        (0..8)
            .map(|b| if b % 2 == 0 { text(b) } else { noise(b + 300) })
            .collect(),
    );
    let mut op = Op::new(Kind::Frames, vec![(0, new.len() as u64)]);
    op.data = new.clone();
    let spec = Spec {
        chunk: BS,
        partition: new.len() as u64,
        old_hash: None,
        ops: vec![op],
        sign: None,
    };
    let payload = match build(&spec) {
        Err(e) => panic!("builder failed: {}", e.msg),
        Ok(v) => v,
    };
    let ops = operations(&payload);
    let frames: Vec<&ota_payload::manifest::Operation> = ops
        .iter()
        .filter(|op| op.r#type == TYPE_REPLACE_FRAMES)
        .collect();
    assert_eq!(frames.len(), 1, "the changed run is one operation");
    let lens = stored_lengths(frames[0]);
    assert!(lens.iter().any(|n| *n < BS), "some frames are compressed");
    assert!(lens.iter().any(|n| *n == BS), "some blocks are stored raw");
}

#[test]
fn a_stream_cut_inside_a_frame_resumes_to_the_target() {
    let old = image((0..8).map(|b| noise(b + 40)).collect());
    let new = image((0..8).map(|b| text(b)).collect());
    let payload = build_payload_with(&old, &new, b"A/B", PRIVATE, BS, &EMBEDDED, &SearchConfig::quick())
        .map_err(|e| e.msg)
        .unwrap();
    let ops = operations(&payload);
    let frame_op = ops
        .iter()
        .find(|op| op.r#type == TYPE_REPLACE_FRAMES)
        .unwrap();
    let lens = stored_lengths(frame_op);

    let parsed = layout::parse_unverified(&payload, &EMBEDDED).map_err(|e| e.msg).unwrap();
    let blobs = parsed.parsed().info.blobs_start as usize;
    let cut = blobs + lens[0] + lens[1] + lens[2] + 5;
    let mut session = Session::new();
    let mut disk = Disk::new(old.clone());
    let first = session.streamed(&payload[..cut], 512, &mut disk);
    assert!(first.is_err(), "a cut stream must not complete");

    assert!(disk.data[..(3 * BS)] == new[..(3 * BS)]);
    assert!(disk.data[(3 * BS)..] == old[(3 * BS)..]);
    let before = disk.writes;
    session.streamed(&payload, 512, &mut disk).map_err(|e| e.msg).unwrap();
    assert!(disk.data == new);


    assert_eq!(disk.writes - before, 8);
}

#[test]
fn reapplying_a_completed_frame_operation_writes_nothing() {
    let old = image((0..6).map(|b| noise(b + 7)).collect());
    let new = image((0..6).map(|b| text(b)).collect());
    let payload = build_payload_with(&old, &new, b"A/B", PRIVATE, BS, &EMBEDDED, &SearchConfig::quick())
        .map_err(|e| e.msg)
        .unwrap();
    let mut session = Session::new();
    let mut disk = Disk::new(old.clone());
    session.streamed(&payload, 4096, &mut disk).map_err(|e| e.msg).unwrap();
    assert!(disk.data == new);
    let writes = disk.writes;

    session.streamed(&payload, 4096, &mut disk).map_err(|e| e.msg).unwrap();
    assert_eq!(disk.writes, writes);



    let mut disk = Disk::new(new.clone());
    apply_buffered(&payload, &mut disk).unwrap();
    assert_eq!(disk.data, new);
    assert!(disk.writes > 0);
}

#[test]
fn a_malformed_frame_with_a_matching_hash_is_refused_before_its_write() {


    let frame = vec![0x55u8; 100];
    let mut table = Vec::new();
    push_varint(&mut table, frame.len() as u64);
    let op = Operation {
        r#type: TYPE_REPLACE_FRAMES,
        data_offset: Some(0),
        data_length: Some(frame.len() as u64),
        dst_extents: vec![blocks(0, 1)],
        frame_table: Some(table),
        data_sha256_hash: Some(sha256::hash_bytes(&frame).to_vec()),
        ..Default::default()
    };
    let payload = crafted_payload(op, &frame);
    let parsed = layout::parse_unverified(&payload, &EMBEDDED).map_err(|e| e.msg).unwrap();
    assert_eq!(parsed.parsed().info.blobs_start as usize + 100, payload.len());

    let prepared = match prepare(&payload, Trust::Unverified, &EMBEDDED) {
        Err(e) => panic!("structure refused: {}", e.msg),
        Ok(p) => p,
    };
    let mut disk = Disk::new(vec![0u8; BS]);
    let got = match prepared.apply_to_device(&mut disk, &mut MemoryStore::new(), &TARGET, Policy::Development, &NoProgress {}) {
        Ok(_) => panic!("a malformed frame was applied"),
        Err(e) => e.code,
    };
    assert_eq!(got, Code::frame_malformed);
    assert_eq!(disk.writes, 0);
}
