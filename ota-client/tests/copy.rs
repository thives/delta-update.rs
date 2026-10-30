



mod support;

use std::cell::Cell;

use ota_client::policy::Policy;
use ota_client::apply::NoProgress;
use ota_client::apply::Trust;
use ota_client::apply::prepare;
use ota_client::checkpoint::MemoryStore;
use ota_client::checkpoint::TargetId;
use ota_client::image::OldImage;
use ota_client::sink::check_range;
use ota_client::stream;
use ota_generator::DevicePreference;
use ota_generator::SearchConfig;



fn generous() -> SearchConfig {
    return SearchConfig {
        release_version: 0,
        max_search_seconds: 300.0,
        stagnation_seconds: 300.0,
        max_generations: Some(40),
        ..SearchConfig::quick()
    };
}




fn sink_only() -> SearchConfig {
    return SearchConfig {
        release_version: 0,
        max_search_seconds: 300.0,
        stagnation_seconds: 300.0,
        min_improvement_bytes: 0,
        seed: 1,
        max_generations: Some(40),
        device: device(),
        readable_target: false,
    };
}


fn device() -> DevicePreference {
    return match std::env::var("OTA_TEST_DEVICE").as_deref() {
        Ok("auto") => DevicePreference::Auto,
        _ => DevicePreference::Software,
    };
}

use ota_generator::build_payload_with;
use ota_payload::error::Code;
use ota_payload::error::PayloadError;
use ota_payload::manifest::PartitionUpdate;
use ota_payload::profile::EMBEDDED;
use support::*;

const TARGET: TargetId = [0x33; 32];



struct Image {
    data: Vec<u8>,
    capacity: u64,
    reads: Cell<usize>,
}

impl Image {
    fn new(data: Vec<u8>) -> Image {
        let capacity = data.len() as u64;
        return Image {
            data: data,
            capacity: capacity,
            reads: Cell::new(0),
        };
    }
}

impl OldImage for Image {
    fn capacity(&self) -> u64 {
        return self.capacity;
    }

    fn read_at(&self, offset: u64, out: &mut [u8]) -> Result<(), PayloadError> {
        check_range(offset, out.len(), self.capacity)?;
        self.reads.set(self.reads.get() + 1);
        let off = offset as usize;
        out.copy_from_slice(&self.data[off..(off + out.len())]);
        return Ok(());
    }
}


fn reopen(payload: &[u8]) -> impl FnOnce(u64) -> Chunked + '_ {
    return move |offset: u64| Chunked::at(payload, offset as usize, 4096);
}




fn swapped() -> (Vec<u8>, Vec<u8>, Vec<u8>) {
    let old = image((0..8).map(|b| noise(b + 300)).collect());
    let new = image((0..8).map(|b| old[((b + 4) % 8) * BS..][..BS].to_vec()).collect());
    let payload = swapped_payload(&old, &new, true);
    return (old, new, payload);
}



fn swapped_payload(old: &[u8], new: &[u8], with_hashes: bool) -> Vec<u8> {
    let ops = vec![copy_op(old, 0, 4, 4), copy_op(old, 4, 0, 4)];
    let mut part = PartitionUpdate {
        partition_name: "A/B".into(),
        operations: ops,
        ..Default::default()
    };
    if with_hashes {
        part.old_partition_info = Some(part_info(old));
        part.new_partition_info = Some(part_info(new));
    } else {
        part.old_partition_info = Some(ota_payload::manifest::PartInfo {
            size: Some(old.len() as u64),
            hash: None,
        });


        part.new_partition_info = Some(ota_payload::manifest::PartInfo {
            size: Some(new.len() as u64),
            hash: None,
        });
    }
    return crafted_partition_payload(part, &[]);
}

fn code_of<T>(result: Result<T, PayloadError>) -> Code {
    return match result {
        Ok(_) => panic!("expected a refusal"),
        Err(e) => e.code,
    };
}

