






mod support;

use std::cell::Cell;

use ota_client::policy::Policy;
use ota_client::apply::NoProgress;
use ota_client::apply::Trust;
use ota_client::apply::prepare;
use ota_client::checkpoint::Phase;
use ota_client::checkpoint::Record;
use ota_generator::SearchConfig;
use ota_generator::build_payload_with;
use ota_generator::builder::Kind;
use ota_generator::builder::Op;
use ota_generator::builder::Spec;
use ota_generator::builder::build;
use ota_payload::error::Code;
use ota_payload::error::PayloadError;
use ota_payload::layout;
use ota_payload::profile::EMBEDDED;
use ota_payload::sha256;
use support::model::*;
use support::*;

const CHUNK: usize = 512;
const DEVICE: usize = 6000;



struct Scenario {
    old: Vec<u8>,
    want: Vec<u8>,
    ops: Vec<Op>,
    payload: Vec<u8>,
}

fn build_scenario(ops: Vec<Op>, old: Vec<u8>, old_hash: Option<[u8; 32]>) -> Scenario {
    let want = expected(&old, &old, &ops, CHUNK);
    let payload = build(&Spec {
        chunk: CHUNK,
        partition: DEVICE as u64,
        old_hash: old_hash,
        ops: ops.clone(),
        sign: Some(PRIVATE),
    })
    .map_err(|e| e.msg)
    .unwrap();
    return Scenario {
        old: old,
        want: want,
        ops: ops,
        payload: payload,
    };
}

fn scenario() -> Scenario {
    let old: Vec<u8> = (0..DEVICE).map(|i| (i * 31 % 251) as u8 + 1).collect();
    let text: Vec<u8> = b"checkpointed update "
        .iter()
        .cycle()
        .take(500)
        .cloned()
        .collect();
    let mut replace = Op::new(Kind::Replace, vec![(100, 300), (1000, 200)]);
    replace.data = text;
    let mut zero = Op::new(Kind::Zero, vec![(2000, 64)]);
    zero.dst = vec![(2000, 64)];
    let mut frames = Op::new(Kind::Frames, vec![(3000, 1100)]);
    frames.data = b"zlib me, please "
        .iter()
        .cycle()
        .take(1100)
        .cloned()
        .collect();
    let mut sparse = Op::new(Kind::Sparse, vec![(4000, 1600)]);
    sparse.data = (0..1600).map(|i| (i % 7) as u8).collect();
    sparse.keep = vec![false, true, false, false];
    let mut copy = Op::new(Kind::SourceCopy, vec![(5500, 64)]);
    copy.src = vec![(10, 64)];
    copy.src_hash = sha256::hash_bytes(&old[10..74]);
    return build_scenario(vec![replace, zero, frames, sparse, copy], old, None);
}

fn image(s: &Scenario) -> Old {
    return Old::new(s.old.clone());
}


fn clean_counts(s: &Scenario) -> (usize, usize) {
    let mut dev = Disk::new(s.old.clone());
    Session::new()
        .buffered_with_image(&s.payload, &mut dev, &image(s))
        .map_err(|e| e.msg)
        .unwrap();
    assert!(dev.data == s.want);
    return (dev.writes, s.ops.len() + 1);
}

#[test]
fn a_cut_at_every_write_resumes_to_the_target_on_both_paths() {
    let s = scenario();
    let (writes, _) = clean_counts(&s);
    for streamed in [false, true] {
        for cut in 0..writes {
            let mut session = Session::new();
            let mut dev = Flaky::new(s.old.clone(), cut);
            let first = if streamed {
                session.streamed_with_image(&s.payload, 97, &mut dev, &image(&s))
            } else {
                session.buffered_with_image(&s.payload, &mut dev, &image(&s))
            };
            assert!(
                first.is_err(),
                "write {cut} did not interrupt the run (streamed={streamed})"
            );
            dev.fail_at = None;
            let resumed = if streamed {
                session.streamed_with_image(&s.payload, 97, &mut dev, &image(&s))
            } else {
                session.buffered_with_image(&s.payload, &mut dev, &image(&s))
            };
            resumed.map_err(|e| e.msg).unwrap();
            assert!(
                dev.disk.data == s.want,
                "cut at write {cut} (streamed={streamed}) did not converge"
            );
        }
    }
}

