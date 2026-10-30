


mod support;

use ota_generator::builder::Kind;
use ota_generator::builder::Op;
use ota_generator::builder::Spec;
use ota_generator::builder::build;
use support::model::*;
use support::*;

const CHUNK: usize = 512;
const DEVICE: usize = 20_000;

fn old_image() -> Vec<u8> {
    let mut rng = Rng::new(0xabc);
    return rng.bytes(DEVICE);
}



fn check(ops: Vec<Op>) -> Disk {
    let old = old_image();
    let want = expected(&old, &old, &ops, CHUNK);
    let payload = build(&Spec {
        chunk: CHUNK,
        partition: DEVICE as u64,
        old_hash: None,
        ops: ops,
        sign: Some(PRIVATE),
    })
    .map_err(|e| e.msg)
    .unwrap();
    let image = Old::new(old.clone());

    let mut buffered = Disk::new(old.clone());
    Session::new()
        .buffered_with_image(&payload, &mut buffered, &image)
        .map_err(|e| e.msg)
        .unwrap();
    assert!(buffered.data == want, "buffered diverged from the model");

    let mut streamed = Disk::new(old.clone());
    Session::new()
        .streamed_with_image(&payload, 333, &mut streamed, &image)
        .map_err(|e| e.msg)
        .unwrap();
    assert!(streamed.data == want, "streamed diverged from the model");
    return buffered;
}

fn replace(dst: Vec<(u64, u64)>, seed: u8) -> Op {
    let mut op = Op::new(Kind::Replace, dst.clone());
    let total: u64 = dst.iter().map(|(_, n)| *n).sum();
    op.data = (0..total).map(|i| seed.wrapping_add(i as u8)).collect();
    return op;
}

#[test]
fn unaligned_short_and_out_of_order_ranges_match_the_model() {


    let a = replace(vec![(3, 5), (7000, 1), (100, 300)], 1);

    let mut frames = Op::new(Kind::Frames, vec![(9001, 700), (11, 3), (15_000, 613)]);
    frames.data = (0..1316u32)
        .map(|i| if i % 9 < 5 { 0x20 } else { (i % 251) as u8 })
        .collect();

    let mut sparse = Op::new(Kind::Sparse, vec![(12_100, 1300)]);
    sparse.data = (0..1300u32).map(|i| (i % 13) as u8).collect();
    sparse.keep = vec![false, true, false];

    let zero = Op::new(Kind::Zero, vec![(DEVICE as u64 - 17, 17)]);
    let mut copy = Op::new(Kind::SourceCopy, vec![(16_000, 91)]);
    copy.src = vec![(5, 91)];
    copy.src_hash = source_hash(&old_image(), &copy.src);
    check(vec![a, frames, sparse, zero, copy]);
}

#[test]
fn overlapping_operations_resolve_to_the_last_write() {
    let first = replace(vec![(100, 200)], 10);
    let second = replace(vec![(150, 200)], 90);
    let mut zero = Op::new(Kind::Zero, vec![(120, 10)]);
    zero.dst = vec![(120, 10)];

    let within = replace(vec![(500, 40), (520, 40)], 200);
    check(vec![first, second, zero, within]);
}

#[test]
fn an_operation_may_write_the_last_byte_and_a_single_byte() {
    let edge = replace(vec![(DEVICE as u64 - 1, 1)], 7);
    let single = replace(vec![(0, 1)], 9);
    check(vec![edge, single]);
}

#[test]
fn a_large_range_costs_one_write_per_chunk_and_applies_through_bounded_buffers() {

    let big = replace(vec![(2048, 16 * 512)], 3);
    let disk = check(vec![big]);
    assert_eq!(disk.writes, 16);
}
