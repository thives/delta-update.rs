





mod support;

use ota_client::apply::NoProgress;
use ota_client::apply::Trust;
use ota_client::apply::prepare;
use ota_client::checkpoint::MemoryStore;
use ota_client::policy::Outcome;
use ota_client::policy::Policy;
use ota_client::policy::VolatileFloor;
use ota_payload::error::Code;
use ota_payload::error::PayloadError;
use ota_payload::profile::EMBEDDED;
use support::*;



fn payload() -> Vec<u8> {
    return payload_for(independent_ops(), Some(5), true);
}

fn apply(payload: &[u8], dev: &mut Disk, store: &mut MemoryStore) -> Result<Outcome, PayloadError> {
    let k = keys();
    let prepared = prepare(payload, Trust::Verified(&k), &EMBEDDED).unwrap();
    let floor = VolatileFloor::new(0);
    return prepared.apply_to_device(dev, store, &TARGET, Policy::Strict(&floor), &NoProgress {});
}

fn code_of(result: Result<Outcome, PayloadError>) -> Code {
    return match result {
        Ok(o) => panic!("expected an error, got {:?}", o),
        Err(e) => e.code,
    };
}

#[test]
fn an_unconfirmed_record_is_never_resumed_and_no_write_follows_its_refusal() {
    let payload = payload();
    for failing in 0..3 {
        let mut dev = Disk::new(old_image());
        let mut store = MemoryStore::new();

        store.fail_sync_at = Some(failing);
        assert_eq!(code_of(apply(&payload, &mut dev, &mut store)), Code::device_io, "{failing}");
        let writes = dev.writes;
        let replacements = store.replacements;

        store.fail_confirm = true;
        for _ in 0..2 {
            assert_eq!(
                code_of(apply(&payload, &mut dev, &mut store)),
                Code::checkpoint_unconfirmed,
                "{failing}"
            );
        }
        assert_eq!(dev.writes, writes, "{failing}: a write followed an unconfirmed record");
        assert_eq!(store.replacements, replacements, "{failing}");
        assert!(store.stored().is_some(), "{failing}: the unconfirmed record was dropped");

        store.fail_confirm = false;
        assert_eq!(apply(&payload, &mut dev, &mut store).unwrap(), Outcome::Verified, "{failing}");
        assert_eq!(dev.data, new_image(), "{failing}");
    }
}

#[test]
fn a_power_cut_that_loses_an_unconfirmed_record_resumes_from_the_record_that_was_durable() {
    let payload = payload();
    for failing in 0..3 {
        let mut dev = Disk::new(old_image());
        let mut store = MemoryStore::new();
        store.fail_sync_at = Some(failing);
        assert_eq!(code_of(apply(&payload, &mut dev, &mut store)), Code::device_io, "{failing}");



        store.crash();
        let durable_next_op = if failing == 0 { None } else { Some(failing as u32 - 1) };
        assert_eq!(store.stored().map(|r| r.next_op), durable_next_op, "{failing}");
        assert_eq!(apply(&payload, &mut dev, &mut store).unwrap(), Outcome::Verified, "{failing}");
        assert_eq!(dev.data, new_image(), "{failing}");
    }
}

#[test]
fn a_reset_does_not_wait_for_a_confirmation_and_removes_the_record_it_checked() {
    let payload = payload();
    let mut dev = Disk::new(old_image());
    let mut store = MemoryStore::new();
    store.fail_sync_at = Some(1);
    assert_eq!(code_of(apply(&payload, &mut dev, &mut store)), Code::device_io);


    store.fail_confirm = true;
    let k = keys();
    let prepared = prepare(&payload, Trust::Verified(&k), &EMBEDDED).unwrap();
    let before = dev.data.clone();
    let cleared = prepared.reset_checkpoint(&dev, &mut store, &TARGET).unwrap();
    assert!(cleared.is_some());
    assert!(store.stored().is_none());

    assert_eq!(dev.data, before);
}