#[test]
fn a_cut_during_operation_zero_is_recorded_as_initialized_and_resumes_without_a_preflight() {




    let old: Vec<u8> = (0..DEVICE).map(|i| (i * 31 % 251) as u8 + 1).collect();
    let mut first = Op::new(Kind::Replace, vec![(100, 300)]);
    first.data = b"checkpointed update "
        .iter()
        .cycle()
        .take(300)
        .cloned()
        .collect();
    let zero = Op::new(Kind::Zero, vec![(1000, 64)]);
    let s = build_scenario(
        vec![first, zero],
        old.clone(),
        Some(sha256::hash_bytes(&old)),
    );
    let mut session = Session::new();
    let mut dev = Flaky::new(s.old.clone(), 0);
    let cut = session.buffered(&s.payload, &mut dev);
    assert_eq!(
        cut.err().map(|e| e.code),
        Some(Code::device_io),
        "the run stopped before its first write"
    );

    assert!(dev.disk.data == s.old);
    let rec = session.store.stored().unwrap();
    assert_eq!(rec.phase, Phase::Initialized);
    assert_eq!(rec.next_op, 0);





    dev.fail_at = None;
    dev.disk.data[100] ^= 0xff;
    session
        .buffered(&s.payload, &mut dev)
        .map_err(|e| e.msg)
        .unwrap();
    assert!(dev.disk.data == s.want);
}

#[test]
fn a_failed_flush_resumes_at_the_operation_it_did_not_commit() {
    let s = scenario();
    let (_, flushes) = clean_counts(&s);
    for cut in 0..flushes {
        let mut session = Session::new();
        let mut dev = Barrier::new(s.old.clone(), cut);
        assert!(
            session
                .buffered_with_image(&s.payload, &mut dev, &image(&s))
                .is_err()
        );
        dev.fail_flush_at = None;
        session
            .buffered_with_image(&s.payload, &mut dev, &image(&s))
            .map_err(|e| e.msg)
            .unwrap();
        assert!(
            dev.disk.data == s.want,
            "flush {cut} did not resume to the target"
        );
    }
}

#[test]
fn a_failed_checkpoint_replacement_resumes_from_the_previous_record() {
    let s = scenario();


    let records = s.ops.len() + 2;
    for cut in 0..records {
        let mut session = Session::new();
        session.store.fail_at = Some(cut);
        let mut dev = Disk::new(s.old.clone());
        assert!(
            session
                .buffered_with_image(&s.payload, &mut dev, &image(&s))
                .is_err(),
            "replacement {cut} did not interrupt the run"
        );
        session.store.fail_at = None;
        session
            .buffered_with_image(&s.payload, &mut dev, &image(&s))
            .map_err(|e| e.msg)
            .unwrap();
        assert!(
            dev.data == s.want,
            "replacement {cut} did not resume to the target"
        );
    }
}

#[test]
fn a_resume_during_final_verification_writes_nothing_and_checks_the_signature() {
    let s = scenario();
    let (_, flushes) = clean_counts(&s);


    let mut session = Session::new();
    let mut dev = Barrier::new(s.old.clone(), flushes - 1);
    assert!(
        session
            .buffered_with_image(&s.payload, &mut dev, &image(&s))
            .is_err()
    );
    assert_eq!(session.store.stored().unwrap().phase, Phase::AwaitingFinal);
    dev.fail_flush_at = None;
    let writes = dev.disk.writes;
    session
        .buffered_with_image(&s.payload, &mut dev, &image(&s))
        .map_err(|e| e.msg)
        .unwrap();
    assert_eq!(
        dev.disk.writes, writes,
        "final verification rewrote operations"
    );
    assert!(dev.disk.data == s.want);
    assert_eq!(session.store.stored().unwrap().phase, Phase::Complete);
}

