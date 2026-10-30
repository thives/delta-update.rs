



mod support;

use ota_client::apply::NoProgress;
use ota_client::apply::Trust;
use ota_client::apply::prepare;
use ota_client::checkpoint::MemoryStore;
use ota_client::policy::Outcome;
use ota_client::policy::Policy;
use ota_client::policy::ReleaseFloor;
use ota_client::policy::VolatileFloor;
use ota_client::stream;
use ota_payload::error::Code;
use ota_payload::error::PayloadError;
use ota_payload::profile::EMBEDDED;
use support::*;

fn refused(result: Result<Outcome, PayloadError>) -> Code {
    return match result {
        Ok(o) => panic!("expected a refusal, got {:?}", o),
        Err(e) => e.code,
    };
}

#[test]
fn strict_refuses_unauthenticated_metadata_before_any_write() {
    let payload = payload_for(base_dependent_ops(), Some(5), true);
    let prepared = prepare(&payload, Trust::Unverified, &EMBEDDED).unwrap();
    let floor = VolatileFloor::new(0);
    let mut dev = Disk::new(old_image());
    let mut store = MemoryStore::new();
    let got = refused(prepared.apply_to_device_with_image(
        &mut dev,
        &BaseImage(old_image()),
        &mut store,
        &TARGET,
        Policy::Strict(&floor),
        &NoProgress {},
    ));
    assert_eq!(got, Code::signature_missing);
    assert_eq!(dev.writes, 0);
    assert!(store.stored().is_none());
}

#[test]
fn development_applies_unauthenticated_metadata_but_never_claims_verification() {
    let payload = payload_for(base_dependent_ops(), Some(5), true);
    let prepared = prepare(&payload, Trust::Unverified, &EMBEDDED).unwrap();
    let mut dev = Disk::new(old_image());
    let outcome = prepared
        .apply_to_device_with_image(
            &mut dev,
            &BaseImage(old_image()),
            &mut MemoryStore::new(),
            &TARGET,
            Policy::Development,
            &NoProgress {},
        )
        .unwrap();
    assert_eq!(outcome, Outcome::Applied);
    assert_eq!(&dev.data[..2 * BS], &new_image()[..2 * BS]);
}

#[test]
fn a_strict_update_with_the_base_and_a_release_is_verified() {
    let payload = payload_for(base_dependent_ops(), Some(5), true);
    let k = keys();
    let prepared = prepare(&payload, Trust::Verified(&k), &EMBEDDED).unwrap();
    let floor = VolatileFloor::new(5);
    let mut dev = Disk::new(old_image());
    let outcome = prepared
        .apply_to_device_with_image(
            &mut dev,
            &BaseImage(old_image()),
            &mut MemoryStore::new(),
            &TARGET,
            Policy::Strict(&floor),
            &NoProgress {},
        )
        .unwrap();
    assert_eq!(outcome, Outcome::Verified);
    assert_eq!(dev.data, new_image());
}

#[test]
fn strict_refuses_a_payload_without_a_release_version_before_any_write() {
    let payload = payload_for(base_dependent_ops(), None, true);
    let k = keys();
    let prepared = prepare(&payload, Trust::Verified(&k), &EMBEDDED).unwrap();
    let floor = VolatileFloor::new(0);
    let mut dev = Disk::new(old_image());
    let got = refused(prepared.apply_to_device(
        &mut dev,
        &mut MemoryStore::new(),
        &TARGET,
        Policy::Strict(&floor),
        &NoProgress {},
    ));
    assert_eq!(got, Code::required_field_missing);
    assert_eq!(dev.writes, 0);
}

#[test]
fn strict_refuses_a_release_below_the_floor_and_accepts_the_floor_itself() {
    let payload = payload_for(independent_ops(), Some(4), true);
    let k = keys();
    let prepared = prepare(&payload, Trust::Verified(&k), &EMBEDDED).unwrap();
    let mut dev = Disk::new(old_image());
    let mut store = MemoryStore::new();
    let floor = VolatileFloor::new(5);
    assert_eq!(
        refused(prepared.apply_to_device(&mut dev, &mut store, &TARGET, Policy::Strict(&floor), &NoProgress {})),
        Code::rollback_refused
    );
    assert_eq!(dev.writes, 0);
    assert!(store.stored().is_none());


    let payload = payload_for(independent_ops(), Some(5), true);
    let prepared = prepare(&payload, Trust::Verified(&k), &EMBEDDED).unwrap();
    let outcome = prepared
        .apply_to_device(&mut dev, &mut MemoryStore::new(), &TARGET, Policy::Strict(&floor), &NoProgress {})
        .unwrap();
    assert_eq!(outcome, Outcome::Verified);
}

