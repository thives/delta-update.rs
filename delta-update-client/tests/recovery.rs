mod support;

use delta_update_client::apply::NoProgress;
use delta_update_client::apply::Trust;
use delta_update_client::apply::prepare;
use delta_update_client::checkpoint::CheckpointStore;
use delta_update_client::checkpoint::MemoryStore;
use delta_update_client::checkpoint::Phase;
use delta_update_client::checkpoint::RECORD_SIZE;
use delta_update_client::checkpoint::TargetId;
use delta_update_client::policy::Outcome;
use delta_update_client::policy::Policy;
use delta_update_client::policy::VolatileFloor;
use delta_update_client::sink::Readable;
use delta_update_client::sink::Sink;
use delta_update_client::sink::check_range;
use delta_update_client::stream;
use delta_update_payload::error::Code;
use delta_update_payload::error::PayloadError;
use delta_update_payload::profile::EMBEDDED;
use support::*;

const OTHER_TARGET: TargetId = [0x11; 32];

pub struct Lossy {
    pub data: Vec<u8>,
    pub writes: usize,
    pub lost: Option<std::ops::Range<usize>>,
}

impl Lossy {
    fn new(data: Vec<u8>) -> Lossy {
        return Lossy {
            data: data,
            writes: 0,
            lost: None,
        };
    }

    fn losing(data: Vec<u8>, lost: std::ops::Range<usize>) -> Lossy {
        return Lossy {
            data: data,
            writes: 0,
            lost: Some(lost),
        };
    }
}

impl Sink for Lossy {
    fn capacity(self: &Self) -> u64 {
        return self.data.len() as u64;
    }

    fn write_at(self: &mut Self, offset: u64, data: &[u8]) -> Result<(), PayloadError> {
        match check_range(offset, data.len(), self.capacity()) {
            Err(e) => return Err(e),
            Ok(()) => {}
        };
        self.writes += 1;
        let at = offset as usize;
        let dropped = match &self.lost {
            None => false,
            Some(r) => at < r.end && r.start < at + data.len(),
        };
        if !dropped {
            self.data[at..at + data.len()].copy_from_slice(data);
        }
        return Ok(());
    }

    fn flush(self: &mut Self) -> Result<(), PayloadError> {
        return Ok(());
    }
}

impl Readable for Lossy {
    fn read_at(self: &Self, offset: u64, out: &mut [u8]) -> Result<(), PayloadError> {
        let at = offset as usize;
        out.copy_from_slice(&self.data[at..at + out.len()]);
        return Ok(());
    }
}

fn payload_claiming(claimed: &[u8]) -> Vec<u8> {
    let ops = independent_ops();
    let blob = blob_of(&ops);
    let mut part = update(&old_image(), &new_image(), ops, true);
    part.new_partition_info = Some(part_info(claimed));
    return signed_partition(part, &blob, Some(5));
}

fn code_of(result: Result<Outcome, PayloadError>) -> Code {
    return match result {
        Ok(o) => panic!("expected an error, got {:?}", o),
        Err(e) => e.code,
    };
}

fn apply_independent(
    payload: &[u8],
    dev: &mut Lossy,
    store: &mut MemoryStore,
) -> Result<Outcome, PayloadError> {
    let k = keys();
    let prepared = prepare(payload, Trust::Verified(&k), &EMBEDDED).unwrap();
    let floor = VolatileFloor::new(0);
    return prepared.apply_to_device(dev, store, &TARGET, Policy::Strict(&floor), &NoProgress {});
}

#[test]
fn a_final_mismatch_is_recorded_and_refused_on_every_later_run() {
    let payload = payload_claiming(&[0x55; 3 * BS]);
    let mut dev = Lossy::new(old_image());
    let mut store = MemoryStore::new();
    assert_eq!(
        code_of(apply_independent(&payload, &mut dev, &mut store)),
        Code::hash_mismatch
    );

    assert_eq!(store.stored().unwrap().phase, Phase::Mismatch);
    assert_eq!(dev.data, new_image());
    let writes = dev.writes;
    let replacements = store.replacements;
    for _ in 0..3 {
        assert_eq!(
            code_of(apply_independent(&payload, &mut dev, &mut store)),
            Code::recovery_required
        );
    }

    assert_eq!(dev.writes, writes);
    assert_eq!(store.replacements, replacements);
}

