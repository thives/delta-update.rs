#![allow(dead_code)]

use delta_update_client::apply::NoProgress;
use delta_update_client::apply::Trust;
use delta_update_client::apply::prepare;
use delta_update_client::checkpoint::MemoryStore;
use delta_update_client::checkpoint::TargetId;
use delta_update_client::image::OldImage;
use delta_update_client::policy::Policy;
use delta_update_client::sink::Readable;
use delta_update_client::sink::Sink;
use delta_update_client::sink::check_range;
use delta_update_client::source::Source;
use delta_update_client::stream;
use delta_update_generator::SearchConfig;
use delta_update_generator::build_payload_with;
use delta_update_payload::encoding::Varints;
use delta_update_payload::error::PayloadError;
use delta_update_payload::layout;
use delta_update_payload::manifest::Extent;
use delta_update_payload::manifest::Operation;
use delta_update_payload::manifest::PartitionUpdate;
use delta_update_payload::manifest::TYPE_REPLACE;
use delta_update_payload::profile::EMBEDDED;
use delta_update_payload::profile::ResourceProfile;
use delta_update_payload::rsa::PubKey;
use delta_update_payload::rsa::parse_pubkey_pem;
use delta_update_payload::sha256;

pub const BS: usize = 4096;
pub const PRIVATE: &[u8] =
    include_bytes!("../../../delta-update-payload/tests/fixtures/private.pem");
pub const PUBLIC: &[u8] = include_bytes!("../../../delta-update-payload/tests/fixtures/public.pem");

pub struct Disk {
    pub data: Vec<u8>,
    pub writes: usize,
}

impl Disk {
    pub fn new(data: Vec<u8>) -> Disk {
        return Disk {
            data: data,
            writes: 0,
        };
    }
}

impl Sink for Disk {
    fn capacity(&self) -> u64 {
        return self.data.len() as u64;
    }

    fn write_at(&mut self, offset: u64, data: &[u8]) -> Result<(), PayloadError> {
        match check_range(offset, data.len(), self.capacity()) {
            Err(e) => return Err(e),
            Ok(()) => {}
        };
        self.writes += 1;
        let off = offset as usize;
        self.data[off..(off + data.len())].copy_from_slice(data);
        return Ok(());
    }

    fn flush(&mut self) -> Result<(), PayloadError> {
        return Ok(());
    }
}

impl Readable for Disk {
    fn read_at(&self, offset: u64, out: &mut [u8]) -> Result<(), PayloadError> {
        match check_range(offset, out.len(), self.capacity()) {
            Err(e) => return Err(e),
            Ok(()) => {}
        };
        let off = offset as usize;
        out.copy_from_slice(&self.data[off..(off + out.len())]);
        return Ok(());
    }
}

pub struct Flaky {
    pub disk: Disk,
    pub fail_at: Option<usize>,
}

impl Flaky {
    pub fn new(data: Vec<u8>, fail_at: usize) -> Flaky {
        return Flaky {
            disk: Disk::new(data),
            fail_at: Some(fail_at),
        };
    }
}

impl Sink for Flaky {
    fn capacity(&self) -> u64 {
        return self.disk.capacity();
    }

    fn write_at(&mut self, offset: u64, data: &[u8]) -> Result<(), PayloadError> {
        if self.fail_at == Some(self.disk.writes) {
            return Err(delta_update_client::sink::io_error());
        }
        return self.disk.write_at(offset, data);
    }

    fn flush(&mut self) -> Result<(), PayloadError> {
        return self.disk.flush();
    }
}

impl Readable for Flaky {
    fn read_at(&self, offset: u64, out: &mut [u8]) -> Result<(), PayloadError> {
        return self.disk.read_at(offset, out);
    }
}

pub struct Barrier {
    pub disk: Disk,
    pub fail_flush_at: Option<usize>,
    pub flushes: usize,
}

impl Barrier {
    pub fn new(data: Vec<u8>, fail_flush_at: usize) -> Barrier {
        return Barrier {
            disk: Disk::new(data),
            fail_flush_at: Some(fail_flush_at),
            flushes: 0,
        };
    }
}

impl Sink for Barrier {
    fn capacity(&self) -> u64 {
        return self.disk.capacity();
    }

    fn write_at(&mut self, offset: u64, data: &[u8]) -> Result<(), PayloadError> {
        return self.disk.write_at(offset, data);
    }

    fn flush(&mut self) -> Result<(), PayloadError> {
        let n = self.flushes;
        self.flushes += 1;
        if self.fail_flush_at == Some(n) {
            return Err(delta_update_client::sink::io_error());
        }
        return self.disk.flush();
    }
}

impl Readable for Barrier {
    fn read_at(&self, offset: u64, out: &mut [u8]) -> Result<(), PayloadError> {
        return self.disk.read_at(offset, out);
    }
}