#[test]
fn a_resume_checks_the_payload_signature_over_the_restored_hash_state() {






    let s = scenario();
    let (_, flushes) = clean_counts(&s);
    let mut session = Session::new();
    let mut dev = Barrier::new(s.old.clone(), flushes - 1);
    assert!(
        session
            .buffered_with_image(&s.payload, &mut dev, &image(&s))
            .is_err()
    );
    assert_eq!(session.store.stored().unwrap().phase, Phase::AwaitingFinal);
    dev.fail_flush_at = None;
    let before = session.store.stored().unwrap();
    let writes = dev.disk.writes;
    let replacements = session.store.replacements;
    let mut damaged = s.payload.clone();
    let n = damaged.len();
    damaged[n - 40] ^= 0x01;
    let keys = keys();
    let prepared = match ota_client::stream::prepare(
        Chunked::at(&damaged, 0, 97),
        Trust::Verified(&keys),
        &EMBEDDED,
    ) {
        Err(e) => panic!("prepare refused: {}", e.msg),
        Ok(p) => p,
    };
    let opened = Cell::new(None);
    let refused = prepared.apply_with_image(
        |offset| {
            opened.set(Some(offset));
            Chunked::at(&damaged, offset as usize, 97)
        },
        &mut dev,
        &image(&s),
        &mut session.store,
        &TARGET,
        Policy::Development,
        &NoProgress {},
    );
    assert_eq!(
        opened.get(),
        Some(before.payload_offset),
        "the source was not opened at the checkpoint"
    );
    assert_eq!(
        refused.err().map(|e| e.code),
        Some(Code::signature_mismatch),
        "a damaged payload signature was accepted on resume"
    );
    assert_eq!(dev.disk.writes, writes);
    assert_eq!(session.store.replacements, replacements);
    assert_eq!(session.store.stored().unwrap(), before);

    session
        .streamed_with_image(&s.payload, 97, &mut dev, &image(&s))
        .map_err(|e| e.msg)
        .unwrap();
    assert!(dev.disk.data == s.want);
}

#[test]
fn zero_data_operations_advance_the_index_without_moving_the_offset() {
    let old: Vec<u8> = (0..DEVICE).map(|i| (i % 200) as u8 + 3).collect();
    let mut a = Op::new(Kind::Replace, vec![(0, 512)]);
    a.data = vec![0x41; 512];
    let zero = Op::new(Kind::Zero, vec![(1024, 512)]);
    let mut b = Op::new(Kind::Replace, vec![(2048, 512)]);
    b.data = vec![0x42; 512];
    let s = build_scenario(vec![a, zero, b], old, None);
    let blobs_start = layout::parse_unverified(&s.payload, &EMBEDDED)
        .ok()
        .unwrap()
        .parsed()
        .info
        .blobs_start;



    let mut session = Session::new();
    let mut dev = Flaky::new(s.old.clone(), 2);
    assert!(
        session
            .buffered_with_image(&s.payload, &mut dev, &image(&s))
            .is_err()
    );
    let rec = session.store.stored().unwrap();
    assert_eq!(rec.next_op, 2, "the zero operation was not committed");
    assert_eq!(
        rec.payload_offset,
        blobs_start + 512,
        "the zero operation moved the offset"
    );

    dev.fail_at = None;
    let before = dev.disk.writes;
    session
        .buffered_with_image(&s.payload, &mut dev, &image(&s))
        .map_err(|e| e.msg)
        .unwrap();
    assert!(dev.disk.data == s.want);

    assert_eq!(dev.disk.writes - before, 1);
}

#[test]
fn a_checkpoint_of_another_payload_is_refused_with_no_write() {
    let s = scenario();
    let mut session = Session::new();
    let mut dev = Flaky::new(s.old.clone(), 3);
    assert!(
        session
            .buffered_with_image(&s.payload, &mut dev, &image(&s))
            .is_err()
    );
    let before = dev.disk.data.clone();
    let writes = dev.disk.writes;

    let other = build_scenario(
        vec![{
            let mut op = Op::new(Kind::Replace, vec![(0, 512)]);
            op.data = vec![0x99; 512];
            op
        }],
        s.old.clone(),
        None,
    );
    dev.fail_at = None;
    let refused = session.buffered_with_image(&other.payload, &mut dev, &image(&other));
    assert_eq!(
        refused.err().map(|e| e.code),
        Some(Code::checkpoint_mismatch)
    );
    assert_eq!(dev.disk.data, before);
    assert_eq!(dev.disk.writes, writes);
}

#[test]
fn a_checkpoint_of_another_target_is_refused_with_no_write() {
    let s = scenario();
    let mut session = Session::new();
    let mut dev = Flaky::new(s.old.clone(), 3);
    assert!(
        session
            .buffered_with_image(&s.payload, &mut dev, &image(&s))
            .is_err()
    );
    let before = dev.disk.data.clone();
    let writes = dev.disk.writes;
    dev.fail_at = None;
    let keys = keys();
    let prepared = prepare(&s.payload, Trust::Verified(&keys), &EMBEDDED)
        .map_err(|e| e.msg)
        .unwrap();
    let other_target = [0x12u8; 32];
    let refused = prepared.apply_to_device_with_image(
        &mut dev,
        &image(&s),
        &mut session.store,
        &other_target,
        Policy::Development,
        &NoProgress {},
    );
    assert_eq!(
        refused.err().map(|e| e.code),
        Some(Code::checkpoint_mismatch)
    );
    assert_eq!(dev.disk.data, before);
    assert_eq!(dev.disk.writes, writes);
}