#[test]
fn a_copy_round_trips_through_every_path() {
    let (old, new, payload) = swapped();

    let mut sink = Disk::new(old.clone());
    let prepared = prepare(&payload, Trust::Unverified, &EMBEDDED).map_err(|e| e.msg).unwrap();
    prepared
        .apply_to_sink_with_image(&mut sink, &Image::new(old.clone()), &mut MemoryStore::new(), &TARGET, &NoProgress {})
        .map_err(|e| e.msg)
        .unwrap();
    assert!(sink.data == new);
    assert_eq!(sink.writes, 8);

    let mut device = Disk::new(old.clone());
    let prepared = prepare(&payload, Trust::Unverified, &EMBEDDED).map_err(|e| e.msg).unwrap();
    prepared
        .apply_to_device_with_image(&mut device, &Image::new(old.clone()), &mut MemoryStore::new(), &TARGET, Policy::Development, &NoProgress {})
        .map_err(|e| e.msg)
        .unwrap();
    assert!(device.data == new);

    let mut streamed = Disk::new(old.clone());
    let src = Chunked {
        data: payload.clone(),
        pos: 0,
        chunk: 4096,
    };
    let prepared = stream::prepare(src, Trust::Unverified, &EMBEDDED).map_err(|e| e.msg).unwrap();
    prepared
        .apply_with_image(reopen(&payload), &mut streamed, &Image::new(old.clone()), &mut MemoryStore::new(), &TARGET, Policy::Development, &NoProgress {})
        .map_err(|e| e.msg)
        .unwrap();
    assert!(streamed.data == new);
}

#[test]
fn a_copy_without_the_old_image_is_refused_with_no_write() {
    let (old, _new, payload) = swapped();

    let mut sink = Disk::new(old.clone());
    let prepared = prepare(&payload, Trust::Unverified, &EMBEDDED).map_err(|e| e.msg).unwrap();
    assert_eq!(code_of(prepared.apply_to_sink(&mut sink, &mut MemoryStore::new(), &TARGET, &NoProgress {})), Code::source_missing);
    assert_eq!(sink.writes, 0);

    let mut device = Disk::new(old.clone());
    let prepared = prepare(&payload, Trust::Unverified, &EMBEDDED).map_err(|e| e.msg).unwrap();
    assert_eq!(code_of(prepared.apply_to_device(&mut device, &mut MemoryStore::new(), &TARGET, Policy::Development, &NoProgress {})), Code::source_missing);
    assert_eq!(device.writes, 0);

    let mut streamed = Disk::new(old.clone());
    let src = Chunked {
        data: payload.clone(),
        pos: 0,
        chunk: 4096,
    };
    let prepared = stream::prepare(src, Trust::Unverified, &EMBEDDED).map_err(|e| e.msg).unwrap();
    assert_eq!(code_of(prepared.apply(reopen(&payload), &mut streamed, &mut MemoryStore::new(), &TARGET, Policy::Development, &NoProgress {})), Code::source_missing);
    assert_eq!(streamed.writes, 0);
}

#[test]
fn a_changed_old_image_fails_its_whole_hash_before_any_write() {
    let (old, _new, payload) = swapped();
    let mut wrong = old.clone();
    wrong[6 * BS] ^= 1;
    let mut device = Disk::new(old.clone());
    let prepared = prepare(&payload, Trust::Unverified, &EMBEDDED).map_err(|e| e.msg).unwrap();
    assert_eq!(
        code_of(prepared.apply_to_device_with_image(&mut device, &Image::new(wrong), &mut MemoryStore::new(), &TARGET, Policy::Development, &NoProgress {})),
        Code::source_mismatch
    );
    assert_eq!(device.writes, 0);
}

#[test]
fn a_changed_source_fails_its_copy_hash_before_any_write() {

    let old = image((0..8).map(|b| noise(b + 300)).collect());
    let new = image((0..8).map(|b| old[((b + 4) % 8) * BS..][..BS].to_vec()).collect());
    let payload = swapped_payload(&old, &new, false);
    let mut wrong = old.clone();

    wrong[6 * BS + 1] ^= 1;
    let mut sink = Disk::new(old.clone());
    let prepared = prepare(&payload, Trust::Unverified, &EMBEDDED).map_err(|e| e.msg).unwrap();
    assert_eq!(
        code_of(prepared.apply_to_sink_with_image(&mut sink, &Image::new(wrong), &mut MemoryStore::new(), &TARGET, &NoProgress {})),
        Code::source_mismatch
    );
    assert_eq!(sink.writes, 0);
}

