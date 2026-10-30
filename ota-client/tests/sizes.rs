mod support;

use ota_client::policy::Policy;
use ota_client::apply::{NoProgress, Trust, prepare};
use ota_client::checkpoint::MemoryStore;
use ota_client::image::OldImage;
use ota_client::sink::{Readable, check_range};
use ota_client::stream;
use ota_generator::{DevicePreference, SearchConfig, build_full_payload, build_payload_with};
use ota_payload::error::{Code, PayloadError};
use ota_payload::manifest::{
    Extent, Operation, PartitionUpdate, TYPE_REPLACE, TYPE_REPLACE_SPARSE, TYPE_SOURCECOPY,
    TYPE_ZERO,
};
use ota_payload::profile::EMBEDDED;
use ota_payload::sha256;
use support::*;

struct Image(Vec<u8>);
impl OldImage for Image {
    fn capacity(&self) -> u64 {
        self.0.len() as u64
    }
    fn read_at(&self, offset: u64, out: &mut [u8]) -> Result<(), PayloadError> {
        check_range(offset, out.len(), self.capacity())?;
        out.copy_from_slice(&self.0[offset as usize..offset as usize + out.len()]);
        Ok(())
    }
}

fn ext(start: usize, len: usize) -> Extent {
    Extent {
        start_byte: Some(start as u64),
        num_bytes: Some(len as u64),
    }
}

fn payload(old: &[u8], new: &[u8], ops: Vec<Operation>, blob: &[u8]) -> Vec<u8> {
    crafted_partition_payload(
        PartitionUpdate {
            partition_name: "root".into(),
            old_partition_info: Some(part_info(old)),
            new_partition_info: Some(part_info(new)),
            operations: ops,
            ..Default::default()
        },
        blob,
    )
}

fn replace(new: &[u8]) -> Operation {
    Operation {
        r#type: TYPE_REPLACE,
        data_length: Some(new.len() as u64),
        dst_extents: vec![ext(0, new.len())],
        data_sha256_hash: Some(sha256::hash_bytes(new).to_vec()),
        ..Default::default()
    }
}

fn run<D: Readable>(
    bytes: &[u8],
    streamed: bool,
    dev: &mut D,
    image: Option<&Image>,
    store: &mut MemoryStore,
) -> Result<(), PayloadError> {
    if streamed {
        let prepared = stream::prepare(Chunked::at(bytes, 0, 3), Trust::Unverified, &EMBEDDED)?;
        let open = |offset| Chunked::at(bytes, offset as usize, 3);
        match image {
            Some(image) => {
                prepared.apply_with_image(open, dev, image, store, &TARGET, Policy::Development, &NoProgress {}).map(|_| ())
            }
            None => prepared.apply(open, dev, store, &TARGET, Policy::Development, &NoProgress {}).map(|_| ()),
        }
    } else {
        let prepared = prepare(bytes, Trust::Unverified, &EMBEDDED)?;
        match image {
            Some(image) => {
                prepared.apply_to_device_with_image(dev, image, store, &TARGET, Policy::Development, &NoProgress {}).map(|_| ())
            }
            None => prepared.apply_to_device(dev, store, &TARGET, Policy::Development, &NoProgress {}).map(|_| ()),
        }
    }
}

#[test]
fn in_place_grow_and_shrink_hash_only_image_prefixes_and_keep_capacity_and_tail() {
    for (old_len, new_len) in [(7, 19), (19, 7)] {
        let old = vec![0x31; old_len];
        let new = vec![0x72; new_len];
        let bytes = payload(&old, &new, vec![replace(&new)], &new);
        for streamed in [false, true] {
            let mut initial = vec![0xcd; old_len.max(new_len) + 11];
            initial[..old_len].copy_from_slice(&old);
            let tail = initial[new_len..].to_vec();
            let capacity = initial.len();
            let mut dev = Disk::new(initial);
            let mut store = MemoryStore::new();
            run(&bytes, streamed, &mut dev, None, &mut store).unwrap();
            assert_eq!(&dev.data[..new_len], &new);
            assert_eq!(&dev.data[new_len..], &tail);
            assert_eq!(dev.data.len(), capacity);
            let writes = dev.writes;
            dev.data[new_len] ^= 1;
            run(&bytes, streamed, &mut dev, None, &mut store).unwrap();
            assert_eq!(dev.writes, writes);
            dev.data[0] ^= 1;
            assert_eq!(
                run(&bytes, streamed, &mut dev, None, &mut store)
                    .unwrap_err()
                    .code,
                Code::hash_mismatch
            );
        }
    }
}

