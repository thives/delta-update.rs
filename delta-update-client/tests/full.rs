mod support;

use delta_update_client::apply::NoProgress;
use delta_update_client::apply::Trust;
use delta_update_client::apply::prepare;
use delta_update_client::checkpoint::MemoryStore;
use delta_update_generator::build_full_payload;
use delta_update_payload::error::Code;
use delta_update_payload::error::PayloadError;
use delta_update_payload::layout;
use delta_update_payload::manifest::Extent;
use delta_update_payload::manifest::Operation;
use delta_update_payload::manifest::TYPE_REPLACE_FRAMES;
use delta_update_payload::profile::EMBEDDED;
use delta_update_payload::sha256;
use support::*;

fn full(new: &[u8]) -> Vec<u8> {
    return build_full_payload(new, b"A/B", PRIVATE, BS, &EMBEDDED)
        .map_err(|e| e.msg)
        .unwrap();
}

fn code<T>(result: Result<T, PayloadError>) -> Code {
    return match result {
        Ok(_) => panic!("expected a refusal"),
        Err(e) => e.code,
    };
}

fn zeros(blocks: usize) -> Vec<u8> {
    return vec![0u8; blocks * BS];
}

fn unrelated(len: usize) -> Vec<u8> {
    let mut x: u64 = 0x2545_f491_4f6c_dd1d;
    return (0..len)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            (x >> 24) as u8
        })
        .collect();
}

fn mixed_image() -> Vec<u8> {
    let mut v = zeros(2);
    v.extend(text(1));
    v.extend(text(2));
    v.extend(noise(3));
    v.extend(text(4));
    v.extend(zeros(1));
    v.extend(text(5));
    v.extend(vec![1u8; BS + 100]);
    return v;
}

fn shapes() -> Vec<Vec<u8>> {
    let mut partial = text(1);
    partial.truncate(BS - 3);
    return vec![
        zeros(3),
        text(1).into_iter().chain(text(2)).chain(text(3)).collect(),
        unrelated(2 * BS),
        mixed_image(),
        vec![7u8],
        vec![1u8, 2, 3, 4, 5],
        partial,
        unrelated(BS + 1),
    ];
}

fn starting(len: usize) -> Vec<Vec<u8>> {
    return vec![vec![0u8; len], vec![0xffu8; len], unrelated(len)];
}

#[test]
fn a_full_payload_updates_any_starting_contents_through_both_pipelines() {
    for new in shapes() {
        let payload = full(&new);
        for chunk in [None, Some(97usize), Some(BS)] {
            for start in starting(new.len()) {
                let mut dev = Disk::new(start);
                let result = match chunk {
                    Some(chunk) => Session::new().streamed(&payload, chunk, &mut dev),
                    None => Session::new().buffered(&payload, &mut dev),
                };
                result.map_err(|e| e.msg).unwrap();
                assert!(
                    dev.data == new,
                    "{}: the image of {} bytes did not apply",
                    chunk.map_or_else(|| "buffered".to_string(), |size| format!("streamed {size}")),
                    new.len()
                );
            }
        }
    }
}

#[test]
fn a_full_payload_applies_to_a_write_only_sink_with_no_old_image() {
    let new = mixed_image();
    let payload = full(&new);
    let keys = keys();
    let prepared = prepare(&payload, Trust::Verified(&keys), &EMBEDDED)
        .map_err(|e| e.msg)
        .unwrap();
    assert!(!prepared.geometry().needs_source, "no old image is needed");
    let mut dev = Disk::new(unrelated(new.len()));
    prepared
        .apply_to_sink(&mut dev, &mut MemoryStore::new(), &TARGET, &NoProgress {})
        .map_err(|e| e.msg)
        .unwrap();
    assert!(dev.data == new);
}

#[test]
fn an_undersized_device_is_refused_with_no_write_on_both_pipelines() {
    let new = mixed_image();
    let payload = full(&new);
    for capacity in [0, new.len() - 1] {
        let mut dev = Disk::new(vec![0u8; capacity]);
        assert_eq!(
            code(Session::new().buffered(&payload, &mut dev)),
            Code::partition_mismatch
        );
        assert_eq!(
            dev.writes, 0,
            "buffered: a device of {} bytes was written",
            capacity
        );

        let mut dev = Disk::new(vec![0u8; capacity]);
        assert_eq!(
            code(Session::new().streamed(&payload, 97, &mut dev)),
            Code::partition_mismatch
        );
        assert_eq!(
            dev.writes, 0,
            "streamed: a device of {} bytes was written",
            capacity
        );
    }
}

#[test]
fn a_frame_table_that_does_not_describe_its_blob_is_refused_before_any_write() {
    let blob = vec![0u8; BS];
    let op = Operation {
        r#type: TYPE_REPLACE_FRAMES,
        data_offset: Some(0),
        data_length: Some(BS as u64),
        dst_extents: vec![Extent {
            start_byte: Some(0),
            num_bytes: Some(BS as u64),
        }],
        data_sha256_hash: Some(sha256::hash_bytes(&blob).to_vec()),
        frame_table: Some(vec![0, 0]),
        ..Default::default()
    };
    let payload = signed_crafted_payload(op, &blob);
    let keys = keys();

    let refusal = match prepare(&payload, Trust::Verified(&keys), &EMBEDDED) {
        Ok(_) => panic!("a table of the wrong length was accepted"),
        Err(e) => e.code,
    };
    assert_eq!(refusal, Code::frame_table_invalid);
}