#[test]
fn an_old_image_with_the_wrong_geometry_is_refused_with_no_write() {
    let (old, _new, payload) = swapped();

    let mut other = Image::new(old.clone());
    other.capacity = (old.len() + BS) as u64;
    let mut device = Disk::new(old.clone());
    let prepared = prepare(&payload, Trust::Unverified, &EMBEDDED).map_err(|e| e.msg).unwrap();
    assert_eq!(
        code_of(prepared.apply_to_device_with_image(&mut device, &other, &mut MemoryStore::new(), &TARGET, Policy::Development, &NoProgress {})),
        Code::source_geometry
    );
    assert_eq!(device.writes, 0);

    let short = Image::new(old[..(4 * BS)].to_vec());
    let mut device = Disk::new(old.clone());
    let prepared = prepare(&payload, Trust::Unverified, &EMBEDDED).map_err(|e| e.msg).unwrap();
    assert_eq!(
        code_of(prepared.apply_to_device_with_image(&mut device, &short, &mut MemoryStore::new(), &TARGET, Policy::Development, &NoProgress {})),
        Code::source_geometry
    );
    assert_eq!(device.writes, 0);
}

#[test]
fn a_copy_outside_the_recorded_old_partition_is_refused_at_prepare() {
    let old = image((0..8).map(|b| noise(b + 300)).collect());

    let mut op = copy_op(&old, 0, 4, 4);
    op.src_extents[0].start_byte = Some(6 * BS as u64);
    let part = PartitionUpdate {
        partition_name: "A/B".into(),
        operations: vec![op],
        old_partition_info: Some(part_info(&old)),
        ..Default::default()
    };
    let payload = crafted_partition_payload(part, &[]);
    assert_eq!(
        code_of(prepare(&payload, Trust::Unverified, &EMBEDDED)),
        Code::extent_out_of_range
    );
}

#[test]
fn resume_copies_only_the_blocks_not_yet_written() {
    let (old, new, payload) = swapped();
    let mut store = MemoryStore::new();


    let mut device = Flaky::new(old.clone(), 5);
    let prepared = prepare(&payload, Trust::Unverified, &EMBEDDED).map_err(|e| e.msg).unwrap();
    assert!(prepared
        .apply_to_device_with_image(&mut device, &Image::new(old.clone()), &mut store, &TARGET, Policy::Development, &NoProgress {})
        .is_err());
    device.fail_at = None;
    let before = device.disk.writes;
    let prepared = prepare(&payload, Trust::Unverified, &EMBEDDED).map_err(|e| e.msg).unwrap();
    prepared
        .apply_to_device_with_image(&mut device, &Image::new(old.clone()), &mut store, &TARGET, Policy::Development, &NoProgress {})
        .map_err(|e| e.msg)
        .unwrap();
    assert!(device.disk.data == new);

    assert_eq!(device.disk.writes - before, 4);
}

#[test]
fn reapplying_a_completed_copy_writes_nothing() {


    let (old, new, payload) = swapped();
    let mut store = MemoryStore::new();
    let mut device = Disk::new(old.clone());
    let prepared = prepare(&payload, Trust::Unverified, &EMBEDDED).map_err(|e| e.msg).unwrap();
    prepared
        .apply_to_device_with_image(&mut device, &Image::new(old.clone()), &mut store, &TARGET, Policy::Development, &NoProgress {})
        .map_err(|e| e.msg)
        .unwrap();
    let writes = device.writes;
    let prepared = prepare(&payload, Trust::Unverified, &EMBEDDED).map_err(|e| e.msg).unwrap();
    prepared
        .apply_to_device_with_image(&mut device, &Image::new(old.clone()), &mut store, &TARGET, Policy::Development, &NoProgress {})
        .map_err(|e| e.msg)
        .unwrap();
    assert_eq!(device.writes, writes);
    assert!(device.data == new);
}