#[test]
fn independent_ab_source_can_be_larger_or_smaller_than_the_destination() {
    for (old_len, new_len) in [(19, 7), (7, 19)] {
        let old: Vec<u8> = (0..old_len).map(|i| i as u8 + 1).collect();
        let new = vec![old[old_len - 1]; new_len];
        let copy = Operation {
            r#type: TYPE_SOURCECOPY,
            src_extents: vec![ext(old_len - 1, 1); new_len],
            dst_extents: vec![ext(0, new_len)],
            src_sha256_hash: Some(sha256::hash_bytes(&new).to_vec()),
            ..Default::default()
        };
        let bytes = payload(&old, &new, vec![copy], &[]);
        let image = Image(old);
        for streamed in [false, true] {
            let mut dev = Disk::new(vec![0xe7; new_len]);
            run(
                &bytes,
                streamed,
                &mut dev,
                Some(&image),
                &mut MemoryStore::new(),
            )
            .unwrap();
            assert_eq!(dev.data, new);
            let mut wrong = Image(image.0.clone());
            wrong.0[0] ^= 1;
            let mut dev = Disk::new(vec![0xe7; new_len]);
            assert_eq!(
                run(
                    &bytes,
                    streamed,
                    &mut dev,
                    Some(&wrong),
                    &mut MemoryStore::new()
                )
                .unwrap_err()
                .code,
                Code::source_mismatch
            );
            assert_eq!(dev.writes, 0);
        }
        let mut sink = Disk::new(vec![0xe7; new_len + 5]);
        prepare(&bytes, Trust::Unverified, &EMBEDDED)
            .unwrap()
            .apply_to_sink_with_image(
                &mut sink,
                &image,
                &mut MemoryStore::new(),
                &TARGET,
                &NoProgress {},
            )
            .unwrap();
        assert_eq!(&sink.data[..new_len], &new);
        assert_eq!(&sink.data[new_len..], &[0xe7; 5]);
    }
}

#[test]
fn insufficient_new_capacity_fails_before_writes() {
    let old = vec![1; 19];
    let new = vec![2; 7];
    let bytes = payload(&old, &new, vec![replace(&new)], &new);
    for streamed in [false, true] {


        for capacity in [6] {
            let mut dev = Disk::new(vec![1; capacity]);
            assert_eq!(
                run(&bytes, streamed, &mut dev, None, &mut MemoryStore::new())
                    .unwrap_err()
                    .code,
                Code::partition_mismatch
            );
            assert_eq!(dev.writes, 0);
        }


        let mut dev = Disk::new(vec![9; 7]);
        run(
            &bytes,
            streamed,
            &mut dev,
            Some(&Image(old.clone())),
            &mut MemoryStore::new(),
        )
        .unwrap();
        assert_eq!(dev.data, new);
    }
}

