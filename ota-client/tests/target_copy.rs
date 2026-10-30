



mod support;

use ota_client::apply::NoProgress;
use ota_client::apply::Trust;
use ota_client::apply::prepare;
use ota_client::checkpoint::MemoryStore;
use ota_client::sink::Sink;
use ota_generator::builder::Kind;
use ota_generator::builder::Op;
use ota_generator::builder::Spec;
use ota_generator::builder::build;
use ota_payload::error::Code;
use ota_payload::error::PayloadError;
use ota_payload::profile::EMBEDDED;
use support::model::*;
use support::*;

const CHUNK: usize = 512;
const DEVICE: usize = 12_000;
const TARGET_ID: [u8; 32] = [0x21; 32];

fn old_image() -> Vec<u8> {
    let mut rng = Rng::new(0x51);
    return rng.bytes(DEVICE);
}

fn payload_of(ops: Vec<Op>) -> Vec<u8> {
    return build(&Spec {
        chunk: CHUNK,
        partition: DEVICE as u64,
        old_hash: None,
        ops: ops,
        sign: Some(PRIVATE),
    })
    .map_err(|e| e.msg)
    .unwrap();
}

fn copy(src: Vec<(u64, u64)>, dst: Vec<(u64, u64)>, state: &[u8]) -> Op {
    let mut op = Op::new(Kind::TargetCopy, dst);
    op.src_hash = source_hash(state, &src);
    op.src = src;
    return op;
}


struct WriteOnly {
    disk: Disk,
}

impl Sink for WriteOnly {
    fn capacity(&self) -> u64 {
        return self.disk.capacity();
    }

    fn write_at(&mut self, offset: u64, data: &[u8]) -> Result<(), PayloadError> {
        return self.disk.write_at(offset, data);
    }

    fn flush(&mut self) -> Result<(), PayloadError> {
        return self.disk.flush();
    }
}

#[test]
fn an_in_target_copy_matches_the_model_on_both_paths() {
    let old = old_image();
    let mut seed = old.clone();
    let mut fill = Op::new(Kind::Replace, vec![(0, 1000)]);
    fill.data = (0..1000u32).map(|i| (i % 97) as u8 + 1).collect();
    apply_op(&mut seed, &old, &fill, CHUNK);


    let first = copy(
        vec![(2000, 300), (5000, 200)],
        vec![(7000, 300), (8000, 200)],
        &seed,
    );
    let mut state = seed.clone();
    apply_op(&mut state, &old, &first, CHUNK);
    let second = copy(vec![(7100, 150)], vec![(9500, 150)], &state);
    let ops = vec![fill, first, second];
    let want = expected(&old, &old, &ops, CHUNK);
    let payload = payload_of(ops);
    let image = Old::new(old.clone());

    let mut buffered = Disk::new(old.clone());
    Session::new()
        .buffered_with_image(&payload, &mut buffered, &image)
        .map_err(|e| e.msg)
        .unwrap();
    assert!(buffered.data == want);

    let mut streamed = Disk::new(old.clone());
    Session::new()
        .streamed_with_image(&payload, 401, &mut streamed, &image)
        .map_err(|e| e.msg)
        .unwrap();
    assert!(streamed.data == want);
}

#[test]
fn a_copy_onto_its_own_source_is_refused_before_any_write() {
    let old = old_image();
    let op = copy(vec![(0, 100)], vec![(50, 100)], &old);
    let payload = payload_of(vec![op]);
    let keys = keys();
    let refused = prepare(&payload, Trust::Verified(&keys), &EMBEDDED);
    assert_eq!(refused.err().map(|e| e.code), Some(Code::copy_overlap));
}

#[test]
fn a_cyclic_copy_within_one_operation_is_refused() {


    let old = old_image();
    let op = copy(vec![(0, 100), (200, 100)], vec![(200, 100), (0, 100)], &old);
    let payload = payload_of(vec![op]);
    let keys = keys();
    let refused = prepare(&payload, Trust::Verified(&keys), &EMBEDDED);
    assert_eq!(refused.err().map(|e| e.code), Some(Code::copy_overlap));
}

#[test]
fn an_in_target_copy_needs_a_readable_target() {
    let old = old_image();
    let mut fill = Op::new(Kind::Replace, vec![(4000, 64)]);
    fill.data = vec![7; 64];
    let op = copy(vec![(4000, 64)], vec![(8000, 64)], &{
        let mut s = old.clone();
        apply_op(&mut s, &old, &fill, CHUNK);
        s
    });
    let payload = payload_of(vec![fill, op]);
    let keys = keys();
    let prepared = prepare(&payload, Trust::Verified(&keys), &EMBEDDED)
        .map_err(|e| e.msg)
        .unwrap();
    let mut dev = WriteOnly {
        disk: Disk::new(old.clone()),
    };
    let refused = prepared.apply_to_sink(
        &mut dev,
        &mut MemoryStore::new(),
        &TARGET_ID,
        &NoProgress {},
    );
    assert_eq!(
        refused.err().map(|e| e.code),
        Some(Code::target_not_readable)
    );
    assert!(dev.disk.data == old, "a refused copy wrote to the target");
    assert_eq!(dev.disk.writes, 0);
}

#[test]
fn a_source_that_does_not_match_its_hash_is_refused_before_the_copy_writes() {
    let old = old_image();
    let mut op = copy(vec![(100, 64)], vec![(3000, 64)], &old);
    op.src_hash = [0xee; 32];
    let payload = payload_of(vec![op]);
    let image = Old::new(old.clone());
    let mut dev = Disk::new(old.clone());
    let refused = Session::new().buffered_with_image(&payload, &mut dev, &image);
    assert_eq!(refused.err().map(|e| e.code), Some(Code::source_mismatch));
    assert_eq!(dev.writes, 0);
    assert!(dev.data == old);
}