pub struct Chunked {
    pub data: Vec<u8>,
    pub pos: usize,
    pub chunk: usize,
}

impl Chunked {
    pub fn at(data: &[u8], pos: usize, chunk: usize) -> Chunked {
        return Chunked {
            data: data.to_vec(),
            pos: pos,
            chunk: chunk,
        };
    }
}

impl Source for Chunked {
    fn read(&mut self, out: &mut [u8]) -> Result<usize, PayloadError> {
        let n = out.len().min(self.chunk).min(self.data.len() - self.pos);
        out[..n].copy_from_slice(&self.data[self.pos..(self.pos + n)]);
        self.pos += n;
        return Ok(n);
    }
}

pub fn keys() -> Vec<PubKey> {
    let mut v = Vec::new();
    v.push(parse_pubkey_pem(PUBLIC).map_err(|e| e.msg).unwrap());
    return v;
}

pub fn text(seed: usize) -> Vec<u8> {
    let word = format!("update-engine block {} ", seed);
    let mut out = Vec::new();
    while out.len() < BS {
        out.extend_from_slice(word.as_bytes());
    }
    out.truncate(BS);
    return out;
}

pub fn noise(seed: usize) -> Vec<u8> {
    let mut x = (seed as u64).wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1;
    let mut out = Vec::with_capacity(BS);
    for _ in 0..BS {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        out.push((x >> 24) as u8);
    }
    return out;
}

pub fn image(blocks: Vec<Vec<u8>>) -> Vec<u8> {
    let mut out = Vec::new();
    for b in blocks {
        out.extend(b);
    }
    return out;
}

pub fn operations(payload: &[u8]) -> Vec<delta_update_payload::manifest::Operation> {
    let parsed = layout::parse_unverified(payload, &EMBEDDED)
        .map_err(|e| e.msg)
        .unwrap();
    return parsed.parsed().manifest.partitions[0].operations.clone();
}

pub fn stored_lengths(op: &delta_update_payload::manifest::Operation) -> Vec<usize> {
    let table: &[u8] = match &op.frame_table {
        None => &[],
        Some(t) => &t[..],
    };
    let mut reader = Varints::new(table);
    let mut out = Vec::new();
    loop {
        match reader.next_entry() {
            Ok(v) => out.push(if v == 0 { BS } else { v as usize }),
            Err(_) => break,
        }
        if out.len() == (op.dst_extents[0].num_bytes.unwrap_or(0) as usize) / BS {
            break;
        }
    }
    return out;
}

pub const TARGET: TargetId = [0x5a; 32];

pub struct Session {
    pub store: MemoryStore,
}

impl Session {
    pub fn new() -> Session {
        return Session {
            store: MemoryStore::new(),
        };
    }

    pub fn buffered<D: Readable>(
        &mut self,
        payload: &[u8],
        dev: &mut D,
    ) -> Result<(), PayloadError> {
        let keys = keys();
        let prepared = match prepare(payload, Trust::Verified(&keys), &EMBEDDED) {
            Err(e) => return Err(e),
            Ok(p) => p,
        };
        return prepared
            .apply_to_device(
                dev,
                &mut self.store,
                &TARGET,
                Policy::Development,
                &NoProgress {},
            )
            .map(|_| ());
    }

    pub fn buffered_with_image<D: Readable, I: OldImage>(
        &mut self,
        payload: &[u8],
        dev: &mut D,
        image: &I,
    ) -> Result<(), PayloadError> {
        let keys = keys();
        let prepared = match prepare(payload, Trust::Verified(&keys), &EMBEDDED) {
            Err(e) => return Err(e),
            Ok(p) => p,
        };
        return prepared
            .apply_to_device_with_image(
                dev,
                image,
                &mut self.store,
                &TARGET,
                Policy::Development,
                &NoProgress {},
            )
            .map(|_| ());
    }

    pub fn streamed<D: Readable>(
        &mut self,
        payload: &[u8],
        chunk: usize,
        dev: &mut D,
    ) -> Result<(), PayloadError> {
        let keys = keys();
        let head = Chunked::at(payload, 0, chunk);
        let prepared = match stream::prepare(head, Trust::Verified(&keys), &EMBEDDED) {
            Err(e) => return Err(e),
            Ok(p) => p,
        };
        let open = |offset: u64| Chunked::at(payload, offset as usize, chunk);
        return prepared
            .apply(
                open,
                dev,
                &mut self.store,
                &TARGET,
                Policy::Development,
                &NoProgress {},
            )
            .map(|_| ());
    }