#[test]
fn a_corrupt_raw_block_on_the_streamed_path_is_refused_after_its_writes() {
    let new = unrelated(2 * BS);
    let mut payload = full(&new);
    let info = layout::parse_unverified(&payload, &EMBEDDED).ok().unwrap();
    let blob = info.parsed().info.blobs_start as usize;
    payload[blob + 1] ^= 1;
    let mut dev = Disk::new(vec![0u8; new.len()]);
    assert_eq!(
        code(Session::new().streamed(&payload, 97, &mut dev)),
        Code::blob_mismatch
    );
    assert!(
        dev.writes > 0,
        "the stream writes the operation before its hash is checked"
    );
    assert!(
        dev.data != new,
        "the device is left partly written, and the rerun must finish it"
    );
}

fn operation_count(payload: &[u8]) -> usize {
    let info = layout::parse_unverified(payload, &EMBEDDED).ok().unwrap();
    return info.parsed().manifest.partitions[0].operations.len();
}

fn clean_writes(payload: &[u8], len: usize) -> usize {
    let mut dev = Disk::new(vec![0u8; len]);
    Session::new()
        .buffered(payload, &mut dev)
        .map_err(|e| e.msg)
        .unwrap();
    return dev.writes;
}

fn resume_shapes() -> Vec<Vec<u8>> {
    return vec![mixed_image(), unrelated(3 * BS + 17)];
}

#[test]
fn a_cut_at_every_write_resumes_to_the_new_image_on_both_pipelines() {
    for new in resume_shapes() {
        let payload = full(&new);
        let writes = clean_writes(&payload, new.len());
        let start = unrelated(new.len());
        for streamed in [false, true] {
            for cut in 0..writes {
                let mut session = Session::new();
                let mut dev = Flaky::new(start.clone(), cut);
                let first = if streamed {
                    session.streamed(&payload, 97, &mut dev)
                } else {
                    session.buffered(&payload, &mut dev)
                };
                assert!(
                    first.is_err(),
                    "write {} did not interrupt (streamed={})",
                    cut,
                    streamed
                );
                dev.fail_at = None;
                let resumed = if streamed {
                    session.streamed(&payload, 97, &mut dev)
                } else {
                    session.buffered(&payload, &mut dev)
                };
                resumed.map_err(|e| e.msg).unwrap();
                assert!(
                    dev.disk.data == new,
                    "cut at write {} (streamed={}) did not resume to the image",
                    cut,
                    streamed
                );
            }
        }
    }
}

#[test]
fn a_failed_flush_resumes_at_the_operation_it_did_not_commit() {
    for new in resume_shapes() {
        let payload = full(&new);

        let flushes = operation_count(&payload) + 1;
        let start = unrelated(new.len());
        for cut in 0..flushes {
            let mut session = Session::new();
            let mut dev = Barrier::new(start.clone(), cut);
            assert!(session.buffered(&payload, &mut dev).is_err());
            dev.fail_flush_at = None;
            session
                .buffered(&payload, &mut dev)
                .map_err(|e| e.msg)
                .unwrap();
            assert!(
                dev.disk.data == new,
                "flush {} did not resume to the image",
                cut
            );
        }
    }
}

#[test]
fn a_failed_checkpoint_replacement_resumes_from_the_previous_record() {
    for new in resume_shapes() {
        let payload = full(&new);

        let records = operation_count(&payload) + 2;
        let start = unrelated(new.len());
        for cut in 0..records {
            let mut session = Session::new();
            session.store.fail_at = Some(cut);
            let mut dev = Disk::new(start.clone());
            assert!(
                session.buffered(&payload, &mut dev).is_err(),
                "record {} did not interrupt",
                cut
            );
            session.store.fail_at = None;
            session
                .buffered(&payload, &mut dev)
                .map_err(|e| e.msg)
                .unwrap();
            assert!(
                dev.data == new,
                "record {} did not resume to the image",
                cut
            );
        }
    }
}

#[test]
fn a_completed_full_update_writes_nothing_when_run_again_and_refuses_an_altered_target() {
    let new = mixed_image();
    let payload = full(&new);
    for streamed in [false, true] {
        let mut session = Session::new();
        let mut dev = Disk::new(unrelated(new.len()));
        if streamed {
            session
                .streamed(&payload, 97, &mut dev)
                .map_err(|e| e.msg)
                .unwrap();
        } else {
            session
                .buffered(&payload, &mut dev)
                .map_err(|e| e.msg)
                .unwrap();
        }
        let writes = dev.writes;
        if streamed {
            session
                .streamed(&payload, 97, &mut dev)
                .map_err(|e| e.msg)
                .unwrap();
        } else {
            session
                .buffered(&payload, &mut dev)
                .map_err(|e| e.msg)
                .unwrap();
        }
        assert_eq!(
            dev.writes, writes,
            "a completed update wrote again (streamed={})",
            streamed
        );

        dev.data[10] ^= 1;
        let rerun = if streamed {
            session.streamed(&payload, 97, &mut dev)
        } else {
            session.buffered(&payload, &mut dev)
        };
        assert_eq!(code(rerun), Code::hash_mismatch, "streamed={}", streamed);
    }
}