#[test]
fn validation_reads_the_source_and_replay_reads_it_again() {


    let (old, _new, payload) = swapped();
    let image = Image::new(old.clone());
    let mut sink = Disk::new(old.clone());
    let prepared = prepare(&payload, Trust::Unverified, &EMBEDDED).map_err(|e| e.msg).unwrap();
    prepared
        .apply_to_sink_with_image(&mut sink, &image, &mut MemoryStore::new(), &TARGET, &NoProgress {})
        .map_err(|e| e.msg)
        .unwrap();
    assert_eq!(image.reads.get(), 8 + 8 + 8);
}

#[test]
fn many_destinations_may_reuse_one_source_block() {

    let old = image((0..8).map(|b| noise(b + 300)).collect());
    let mut new = Vec::new();
    for _ in 0..4 {
        new.extend_from_slice(&old[5 * BS..6 * BS]);
    }
    new.extend_from_slice(&old[4 * BS..]);
    let ops: Vec<_> = (0..4).map(|d| copy_op(&old, d, 5, 1)).collect();
    let part = PartitionUpdate {
        partition_name: "A/B".into(),
        operations: ops,
        old_partition_info: Some(part_info(&old)),
        new_partition_info: Some(part_info(&new)),
        ..Default::default()
    };
    let payload = crafted_partition_payload(part, &[]);
    let mut device = Disk::new(old.clone());
    let prepared = prepare(&payload, Trust::Unverified, &EMBEDDED).map_err(|e| e.msg).unwrap();
    let image = Image::new(old.clone());
    prepared
        .apply_to_device_with_image(&mut device, &image, &mut MemoryStore::new(), &TARGET, Policy::Development, &NoProgress {})
        .map_err(|e| e.msg)
        .unwrap();
    assert!(device.data == new);
}




fn rotated() -> (Vec<u8>, Vec<u8>) {
    let old = image((0..16).map(|b| noise(b + 700)).collect());
    let mut blocks: Vec<Vec<u8>> = (0..16).map(|b| old[((b + 4) % 16) * BS..][..BS].to_vec()).collect();
    blocks[3] = text(3);
    blocks[12] = text(12);
    return (old.clone(), image(blocks));
}

