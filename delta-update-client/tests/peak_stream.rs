mod common;

use common::Counting;
use common::Window;
use common::signatures::Entry;
use common::signatures::encode;
use common::signatures::encode_without_sizes;
use common::signatures::pad_to;
use common::signatures::rsa_allowance;
use common::signatures::sign;
use delta_update_client::apply::NoProgress;
use delta_update_client::apply::Trust;
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

fn generous() -> SearchConfig {
    return SearchConfig {
        max_search_seconds: 300.0,
        stagnation_seconds: 300.0,
        max_generations: Some(40),
        ..SearchConfig::quick()
    };
}
use delta_update_generator::build_payload_with;
use delta_update_payload::encoding::DECODER_STATE_BYTES;
use delta_update_payload::error::PayloadError;
use delta_update_payload::layout;
use delta_update_payload::manifest::Manifest;
use delta_update_payload::manifest::TYPE_REPLACE_FRAMES;
use delta_update_payload::manifest::TYPE_REPLACE_SPARSE;
use delta_update_payload::manifest::TYPE_SOURCECOPY;
use delta_update_payload::manifest::decode_manifest;
use delta_update_payload::prescan::scan_manifest;
use delta_update_payload::prescan::scan_signatures;
use delta_update_payload::profile::ALLOC_OVERHEAD;
use delta_update_payload::profile::EMBEDDED;
use delta_update_payload::rsa::PrivKey;
use delta_update_payload::rsa::PubKey;
use delta_update_payload::rsa::parse_privkey_pem;
use delta_update_payload::rsa::public_key_of;
use delta_update_payload::sha256;
use prost::Message;

#[global_allocator]
static ALLOC: Counting = Counting;

const TARGET: TargetId = [0x66; 32];

const BS: usize = 4096;
const BLOCKS: usize = 128;

const PRIVATE_4096: &[u8] =
    include_bytes!("../../delta-update-payload/tests/fixtures/private4096.pem");

const REPORTED_PREPARE_PEAK: usize = 42_400;

struct Disk {
    data: Vec<u8>,
    writes: usize,
}

impl Sink for Disk {
    fn capacity(&self) -> u64 {
        return self.data.len() as u64;
    }

    fn write_at(&mut self, offset: u64, data: &[u8]) -> Result<(), PayloadError> {
        check_range(offset, data.len(), self.capacity())?;
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
        check_range(offset, out.len(), self.capacity())?;
        let off = offset as usize;
        out.copy_from_slice(&self.data[off..(off + out.len())]);
        return Ok(());
    }
}

struct Borrowed<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Source for Borrowed<'a> {
    fn read(&mut self, out: &mut [u8]) -> Result<usize, PayloadError> {
        let n = out.len().min(64 * 1024).min(self.data.len() - self.pos);
        out[..n].copy_from_slice(&self.data[self.pos..(self.pos + n)]);
        self.pos += n;
        return Ok(n);
    }
}

struct Old {
    data: Vec<u8>,
}

impl OldImage for Old {
    fn capacity(&self) -> u64 {
        return self.data.len() as u64;
    }

    fn read_at(&self, offset: u64, out: &mut [u8]) -> Result<(), PayloadError> {
        check_range(offset, out.len(), self.capacity())?;
        let off = offset as usize;
        out.copy_from_slice(&self.data[off..(off + out.len())]);
        return Ok(());
    }
}

fn text(seed: usize) -> Vec<u8> {
    let word = format!("update-engine block {} ", seed);
    let mut out = Vec::new();
    while out.len() < BS {
        out.extend_from_slice(word.as_bytes());
    }
    out.truncate(BS);
    return out;
}