#[test]
fn a_destination_independent_update_resets_from_any_contents_and_then_verifies() {
    let good = payload_claiming(&new_image());

    let mut dev = Lossy::losing(old_image(), BS..2 * BS);
    let mut store = MemoryStore::new();
    assert_eq!(
        code_of(apply_independent(&good, &mut dev, &mut store)),
        Code::hash_mismatch
    );
    assert_eq!(store.stored().unwrap().phase, Phase::Mismatch);

    dev.lost = None;
    let k = keys();
    let prepared = prepare(&good, Trust::Verified(&k), &EMBEDDED).unwrap();

    let cleared = prepared
        .reset_checkpoint(&dev, &mut store, &TARGET)
        .unwrap();
    assert_eq!(cleared.map(|c| c.phase), Some(Phase::Mismatch));
    assert!(store.stored().is_none());

    assert_eq!(
        prepared
            .reset_checkpoint(&dev, &mut store, &TARGET)
            .unwrap(),
        None
    );
    assert_eq!(
        apply_independent(&good, &mut dev, &mut store).unwrap(),
        Outcome::Verified
    );
    assert_eq!(dev.data, new_image());
}

#[test]
fn a_base_dependent_reset_requires_the_old_image_on_the_device_first() {
    let ops = base_dependent_ops();
    let blob = blob_of(&ops);
    let good = signed_partition(
        update(&old_image(), &new_image(), ops, true),
        &blob,
        Some(5),
    );
    let k = keys();
    let floor = VolatileFloor::new(0);

    let mut dev = Lossy::losing(old_image(), BS..2 * BS);
    let mut store = MemoryStore::new();
    let prepared = prepare(&good, Trust::Verified(&k), &EMBEDDED).unwrap();
    assert_eq!(
        code_of(prepared.apply_to_device_with_image(
            &mut dev,
            &BaseImage(old_image()),
            &mut store,
            &TARGET,
            Policy::Strict(&floor),
            &NoProgress {},
        )),
        Code::hash_mismatch
    );
    assert_eq!(store.stored().unwrap().phase, Phase::Mismatch);

    dev.lost = None;
    let prepared = prepare(&good, Trust::Verified(&k), &EMBEDDED).unwrap();
    assert_eq!(
        prepared
            .reset_checkpoint(&dev, &mut store, &TARGET)
            .err()
            .map(|e| e.code),
        Some(Code::hash_mismatch)
    );
    assert_eq!(store.stored().unwrap().phase, Phase::Mismatch);

    dev.data = old_image();
    let writes = dev.writes;
    prepared
        .reset_checkpoint(&dev, &mut store, &TARGET)
        .unwrap();
    assert!(store.stored().is_none());
    assert_eq!(dev.writes, writes);
    let outcome = prepare(&good, Trust::Verified(&k), &EMBEDDED)
        .unwrap()
        .apply_to_device_with_image(
            &mut dev,
            &BaseImage(old_image()),
            &mut store,
            &TARGET,
            Policy::Strict(&floor),
            &NoProgress {},
        )
        .unwrap();
    assert_eq!(outcome, Outcome::Verified);
    assert_eq!(dev.data, new_image());
}

#[test]
fn a_failed_mismatch_record_leaves_awaiting_final_and_the_next_run_repeats_the_check() {
    let payload = payload_claiming(&[0x55; 3 * BS]);
    let mut dev = Lossy::losing(old_image(), BS..2 * BS);
    let mut store = MemoryStore::new();

    store.fail_at = Some(3);
    assert_eq!(
        code_of(apply_independent(&payload, &mut dev, &mut store)),
        Code::hash_mismatch
    );
    assert_eq!(store.stored().unwrap().phase, Phase::AwaitingFinal);

    store.fail_at = None;
    assert_eq!(
        code_of(apply_independent(&payload, &mut dev, &mut store)),
        Code::hash_mismatch
    );
    assert_eq!(store.stored().unwrap().phase, Phase::Mismatch);
}