#[test]
fn the_generator_turns_relocated_regions_into_copies_that_round_trip() {
    let (old, new) = rotated();
    let payload = build_payload_with(&old, &new, b"A/B", PRIVATE, BS, &EMBEDDED, &generous())
        .map_err(|e| e.msg)
        .unwrap();
    let ops = operations(&payload);


    let copies: usize = ops
        .iter()
        .filter(|op| op.r#type == ota_payload::manifest::TYPE_SOURCECOPY)
        .map(|op| op.dst_extents.len())
        .sum();
    assert!(copies >= 2, "relocated regions were not copied: {} copy extents", copies);
    let image = Image::new(old.clone());
    let mut sink = Disk::new(old.clone());
    let keys = keys();
    let prepared = prepare(&payload, Trust::Verified(&keys), &EMBEDDED)
        .map_err(|e| e.msg)
        .unwrap();
    prepared
        .apply_to_device_with_image(&mut sink, &image, &mut MemoryStore::new(), &TARGET, Policy::Development, &NoProgress {})
        .map_err(|e| e.msg)
        .unwrap();
    assert!(sink.data == new);
    let mut streamed = Disk::new(old.clone());
    let src = Chunked {
        data: payload.clone(),
        pos: 0,
        chunk: 4096,
    };
    let prepared = stream::prepare(src, Trust::Verified(&keys), &EMBEDDED)
        .map_err(|e| e.msg)
        .unwrap();
    prepared
        .apply_with_image(reopen(&payload), &mut streamed, &image, &mut MemoryStore::new(), &TARGET, Policy::Development, &NoProgress {})
        .map_err(|e| e.msg)
        .unwrap();
    assert!(streamed.data == new);
}



fn fragmented() -> (Vec<u8>, Vec<u8>) {
    let old = image((0..32).map(|b| noise(b + 900)).collect());
    let mut blocks = vec![Vec::new(); 32];
    for k in 0..16usize {
        let src = 2 * ((k * 5) % 16);
        blocks[2 * k] = old[src * BS..][..BS].to_vec();
        blocks[2 * k + 1] = old[(src + 1) * BS..][..BS].to_vec();
    }
    return (old, image(blocks));
}





fn applies_to_device(payload: &[u8], old: &[u8], new: &[u8], profile: &ota_payload::profile::ResourceProfile) {
    let keys = keys();
    let mut dev = Disk::new(old.to_vec());
    let prepared = prepare(payload, Trust::Verified(&keys), profile)
        .map_err(|e| e.msg)
        .unwrap();
    prepared
        .apply_to_device_with_image(&mut dev, &Image::new(old.to_vec()), &mut MemoryStore::new(), &TARGET, Policy::Development, &NoProgress {})
        .map_err(|e| e.msg)
        .unwrap();
    assert!(dev.data == new);
}

#[test]
fn a_fragmented_plan_for_a_write_only_target_meets_the_operation_budget_and_applies_losslessly() {




    let (old, new) = fragmented();
    let profile = ota_payload::profile::ResourceProfile {
        max_operations: 4,
        ..EMBEDDED
    };
    let payload = build_payload_with(&old, &new, b"A/B", PRIVATE, BS, &profile, &sink_only())
        .map_err(|e| e.msg)
        .unwrap();
    let ops = operations(&payload);
    assert!(ops.len() <= 4, "{} operations against a budget of four", ops.len());
    assert!(
        ops.iter().all(|op| op.r#type != ota_payload::manifest::TYPE_TARGET_COPY),
        "a target copy in a payload for a target that cannot be read"
    );
    applies_to_device(&payload, &old, &new, &profile);
}

#[test]
fn a_sink_plan_under_tight_extent_limits_falls_back_and_applies_losslessly() {


    let (old, new) = fragmented();
    let profile = ota_payload::profile::ResourceProfile {
        max_operations: 4,
        max_extents_per_op: 2,
        max_total_extents: 4,
        ..EMBEDDED
    };
    let payload = build_payload_with(&old, &new, b"A/B", PRIVATE, BS, &profile, &sink_only())
        .map_err(|e| e.msg)
        .unwrap();
    let ops = operations(&payload);
    assert!(ops.len() <= 4, "{} operations against a budget of four", ops.len());
    assert!(
        ops.iter().all(|op| op.r#type != ota_payload::manifest::TYPE_TARGET_COPY),
        "a target copy in a payload for a target that cannot be read"
    );
    let copies: usize = ops
        .iter()
        .filter(|op| op.r#type == ota_payload::manifest::TYPE_SOURCECOPY)
        .map(|op| op.dst_extents.len())
        .sum();
    assert!(copies < 16, "all sixteen runs were copied under limits that cannot hold them: {} extents", copies);
    applies_to_device(&payload, &old, &new, &profile);
}

#[test]
fn a_generated_plan_that_keeps_bytes_is_refused_by_a_write_only_sink() {



    let (old, new) = fragmented();
    let profile = ota_payload::profile::ResourceProfile {
        max_operations: 4,
        ..EMBEDDED
    };
    let payload = build_payload_with(&old, &new, b"A/B", PRIVATE, BS, &profile, &sink_only())
        .map_err(|e| e.msg)
        .unwrap();
    let prepared = prepare(&payload, Trust::Unverified, &profile).map_err(|e| e.msg).unwrap();
    assert_eq!(prepared.contract(), ota_payload::plan::Contract::BaseDependent);
    let mut sink = Disk::new(old.clone());
    let mut store = MemoryStore::new();
    assert_eq!(
        code_of(prepared.apply_to_sink_with_image(&mut sink, &Image::new(old.clone()), &mut store, &TARGET, &NoProgress {})),
        Code::target_not_readable
    );
    assert_eq!(sink.writes, 0);
    assert_eq!(store.replacements, 0);
}