    pub fn streamed_with_image<D: Readable, I: OldImage>(
        &mut self,
        payload: &[u8],
        chunk: usize,
        dev: &mut D,
        image: &I,
    ) -> Result<(), PayloadError> {
        let keys = keys();
        let head = Chunked::at(payload, 0, chunk);
        let prepared = match stream::prepare(head, Trust::Verified(&keys), &EMBEDDED) {
            Err(e) => return Err(e),
            Ok(p) => p,
        };
        let open = |offset: u64| Chunked::at(payload, offset as usize, chunk);
        return prepared
            .apply_with_image(
                open,
                dev,
                image,
                &mut self.store,
                &TARGET,
                Policy::Development,
                &NoProgress {},
            )
            .map(|_| ());
    }
}

pub fn apply_buffered(payload: &[u8], disk: &mut Disk) -> Result<(), PayloadError> {
    return Session::new().buffered(payload, disk);
}

pub fn apply_streamed(payload: &[u8], chunk: usize, disk: &mut Disk) -> Result<(), PayloadError> {
    return Session::new().streamed(payload, chunk, disk);
}

pub mod model;

pub fn blocks(start_block: u64, num_blocks: u64) -> Extent {
    return Extent {
        start_byte: Some(start_block * BS as u64),
        num_bytes: Some(num_blocks * BS as u64),
    };
}

pub fn round_trip(old: &[u8], new: &[u8]) -> Vec<u8> {
    let payload = build_payload_with(
        old,
        new,
        b"A/B",
        PRIVATE,
        BS,
        &EMBEDDED,
        &SearchConfig::quick(),
    )
    .map_err(|e| e.msg)
    .unwrap();
    let mut buffered = Disk::new(old.to_vec());
    apply_buffered(&payload, &mut buffered)
        .map_err(|e| e.msg)
        .unwrap();
    assert!(
        buffered.data == new,
        "buffered path diverged from the target"
    );
    let mut streamed = Disk::new(old.to_vec());
    apply_streamed(&payload, 1000, &mut streamed)
        .map_err(|e| e.msg)
        .unwrap();
    assert!(
        streamed.data == new,
        "streamed path diverged from the target"
    );
    return payload;
}

pub fn crafted_payload(op: Operation, blob: &[u8]) -> Vec<u8> {
    let part = delta_update_payload::manifest::PartitionUpdate {
        partition_name: "A/B".into(),
        operations: vec![op],
        ..Default::default()
    };
    return crafted_partition_payload(part, blob);
}

pub fn crafted_partition_payload(
    part: delta_update_payload::manifest::PartitionUpdate,
    blob: &[u8],
) -> Vec<u8> {
    use prost::Message;
    let manifest = delta_update_payload::manifest::Manifest {
        block_size: Some(BS as u32),
        minor_version: Some(layout::PROJECT_MINOR),
        partitions: vec![part],
        ..Default::default()
    }
    .encode_to_vec();
    let mut payload = Vec::new();
    payload.extend_from_slice(b"CrAU");
    payload.extend_from_slice(&2u64.to_be_bytes());
    payload.extend_from_slice(&(manifest.len() as u64).to_be_bytes());
    payload.extend_from_slice(&0u32.to_be_bytes());
    payload.extend_from_slice(&manifest);
    payload.extend_from_slice(blob);
    return payload;
}

pub fn signed_crafted_payload(op: Operation, blob: &[u8]) -> Vec<u8> {
    let part = delta_update_payload::manifest::PartitionUpdate {
        partition_name: "A/B".into(),
        operations: vec![op],
        ..Default::default()
    };
    return signed_partition(part, blob, None);
}

pub fn signed_partition(
    part: delta_update_payload::manifest::PartitionUpdate,
    blob: &[u8],
    release: Option<u64>,
) -> Vec<u8> {
    use prost::Message;
    let key = delta_update_payload::rsa::parse_privkey_pem(PRIVATE)
        .map_err(|e| e.msg)
        .unwrap();
    let sig_size = delta_update_payload::sign::signature_message_size(key.size);
    let manifest = delta_update_payload::manifest::Manifest {
        block_size: Some(BS as u32),
        signatures_offset: Some(blob.len() as u64),
        signatures_size: Some(sig_size as u64),
        minor_version: Some(layout::PROJECT_MINOR),
        partitions: vec![part],
        release_version: release,
        ..Default::default()
    }
    .encode_to_vec();
    let mut header = Vec::new();
    header.extend_from_slice(b"CrAU");
    header.extend_from_slice(&2u64.to_be_bytes());
    header.extend_from_slice(&(manifest.len() as u64).to_be_bytes());
    header.extend_from_slice(&(sig_size as u32).to_be_bytes());
    let metadata_hash = sha256::hash_bytes(&[header.as_slice(), manifest.as_slice()].concat());
    let payload_hash = sha256::hash_bytes(&[header.as_slice(), manifest.as_slice(), blob].concat());
    let sign = |hash: &[u8]| -> Vec<u8> {
        let sig = delta_update_payload::rsa::rsa_sign(&key, hash)
            .map_err(|e| e.msg)
            .unwrap();
        return delta_update_payload::sign::build_signature_message(
            key.size,
            &sig.bytes[..sig.len],
        )
        .map_err(|e| e.msg)
        .unwrap();
    };

    let mut payload = Vec::new();
    payload.extend_from_slice(&header);
    payload.extend_from_slice(&manifest);
    payload.extend(sign(&metadata_hash));
    payload.extend_from_slice(blob);
    payload.extend(sign(&payload_hash));
    return payload;
}