#[test]
fn an_interrupted_reset_keeps_the_mismatch_and_a_retry_completes() {
    let good = signed_partition(
        update(&old_image(), &new_image(), independent_ops(), true),
        &blob_of(&independent_ops()),
        Some(5),
    );
    let mut dev = Lossy::losing(old_image(), BS..2 * BS);
    let mut store = MemoryStore::new();
    assert_eq!(
        code_of(apply_independent(&good, &mut dev, &mut store)),
        Code::hash_mismatch
    );

    dev.lost = None;
    let k = keys();
    let prepared = prepare(&good, Trust::Verified(&k), &EMBEDDED).unwrap();
    store.fail_clear = true;
    assert_eq!(
        prepared
            .reset_checkpoint(&dev, &mut store, &TARGET)
            .err()
            .map(|e| e.code),
        Some(Code::device_io)
    );
    assert_eq!(store.stored().unwrap().phase, Phase::Mismatch);

    store.fail_clear = false;
    prepare(&good, Trust::Verified(&k), &EMBEDDED)
        .unwrap()
        .reset_checkpoint(&dev, &mut store, &TARGET)
        .unwrap();
    assert_eq!(
        apply_independent(&good, &mut dev, &mut store).unwrap(),
        Outcome::Verified
    );
}

#[test]
fn a_reset_whose_removal_is_not_confirmed_durable_says_so_and_leaves_no_record() {
    let good = signed_partition(
        update(&old_image(), &new_image(), independent_ops(), true),
        &blob_of(&independent_ops()),
        Some(5),
    );
    let mut dev = Lossy::losing(old_image(), BS..2 * BS);
    let mut store = MemoryStore::new();
    assert_eq!(
        code_of(apply_independent(&good, &mut dev, &mut store)),
        Code::hash_mismatch
    );

    dev.lost = None;
    let k = keys();
    let prepared = prepare(&good, Trust::Verified(&k), &EMBEDDED).unwrap();

    store.fail_clear_sync = true;
    assert_eq!(
        prepared
            .reset_checkpoint(&dev, &mut store, &TARGET)
            .err()
            .map(|e| e.code),
        Some(Code::checkpoint_removal_uncertain)
    );
    assert!(store.stored().is_none());

    store.fail_clear_sync = false;
    assert_eq!(
        apply_independent(&good, &mut dev, &mut store).unwrap(),
        Outcome::Verified
    );
}

#[test]
fn a_retry_that_finds_no_record_confirms_its_absence_before_it_reports_success() {
    let good = signed_partition(
        update(&old_image(), &new_image(), independent_ops(), true),
        &blob_of(&independent_ops()),
        Some(5),
    );
    let mut dev = Lossy::losing(old_image(), BS..2 * BS);
    let mut store = MemoryStore::new();
    assert_eq!(
        code_of(apply_independent(&good, &mut dev, &mut store)),
        Code::hash_mismatch
    );

    dev.lost = None;
    let k = keys();
    let prepared = prepare(&good, Trust::Verified(&k), &EMBEDDED).unwrap();

    store.fail_clear_sync = true;
    assert_eq!(
        prepared
            .reset_checkpoint(&dev, &mut store, &TARGET)
            .err()
            .map(|e| e.code),
        Some(Code::checkpoint_removal_uncertain)
    );
    assert!(store.stored().is_none());

    assert_eq!(
        prepared
            .reset_checkpoint(&dev, &mut store, &TARGET)
            .err()
            .map(|e| e.code),
        Some(Code::checkpoint_removal_uncertain)
    );
    assert!(store.stored().is_none());

    store.fail_clear_sync = false;
    assert!(
        prepared
            .reset_checkpoint(&dev, &mut store, &TARGET)
            .unwrap()
            .is_none()
    );
    assert!(store.stored().is_none());
}