#[test]
fn a_damaged_checkpoint_is_refused_with_no_write() {
    let s = scenario();
    let mut session = Session::new();
    let mut dev = Flaky::new(s.old.clone(), 3);
    assert!(
        session
            .buffered_with_image(&s.payload, &mut dev, &image(&s))
            .is_err()
    );
    let mut bytes = session.store.stored().unwrap().encode();
    bytes[60] ^= 0x10;
    session.store.overwrite(&bytes);
    let before = dev.disk.data.clone();
    let writes = dev.disk.writes;
    dev.fail_at = None;
    let refused = session.buffered_with_image(&s.payload, &mut dev, &image(&s));
    assert_eq!(
        refused.err().map(|e| e.code),
        Some(Code::checkpoint_invalid)
    );
    assert_eq!(dev.disk.data, before);
    assert_eq!(dev.disk.writes, writes);
}

#[test]
fn a_stale_device_with_no_checkpoint_is_refused_by_the_preflight() {



    let old: Vec<u8> = (0..DEVICE).map(|i| (i * 13 % 251) as u8).collect();
    let mut op = Op::new(Kind::Replace, vec![(0, 512)]);
    op.data = vec![0x33; 512];
    let s = build_scenario(vec![op], old.clone(), Some(sha256::hash_bytes(&old)));
    let mut stale = old.clone();
    stale[DEVICE - 1] ^= 0x01;
    let mut dev = Disk::new(stale.clone());
    let refused = Session::new().buffered(&s.payload, &mut dev);
    assert_eq!(refused.err().map(|e| e.code), Some(Code::hash_mismatch));
    assert_eq!(dev.writes, 0);
    assert_eq!(dev.data, stale);
}

#[test]
fn the_record_matches_the_operation_it_describes() {


    let s = scenario();
    let mut session = Session::new();
    let mut dev = Flaky::new(s.old.clone(), 3);
    assert!(
        session
            .buffered_with_image(&s.payload, &mut dev, &image(&s))
            .is_err()
    );
    let mut rec: Record = session.store.stored().unwrap();
    rec.payload_offset += 1;
    session.store.overwrite(&rec.encode());
    dev.fail_at = None;
    let refused = session.buffered_with_image(&s.payload, &mut dev, &image(&s));
    assert_eq!(
        refused.err().map(|e| e.code),
        Some(Code::checkpoint_invalid)
    );
}

#[test]
fn a_source_opened_at_the_wrong_offset_is_refused_and_the_record_is_kept() {



    let s = scenario();
    let mut session = Session::new();
    let mut dev = Flaky::new(s.old.clone(), 3);
    assert!(session.buffered_with_image(&s.payload, &mut dev, &image(&s)).is_err());
    let before = session.store.stored().unwrap();
    dev.fail_at = None;
    let keys = keys();
    let prepared = match ota_client::stream::prepare(
        Chunked::at(&s.payload, 0, 97),
        Trust::Verified(&keys),
        &EMBEDDED,
    ) {
        Err(e) => panic!("prepare refused: {}", e.msg),
        Ok(p) => p,
    };
    let wrong = prepared.apply_with_image(
        |_| Chunked::at(&s.payload, 0, 97),
        &mut dev,
        &image(&s),
        &mut session.store,
        &TARGET,
        Policy::Development,
        &NoProgress {},
    );
    assert!(wrong.is_err(), "a payload read from the wrong offset was accepted");
    assert_eq!(session.store.stored().unwrap(), before);
    session
        .streamed_with_image(&s.payload, 97, &mut dev, &image(&s))
        .map_err(|e| e.msg)
        .unwrap();
    assert!(dev.disk.data == s.want);
}





struct Delta {
    old: Vec<u8>,
    new: Vec<u8>,
    payload: Vec<u8>,
}