#[test]
fn a_failing_floor_read_refuses_the_update_before_any_write() {
    struct Unreadable;
    impl ReleaseFloor for Unreadable {
        fn minimum(&self) -> Result<u64, PayloadError> {
            return Err(ota_client::sink::io_error());
        }
    }
    let payload = payload_for(independent_ops(), Some(5), true);
    let k = keys();
    let prepared = prepare(&payload, Trust::Verified(&k), &EMBEDDED).unwrap();
    let mut dev = Disk::new(old_image());
    let floor = Unreadable;
    assert_eq!(
        refused(prepared.apply_to_device(
            &mut dev,
            &mut MemoryStore::new(),
            &TARGET,
            Policy::Strict(&floor),
            &NoProgress {}
        )),
        Code::device_io
    );
    assert_eq!(dev.writes, 0);
}

#[test]
fn strict_refuses_a_new_image_without_its_signed_hash_before_any_write() {
    let payload = payload_for(independent_ops(), Some(5), false);
    let k = keys();
    let prepared = prepare(&payload, Trust::Verified(&k), &EMBEDDED).unwrap();
    let floor = VolatileFloor::new(0);
    let mut dev = Disk::new(old_image());
    assert_eq!(
        refused(prepared.apply_to_device(
            &mut dev,
            &mut MemoryStore::new(),
            &TARGET,
            Policy::Strict(&floor),
            &NoProgress {}
        )),
        Code::required_field_missing
    );
    assert_eq!(dev.writes, 0);
}

#[test]
fn a_wrong_base_is_refused_before_the_first_write_even_with_an_old_image() {


    let payload = payload_for(base_dependent_ops(), Some(5), true);
    let k = keys();
    let prepared = prepare(&payload, Trust::Verified(&k), &EMBEDDED).unwrap();
    let floor = VolatileFloor::new(0);
    let mut wrong = old_image();
    wrong[BS + 7] ^= 0x40;
    let mut dev = Disk::new(wrong.clone());
    assert_eq!(
        refused(prepared.apply_to_device_with_image(
            &mut dev,
            &BaseImage(old_image()),
            &mut MemoryStore::new(),
            &TARGET,
            Policy::Strict(&floor),
            &NoProgress {}
        )),
        Code::hash_mismatch
    );
    assert_eq!(dev.writes, 0);
    assert_eq!(dev.data, wrong);
}

#[test]
fn strict_refuses_a_base_dependent_update_that_records_no_old_hash() {
    let mut part = update(&old_image(), &new_image(), base_dependent_ops(), true);
    part.old_partition_info = None;
    let blob = blob_of(&base_dependent_ops());
    let payload = signed_partition(part, &blob, Some(5));
    let k = keys();
    let prepared = prepare(&payload, Trust::Verified(&k), &EMBEDDED).unwrap();
    assert_eq!(prepared.contract(), ota_payload::plan::Contract::BaseDependent);
    let floor = VolatileFloor::new(0);
    let mut dev = Disk::new(old_image());
    assert_eq!(
        refused(prepared.apply_to_device_with_image(
            &mut dev,
            &BaseImage(old_image()),
            &mut MemoryStore::new(),
            &TARGET,
            Policy::Strict(&floor),
            &NoProgress {}
        )),
        Code::required_field_missing
    );
    assert_eq!(dev.writes, 0);
}

#[test]
fn a_destination_independent_update_is_verified_from_unrelated_contents_under_strict() {
    let payload = payload_for(independent_ops(), Some(5), true);
    let k = keys();
    let prepared = prepare(&payload, Trust::Verified(&k), &EMBEDDED).unwrap();
    assert_eq!(prepared.contract(), ota_payload::plan::Contract::DestinationIndependent);
    let floor = VolatileFloor::new(0);
    let mut dev = Disk::new(vec![0x77; 3 * BS]);
    let outcome = prepared
        .apply_to_device(&mut dev, &mut MemoryStore::new(), &TARGET, Policy::Strict(&floor), &NoProgress {})
        .unwrap();
    assert_eq!(outcome, Outcome::Verified);
    assert_eq!(dev.data, new_image());
}

#[test]
fn a_write_only_apply_is_applied_and_never_verified() {
    let payload = payload_for(independent_ops(), Some(5), true);
    let k = keys();
    let prepared = prepare(&payload, Trust::Verified(&k), &EMBEDDED).unwrap();
    let mut dev = Disk::new(vec![0; 3 * BS]);
    let outcome = prepared
        .apply_to_sink(&mut dev, &mut MemoryStore::new(), &TARGET, &NoProgress {})
        .unwrap();
    assert_eq!(outcome, Outcome::Applied);
    assert_eq!(dev.data, new_image());
}