#[test]
fn reset_refuses_another_target_and_a_damaged_record_without_clearing_it() {
    let payload = payload_claiming(&[0x55; 3 * BS]);
    let mut dev = Lossy::losing(old_image(), BS..2 * BS);
    let mut store = MemoryStore::new();
    assert_eq!(
        code_of(apply_independent(&payload, &mut dev, &mut store)),
        Code::hash_mismatch
    );
    let k = keys();
    let prepared = prepare(&payload, Trust::Verified(&k), &EMBEDDED).unwrap();

    assert_eq!(
        prepared
            .reset_checkpoint(&dev, &mut store, &OTHER_TARGET)
            .err()
            .map(|e| e.code),
        Some(Code::checkpoint_mismatch)
    );
    assert_eq!(store.stored().unwrap().phase, Phase::Mismatch);

    let mut bytes = [0u8; RECORD_SIZE];
    store.load(&mut bytes).unwrap();
    bytes[40] ^= 0x01;
    store.overwrite(&bytes);
    let writes = dev.writes;
    assert_eq!(
        prepared
            .reset_checkpoint(&dev, &mut store, &TARGET)
            .err()
            .map(|e| e.code),
        Some(Code::checkpoint_invalid)
    );
    assert_eq!(
        code_of(apply_independent(&payload, &mut dev, &mut store)),
        Code::checkpoint_invalid
    );
    assert_eq!(dev.writes, writes);
}

#[test]
fn a_stream_with_a_bad_payload_signature_is_never_complete_and_a_good_stream_completes() {
    let good = signed_partition(
        update(&old_image(), &new_image(), independent_ops(), true),
        &blob_of(&independent_ops()),
        Some(5),
    );
    let mut bad = good.clone();
    let last = bad.len() - 1;
    bad[last] ^= 0x01;
    let k = keys();
    let floor = VolatileFloor::new(0);
    let mut dev = Disk::new(old_image());
    let mut store = MemoryStore::new();
    let prepared =
        stream::prepare(Chunked::at(&bad, 0, 64), Trust::Verified(&k), &EMBEDDED).unwrap();
    let result = prepared.apply(
        |offset| Chunked::at(&bad, offset as usize, 64),
        &mut dev,
        &mut store,
        &TARGET,
        Policy::Strict(&floor),
        &NoProgress {},
    );
    assert!(result.is_err());

    assert_eq!(dev.data, new_image());
    let rec = store.stored().unwrap();
    assert_eq!(rec.phase, Phase::AwaitingFinal);
    assert_eq!(rec.next_op, 2);

    let prepared =
        stream::prepare(Chunked::at(&bad, 0, 64), Trust::Verified(&k), &EMBEDDED).unwrap();
    assert!(
        prepared
            .apply(
                |offset| Chunked::at(&bad, offset as usize, 64),
                &mut dev,
                &mut store,
                &TARGET,
                Policy::Strict(&floor),
                &NoProgress {},
            )
            .is_err()
    );
    assert_eq!(store.stored().unwrap().phase, Phase::AwaitingFinal);

    let prepared =
        stream::prepare(Chunked::at(&good, 0, 64), Trust::Verified(&k), &EMBEDDED).unwrap();
    let outcome = prepared
        .apply(
            |offset| Chunked::at(&good, offset as usize, 64),
            &mut dev,
            &mut store,
            &TARGET,
            Policy::Strict(&floor),
            &NoProgress {},
        )
        .unwrap();
    assert_eq!(outcome, Outcome::Verified);
    assert_eq!(store.stored().unwrap().phase, Phase::Complete);
}

#[test]
fn a_trailer_and_a_truncated_stream_fail_after_the_writes_with_a_fresh_checkpoint_each_time() {
    let good = signed_partition(
        update(&old_image(), &new_image(), independent_ops(), true),
        &blob_of(&independent_ops()),
        Some(5),
    );
    let k = keys();
    let floor = VolatileFloor::new(0);
    let mut trailing = good.clone();
    trailing.push(0);
    let truncated = good[..good.len() - 1].to_vec();
    for (bytes, want) in [(&trailing, Code::parse), (&truncated, Code::parse)] {
        let mut dev = Disk::new(old_image());
        let mut store = MemoryStore::new();
        let prepared =
            stream::prepare(Chunked::at(bytes, 0, 64), Trust::Verified(&k), &EMBEDDED).unwrap();
        let got = prepared
            .apply(
                |offset| Chunked::at(bytes, offset as usize, 64),
                &mut dev,
                &mut store,
                &TARGET,
                Policy::Strict(&floor),
                &NoProgress {},
            )
            .err()
            .map(|e| e.code);
        assert_eq!(got, Some(want));
        assert_eq!(dev.data, new_image());
        let rec = store.stored().unwrap();
        assert_eq!(rec.phase, Phase::AwaitingFinal);
        assert_eq!(rec.next_op, 2);
    }
}