fn delta() -> Delta {
    let old: Vec<u8> = (0..8 * CHUNK).map(|i| (i * 7 % 251) as u8 + 1).collect();
    let mut new = old.clone();
    new[CHUNK..2 * CHUNK].fill(0xa5);
    new[4 * CHUNK..5 * CHUNK].fill(0);
    let payload = build_payload_with(&old, &new, b"A/B", PRIVATE, CHUNK, &EMBEDDED, &SearchConfig::quick())
        .map_err(|e| e.msg)
        .unwrap();
    return Delta {
        old: old,
        new: new,
        payload: payload,
    };
}


fn apply_on(
    streamed: bool,
    session: &mut Session,
    payload: &[u8],
    dev: &mut Disk,
    old: &[u8],
) -> Result<(), PayloadError> {
    let img = Old::new(old.to_vec());
    return if streamed {
        session.streamed_with_image(payload, 97, dev, &img)
    } else {
        session.buffered_with_image(payload, dev, &img)
    };
}

#[test]
fn a_completed_update_resumes_with_no_write_when_the_target_is_unchanged() {
    let d = delta();
    for streamed in [false, true] {
        let mut session = Session::new();
        let mut dev = Disk::new(d.old.clone());
        apply_on(streamed, &mut session, &d.payload, &mut dev, &d.old)
            .map_err(|e| e.msg)
            .unwrap();
        assert!(dev.data == d.new);
        let writes = dev.writes;
        let replacements = session.store.replacements;
        apply_on(streamed, &mut session, &d.payload, &mut dev, &d.old)
            .map_err(|e| e.msg)
            .unwrap();
        assert_eq!(
            dev.writes, writes,
            "completed update wrote (streamed={streamed})"
        );
        assert_eq!(
            session.store.replacements, replacements,
            "completed update rewrote its record (streamed={streamed})"
        );
        assert!(dev.data == d.new);
    }
}

#[test]
fn a_completed_update_refuses_a_target_altered_since_completion() {
    let d = delta();
    for streamed in [false, true] {
        let mut session = Session::new();
        let mut dev = Disk::new(d.old.clone());
        apply_on(streamed, &mut session, &d.payload, &mut dev, &d.old)
            .map_err(|e| e.msg)
            .unwrap();

        dev.data[3 * CHUNK + 5] ^= 0x01;
        let writes = dev.writes;
        let replacements = session.store.replacements;
        let refused = apply_on(streamed, &mut session, &d.payload, &mut dev, &d.old);
        assert_eq!(
            refused.err().map(|e| e.code),
            Some(Code::hash_mismatch),
            "altered target was accepted (streamed={streamed})"
        );
        assert_eq!(
            dev.writes, writes,
            "refused resume wrote (streamed={streamed})"
        );
        assert_eq!(session.store.replacements, replacements);
        assert_eq!(session.store.stored().unwrap().phase, Phase::Complete);
    }
}

#[test]
fn a_completed_update_still_refuses_a_damaged_stream_ending() {
    let d = delta();
    let truncated = d.payload[..d.payload.len() - 1].to_vec();
    let mut trailing = d.payload.clone();
    trailing.push(0);
    for streamed in [false, true] {
        for bad in [&truncated, &trailing] {
            let mut session = Session::new();
            let mut dev = Disk::new(d.old.clone());
            apply_on(streamed, &mut session, &d.payload, &mut dev, &d.old)
                .map_err(|e| e.msg)
                .unwrap();
            let writes = dev.writes;
            let refused = apply_on(streamed, &mut session, bad, &mut dev, &d.old);



            match refused {
                Err(e) if streamed => assert_eq!(e.code, Code::parse),
                Err(_) => {}
                Ok(()) => panic!("damaged stream ending was accepted (streamed={streamed})"),
            };
            assert_eq!(
                dev.writes, writes,
                "refused resume wrote (streamed={streamed})"
            );
            assert!(dev.data == d.new);
        }
    }
}

#[test]
fn a_completed_update_without_a_recorded_new_hash_is_not_checked_against_the_target() {


    let s = scenario();
    for streamed in [false, true] {
        let mut session = Session::new();
        let mut dev = Disk::new(s.old.clone());
        apply_on(streamed, &mut session, &s.payload, &mut dev, &s.old)
            .map_err(|e| e.msg)
            .unwrap();
        dev.data[0] ^= 0xff;
        let writes = dev.writes;
        apply_on(streamed, &mut session, &s.payload, &mut dev, &s.old)
            .map_err(|e| e.msg)
            .unwrap();
        assert_eq!(
            dev.writes, writes,
            "completed update wrote (streamed={streamed})"
        );
    }
}