fn noise(seed: usize) -> Vec<u8> {
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

fn block(seed: usize, variant: usize) -> Vec<u8> {
    return (0..BS)
        .map(|i| (((seed * 251 + variant * 37 + i * 13) % 254) + 1) as u8)
        .collect();
}

fn image(f: impl Fn(usize) -> Vec<u8>) -> Vec<u8> {
    let mut out = Vec::with_capacity(BLOCKS * BS);
    for b in 0..BLOCKS {
        out.extend(f(b));
    }
    return out;
}

fn assemble(
    generated: &[u8],
    manifest_len: usize,
    meta: &[Entry],
    pay: &[Entry],
    key: &PrivKey,
    encode: fn(&[Entry], &[u8]) -> Vec<u8>,
) -> Vec<u8> {
    let info = match layout::parse_unverified(generated, &EMBEDDED) {
        Err(e) => panic!("generated payload does not frame: {}", e.msg),
        Ok(u) => u.into_parsed().info,
    };
    let blobs_start = info.blobs_start as usize;
    let blobs = &generated[blobs_start..blobs_start + info.sig_offset_blobs as usize];
    let body = &generated[layout::HEADER_SIZE..layout::HEADER_SIZE + info.manifest_size as usize];
    let mut decoded = match Manifest::decode(body) {
        Err(e) => panic!("generated manifest does not decode: {}", e),
        Ok(m) => m,
    };

    let placeholder = [0u8; 512];
    decoded.signatures_offset = Some(blobs.len() as u64);
    decoded.signatures_size = Some(encode(pay, &placeholder).len() as u64);
    let mut manifest = decoded.encode_to_vec();
    pad_to(&mut manifest, manifest_len, 40);
    let meta_len = encode(meta, &placeholder).len();

    let mut head = Vec::new();
    head.extend_from_slice(&layout::MAGIC);
    head.extend_from_slice(&layout::MAJOR_VERSION.to_be_bytes());
    head.extend_from_slice(&(manifest.len() as u64).to_be_bytes());
    head.extend_from_slice(&(meta_len as u32).to_be_bytes());
    head.extend_from_slice(&manifest);
    let metadata_signature = encode(meta, &sign(key, &sha256::hash_bytes(&head)));

    let mut signed = head.clone();
    signed.extend_from_slice(blobs);
    let payload_signature = encode(pay, &sign(key, &sha256::hash_bytes(&signed)));

    let mut payload = head;
    payload.extend_from_slice(&metadata_signature);
    payload.extend_from_slice(blobs);
    payload.extend_from_slice(&payload_signature);
    return payload;
}

fn run_fixture(
    label: &str,
    payload: &[u8],
    old: &[u8],
    new: &[u8],
    keys: &[PubKey],
    rsa: usize,
) -> (usize, usize) {
    let info = match layout::parse_unverified(payload, &EMBEDDED) {
        Err(e) => panic!("{}: fixture does not frame: {}", label, e.msg),
        Ok(u) => u.into_parsed().info,
    };
    let manifest_size = info.manifest_size as usize;
    let manifest_bytes = &payload[layout::HEADER_SIZE..layout::HEADER_SIZE + manifest_size];
    let decoded = match scan_manifest(manifest_bytes, &EMBEDDED) {
        Err(e) => panic!("{}: manifest refused: {}", label, e.msg),
        Ok(u) => u.accounted(),
    };
    let meta_end = info.metadata_size as usize;
    let meta_sig = &payload[meta_end..meta_end + info.meta_sig_size as usize];
    let pay_start = info.sig_msg_offset as usize;
    let pay_sig = &payload[pay_start..pay_start + info.sig_size as usize];
    let meta_model = match scan_signatures(meta_sig, &EMBEDDED) {
        Err(e) => panic!("{}: metadata signature refused: {}", label, e.msg),
        Ok(u) => u.accounted(),
    };
    let pay_model = match scan_signatures(pay_sig, &EMBEDDED) {
        Err(e) => panic!("{}: payload signature refused: {}", label, e.msg),
        Ok(u) => u.accounted(),
    };

    let raw = manifest_size + ALLOC_OVERHEAD;
    let slot = EMBEDDED.max_signature_message + ALLOC_OVERHEAD;

    let scratch = BS + ALLOC_OVERHEAD;
    let verify_metadata = raw + slot + meta_model + rsa;
    let decode = raw + slot + decoded;

    let replay_start = slot + decoded + scratch;
    let prepare_bound = verify_metadata.max(decode).max(replay_start);

    let manifest = match decode_manifest(manifest_bytes, &EMBEDDED) {
        Err(e) => panic!("{}: manifest refused: {}", label, e.msg),
        Ok(m) => m,
    };
    let has_frames = manifest.partitions[0]
        .operations
        .iter()
        .any(|op| op.r#type == TYPE_REPLACE_FRAMES || op.r#type == TYPE_REPLACE_SPARSE);
    let codec = if has_frames {
        DECODER_STATE_BYTES + ALLOC_OVERHEAD + (BS + ALLOC_OVERHEAD)
    } else {
        0
    };

    let apply_bound = (replay_start + codec).max(replay_start + pay_model + rsa);

    let src = Borrowed {
        data: payload,
        pos: 0,
    };
    let mut disk = Disk {
        data: old.to_vec(),
        writes: 0,
    };
    let image = Old { data: old.to_vec() };
    let window = Window::new();
    let (prepared, prepare_peak) =
        window.measure(|| stream::prepare(src, Trust::Verified(keys), &EMBEDDED));
    let prepared = match prepared {
        Err(e) => panic!(
            "{}: streamed prepare refused a valid payload: {}",
            label, e.msg
        ),
        Ok(p) => p,
    };
    let mut store = MemoryStore::new();
    let (applied, apply_peak) = window.measure(|| {
        prepared.apply_with_image(
            |offset| Borrowed {
                data: payload,
                pos: offset as usize,
            },
            &mut disk,
            &image,
            &mut store,
            &TARGET,
            Policy::Development,
            &NoProgress {},
        )
    });
    assert!(
        applied.is_ok(),
        "{}: streamed apply refused a valid payload",
        label
    );
    assert!(
        disk.data == new,
        "{}: streamed apply diverged from the target",
        label
    );

    println!(
        "stream {:<10} manifest={} decoded={} meta={} pay={} rsa={}",
        label, manifest_size, decoded, meta_model, pay_model, rsa
    );
    println!(
        "stream {:<10} prepare peak={} estimate={} apply peak={} estimate={} (writes={})",
        label, prepare_peak, prepare_bound, apply_peak, apply_bound, disk.writes
    );

    assert!(
        prepare_peak >= manifest_size,
        "{}: the window did not see the manifest read",
        label
    );
    assert!(
        prepare_peak <= prepare_bound,
        "{}: prepare peak {} exceeds its estimate {}",
        label,
        prepare_peak,
        prepare_bound
    );
    assert!(
        apply_peak <= apply_bound,
        "{}: apply peak {} exceeds its estimate {}",
        label,
        apply_peak,
        apply_bound
    );
    return (prepare_peak, prepare_bound);
}

#[test]
fn streamed_phase_peaks_stay_within_their_estimates() {
    let private = match parse_privkey_pem(PRIVATE_4096) {
        Err(e) => panic!("cannot parse the RSA-4096 fixture: {}", e.msg),
        Ok(k) => k,
    };
    let keys: Vec<PubKey> = vec![public_key_of(&private)];

    let hash = sha256::hash_bytes(b"peak measurement");
    let rsa = rsa_allowance(&keys[0], &sign(&private, &hash), &hash);
    println!("rsa   host-measured allowance RSA-4096={}", rsa);

    let old = image(|b| block(b, 0));
    let new = image(|b| if b % 3 == 0 { block(b, 1) } else { block(b, 0) });
    let generated = match build_payload_with(
        &old,
        &new,
        b"A/B",
        PRIVATE_4096,
        BS,
        &EMBEDDED,
        &SearchConfig::quick(),
    ) {
        Err(e) => panic!("generator refused the fixture: {}", e.msg),
        Ok(v) => v,
    };
    run_fixture("generated", &generated, &old, &new, &keys, rsa);

    let reported = assemble(
        &generated,
        16 * 1024,
        &[Entry::Junk(512), Entry::Junk(512), Entry::Valid],
        &[
            Entry::Junk(480),
            Entry::Junk(480),
            Entry::Junk(480),
            Entry::Valid,
        ],
        &private,
        encode,
    );
    let (prepare_peak, prepare_bound) = run_fixture("reported", &reported, &old, &new, &keys, rsa);
    assert!(
        prepare_bound >= REPORTED_PREPARE_PEAK,
        "the preparation estimate {} does not cover the reported peak {} (measured {})",
        prepare_bound,
        REPORTED_PREPARE_PEAK,
        prepare_peak
    );

    let entries = [
        Entry::Junk(512),
        Entry::Junk(512),
        Entry::Junk(488),
        Entry::Valid,
    ];
    let message = encode_without_sizes(&entries, &[0; 512]);
    assert_eq!(message.len(), EMBEDDED.max_signature_message);
    assert_eq!(
        scan_signatures(&message, &EMBEDDED).unwrap().accounted(),
        2_904
    );
    let boundary = assemble(
        &generated,
        16 * 1024,
        &entries,
        &entries,
        &private,
        encode_without_sizes,
    );
    run_fixture("size-limit", &boundary, &old, &new, &keys, rsa);

    let old = image_of(|b| noise(b + 500));
    let new = image_of(|b| {
        if b < 32 {
            noise(b + 500 + 64)
        } else {
            match b % 4 {
                0 => text(b),
                1 => vec![0u8; BS],
                2 => noise(b + 500),
                _ => noise(b + 900),
            }
        }
    });
    let project =
        match build_payload_with(&old, &new, b"A/B", PRIVATE_4096, BS, &EMBEDDED, &generous()) {
            Err(e) => panic!("generator refused the project fixture: {}", e.msg),
            Ok(v) => v,
        };
    let manifest = decode_manifest(
        &project[layout::HEADER_SIZE
            ..layout::HEADER_SIZE
                + layout::parse_unverified(&project, &EMBEDDED)
                    .map_err(|e| e.msg)
                    .unwrap()
                    .parsed()
                    .info
                    .manifest_size as usize],
        &EMBEDDED,
    )
    .map_err(|e| e.msg)
    .unwrap();
    let types: Vec<i32> = manifest.partitions[0]
        .operations
        .iter()
        .map(|op| op.r#type)
        .collect();
    assert!(
        types.contains(&TYPE_SOURCECOPY),
        "no copy in the project fixture"
    );
    assert!(
        types.contains(&TYPE_REPLACE_FRAMES) || types.contains(&TYPE_REPLACE_SPARSE),
        "no frame or sparse operation in the project fixture"
    );
    run_fixture("project", &project, &old, &new, &keys, rsa);
}

fn image_of(f: impl Fn(usize) -> Vec<u8>) -> Vec<u8> {
    let mut out = Vec::new();
    for b in 0..BLOCKS {
        out.extend(f(b));
    }
    return out;
}