pub fn coalescing_profile() -> ResourceProfile {
    return ResourceProfile {
        max_operations: 2,
        ..EMBEDDED
    };
}

pub fn copy_op(old: &[u8], dst: u64, src: u64, nb: u64) -> Operation {
    let bytes = &old[(src as usize) * BS..((src + nb) as usize) * BS];
    return Operation {
        r#type: delta_update_payload::manifest::TYPE_SOURCECOPY,
        data_offset: Some(0),
        data_length: Some(0),
        src_extents: vec![blocks(src, nb)],
        dst_extents: vec![blocks(dst, nb)],
        src_sha256_hash: Some(sha256::hash_bytes(bytes).to_vec()),
        ..Default::default()
    };
}

pub fn part_info(image: &[u8]) -> delta_update_payload::manifest::PartInfo {
    return delta_update_payload::manifest::PartInfo {
        size: Some(image.len() as u64),
        hash: Some(sha256::hash_bytes(image).to_vec()),
    };
}

pub fn old_image() -> Vec<u8> {
    return image(vec![vec![10; BS], vec![11; BS], vec![12; BS]]);
}

pub fn new_image() -> Vec<u8> {
    return image(vec![vec![20; BS], vec![21; BS], vec![12; BS]]);
}

pub struct BaseImage(pub Vec<u8>);

impl OldImage for BaseImage {
    fn capacity(self: &Self) -> u64 {
        return self.0.len() as u64;
    }

    fn read_at(self: &Self, offset: u64, out: &mut [u8]) -> Result<(), PayloadError> {
        let at = offset as usize;
        out.copy_from_slice(&self.0[at..at + out.len()]);
        return Ok(());
    }
}

pub fn extent(start: u64, n: u64) -> Extent {
    return Extent {
        start_byte: Some(start),
        num_bytes: Some(n),
    };
}

pub fn replace(start: u64, data: &[u8]) -> Operation {
    return Operation {
        r#type: TYPE_REPLACE,
        data_length: Some(data.len() as u64),
        dst_extents: vec![extent(start, data.len() as u64)],
        data_sha256_hash: Some(sha256::hash_bytes(data).to_vec()),
        ..Default::default()
    };
}

pub fn update(
    old: &[u8],
    new: &[u8],
    ops: Vec<(Operation, Vec<u8>)>,
    new_hash: bool,
) -> PartitionUpdate {
    let mut blob_at: u64 = 0;
    let mut operations = Vec::new();
    for (mut op, data) in ops {
        op.data_offset = Some(blob_at);
        blob_at += data.len() as u64;
        operations.push(op);
    }
    let mut new_info = part_info(new);
    if !new_hash {
        new_info.hash = None;
    }
    return PartitionUpdate {
        partition_name: "A/B".into(),
        old_partition_info: Some(part_info(old)),
        new_partition_info: Some(new_info),
        operations: operations,
        ..Default::default()
    };
}

pub fn blob_of(ops: &[(Operation, Vec<u8>)]) -> Vec<u8> {
    let mut blob = Vec::new();
    for (_, data) in ops {
        blob.extend_from_slice(data);
    }
    return blob;
}

pub fn base_dependent_ops() -> Vec<(Operation, Vec<u8>)> {
    let new = new_image();
    return vec![(replace(0, &new[..2 * BS]), new[..2 * BS].to_vec())];
}

pub fn independent_ops() -> Vec<(Operation, Vec<u8>)> {
    let new = new_image();
    return vec![
        (replace(0, &new[..2 * BS]), new[..2 * BS].to_vec()),
        (
            replace(2 * BS as u64, &new[2 * BS..]),
            new[2 * BS..].to_vec(),
        ),
    ];
}

pub fn payload_for(
    ops: Vec<(Operation, Vec<u8>)>,
    release: Option<u64>,
    new_hash: bool,
) -> Vec<u8> {
    let blob = blob_of(&ops);
    let part = update(&old_image(), &new_image(), ops, new_hash);
    return signed_partition(part, &blob, release);
}