#[test]
fn untouched_and_sparse_kept_bytes_are_target_bytes_not_implicit_source_copies() {
    let old = vec![3; 7];
    let mut new = old.clone();
    new.extend([0; 5]);
    let zero = Operation {
        r#type: TYPE_ZERO,
        dst_extents: vec![ext(7, 5)],
        ..Default::default()
    };
    let keep = Operation {
        r#type: TYPE_REPLACE_SPARSE,
        dst_extents: vec![ext(0, 7)],
        frame_table: Some(vec![0]),
        data_sha256_hash: Some(sha256::hash_bytes(&[]).to_vec()),
        ..Default::default()
    };
    for ops in [vec![zero.clone()], vec![keep, zero]] {
        let bytes = payload(&old, &new, ops, &[]);
        for streamed in [false, true] {
            let mut dev = Disk::new(vec![3; 12]);
            run(
                &bytes,
                streamed,
                &mut dev,
                Some(&Image(old.clone())),
                &mut MemoryStore::new(),
            )
            .unwrap();
            assert_eq!(dev.data, new);
            let mut dev = Disk::new(vec![9; 12]);
            assert_eq!(
                run(
                    &bytes,
                    streamed,
                    &mut dev,
                    Some(&Image(old.clone())),
                    &mut MemoryStore::new()
                )
                .unwrap_err()
                .code,
                Code::hash_mismatch
            );
            assert_eq!(&dev.data[..7], &[9; 7]);
        }
    }
}

#[test]
fn interrupted_grow_and_shrink_resume_without_rechecking_the_modified_old_prefix() {
    for (old_len, new_len) in [(BS + 7, 2 * BS + 13), (2 * BS + 13, BS + 7)] {
        let old = vec![0x31; old_len];
        let new = vec![0x72; new_len];
        let bytes = payload(&old, &new, vec![replace(&new)], &new);
        for streamed in [false, true] {
            let mut start = vec![0xcd; old_len.max(new_len) + 11];
            start[..old_len].copy_from_slice(&old);
            let tail = start[new_len..].to_vec();
            let mut dev = Flaky::new(start, 1);
            let mut store = MemoryStore::new();
            assert!(run(&bytes, streamed, &mut dev, None, &mut store).is_err());
            assert_eq!(dev.disk.writes, 1);
            dev.fail_at = None;
            run(&bytes, streamed, &mut dev, None, &mut store).unwrap();
            assert_eq!(&dev.disk.data[..new_len], &new);
            assert_eq!(&dev.disk.data[new_len..], &tail);
        }
    }
}

#[test]
fn prepared_partition_label_is_available_before_binding_storage() {
    let name = "logical/root-β";
    let bytes = build_full_payload(b"new image", name.as_bytes(), PRIVATE, 16, &EMBEDDED).unwrap();
    let keys = keys();
    for trust in [Trust::Verified(&keys), Trust::Unverified] {
        let buffered = prepare(&bytes, trust, &EMBEDDED).unwrap();
        assert_eq!(buffered.partition_name(), name);
        let streamed = stream::prepare(Chunked::at(&bytes, 0, 3), trust, &EMBEDDED).unwrap();
        assert_eq!(streamed.partition_name(), name);
    }
}

#[test]
fn signed_generated_grow_shrink_and_full_payloads_apply_on_larger_storage() {
    let config = SearchConfig {
        device: DevicePreference::Software,
        max_generations: Some(3),
        ..SearchConfig::quick()
    };
    for (old_len, new_len) in [(71, 137), (137, 71)] {
        let old = vec![0x31; old_len];
        let mut new = old[..old_len.min(new_len)].to_vec();
        new.resize(new_len, 0);
        new[0] = 0x56;
        let delta =
            build_payload_with(&old, &new, b"root", PRIVATE, 16, &EMBEDDED, &config).unwrap();
        let full = build_full_payload(&new, b"root", PRIVATE, 16, &EMBEDDED).unwrap();
        for bytes in [&delta, &full] {
            for streamed in [false, true] {
                let mut start = vec![0xab; old_len.max(new_len) + 5];
                start[..old_len].copy_from_slice(&old);
                let tail = start[new_len..].to_vec();
                let mut dev = Disk::new(start);
                let mut session = Session::new();
                if streamed {
                    session.streamed(bytes, 7, &mut dev)
                } else {
                    session.buffered(bytes, &mut dev)
                }
                .unwrap();
                assert_eq!(&dev.data[..new_len], &new);
                assert_eq!(&dev.data[new_len..], &tail);
            }
        }
    }
}