#[test]
fn a_duplicate_release_version_is_refused_by_the_scanner() {
    use prost::Message;
    let mut manifest = ota_payload::manifest::Manifest {
        block_size: Some(BS as u32),
        minor_version: Some(ota_payload::layout::PROJECT_MINOR),
        partitions: vec![update(&old_image(), &new_image(), vec![], true)],
        release_version: Some(5),
        ..Default::default()
    }
    .encode_to_vec();

    manifest.extend_from_slice(&[0xa0, 0x06, 0x01]);
    let mut payload = Vec::new();
    payload.extend_from_slice(b"CrAU");
    payload.extend_from_slice(&2u64.to_be_bytes());
    payload.extend_from_slice(&(manifest.len() as u64).to_be_bytes());
    payload.extend_from_slice(&0u32.to_be_bytes());
    payload.extend_from_slice(&manifest);
    assert_eq!(
        prepare(&payload, Trust::Unverified, &EMBEDDED).err().map(|e| e.code),
        Some(Code::bad_protobuf)
    );
}

#[test]
fn the_streamed_strict_path_refuses_before_it_opens_the_payload() {
    let payload = payload_for(base_dependent_ops(), Some(5), true);
    let prepared = stream::prepare(Chunked::at(&payload, 0, 64), Trust::Unverified, &EMBEDDED).unwrap();
    let floor = VolatileFloor::new(0);
    let mut dev = Disk::new(old_image());
    let mut opened = false;
    let result = prepared.apply_with_image(
        |offset| {
            opened = true;
            Chunked::at(&payload, offset as usize, 64)
        },
        &mut dev,
        &BaseImage(old_image()),
        &mut MemoryStore::new(),
        &TARGET,
        Policy::Strict(&floor),
        &NoProgress {},
    );
    assert_eq!(refused(result), Code::signature_missing);
    assert!(!opened);
    assert_eq!(dev.writes, 0);
}



fn keep_then_zero() -> (ota_payload::manifest::Operation, Vec<u8>) {
    let op = ota_payload::manifest::Operation {
        r#type: ota_payload::manifest::TYPE_REPLACE_SPARSE,
        data_offset: Some(0),
        data_length: Some(0),
        dst_extents: vec![extent(0, 2 * BS as u64)],

        frame_table: Some(vec![0b0100]),
        data_sha256_hash: Some(ota_payload::sha256::hash_bytes(&[]).to_vec()),
        ..Default::default()
    };
    return (op, Vec::new());
}

#[test]
fn a_write_only_sink_refuses_a_kept_sparse_chunk_and_leaves_the_checkpoint_as_it_was() {


    let new = image(vec![vec![10; BS], vec![0; BS], vec![12; BS]]);
    let k = keys();
    let ops = vec![keep_then_zero()];
    let payload = signed_partition(update(&old_image(), &new, ops.clone(), true), &blob_of(&ops), Some(5));



    let mut store = MemoryStore::new();
    let mut readable = Disk::new(old_image());
    prepare(&payload, Trust::Verified(&k), &EMBEDDED)
        .unwrap()
        .apply_to_device(&mut readable, &mut store, &TARGET, Policy::Development, &NoProgress {})
        .unwrap();
    assert_eq!(store.stored().unwrap().phase, ota_client::checkpoint::Phase::Complete);
    assert!(readable.data == new);

    let stored = store.stored();
    let replacements = store.replacements;
    let prepared = prepare(&payload, Trust::Verified(&k), &EMBEDDED).unwrap();
    assert_eq!(prepared.contract(), ota_payload::plan::Contract::BaseDependent);
    let mut sink = Disk::new(old_image());
    assert_eq!(
        refused(prepared.apply_to_sink(&mut sink, &mut store, &TARGET, &NoProgress {})),
        Code::target_not_readable
    );
    let prepared = prepare(&payload, Trust::Verified(&k), &EMBEDDED).unwrap();
    assert_eq!(
        refused(prepared.apply_to_sink_with_image(&mut sink, &BaseImage(old_image()), &mut store, &TARGET, &NoProgress {})),
        Code::target_not_readable
    );
    assert_eq!(sink.writes, 0);
    assert_eq!(sink.data, old_image());
    assert_eq!(store.replacements, replacements);
    assert_eq!(store.stored(), stored);


    let mut empty = MemoryStore::new();
    let prepared = prepare(&payload, Trust::Verified(&k), &EMBEDDED).unwrap();
    assert_eq!(
        refused(prepared.apply_to_sink(&mut sink, &mut empty, &TARGET, &NoProgress {})),
        Code::target_not_readable
    );
    assert_eq!(empty.replacements, 0);
    assert!(empty.stored().is_none());
}
