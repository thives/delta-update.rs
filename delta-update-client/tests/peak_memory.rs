mod common;

use common::Counting;
use common::Window;
use common::signatures::Entry;
use common::signatures::encode;
use common::signatures::encode_without_sizes;
use common::signatures::pad_to;
use common::signatures::rsa_allowance;
use common::signatures::sign;
use delta_update_payload::encoding::BlockDecoder;
use delta_update_payload::encoding::DECODER_STATE_BYTES;
use delta_update_payload::error::Code;
use delta_update_payload::layout::PROJECT_MINOR;
use delta_update_payload::manifest::Extent;
use delta_update_payload::manifest::Manifest;
use delta_update_payload::manifest::Operation;
use delta_update_payload::manifest::PartInfo;
use delta_update_payload::manifest::PartitionUpdate;
use delta_update_payload::manifest::TYPE_REPLACE;
use delta_update_payload::manifest::TYPE_REPLACE_FRAMES;
use delta_update_payload::manifest::TYPE_TARGET_COPY;
use delta_update_payload::manifest::decode_manifest;
use delta_update_payload::manifest::decode_signatures;
use delta_update_payload::plan::validate;
use delta_update_payload::prescan::scan_manifest;
use delta_update_payload::prescan::scan_signatures;
use delta_update_payload::profile::ALLOC_OVERHEAD;
use delta_update_payload::profile::EMBEDDED;
use delta_update_payload::profile::ResourceProfile;
use delta_update_payload::profile::SPAN_COST;
use delta_update_payload::rsa::PubKey;
use delta_update_payload::rsa::parse_privkey_pem;
use delta_update_payload::rsa::public_key_of;
use delta_update_payload::sha256;
use delta_update_payload::verify::verify_message;
use prost::Message;

#[global_allocator]
static ALLOC: Counting = Counting;

const PRIVATE: &[u8] = include_bytes!("../../delta-update-payload/tests/fixtures/private.pem");
const PRIVATE_4096: &[u8] =
    include_bytes!("../../delta-update-payload/tests/fixtures/private4096.pem");

fn unbudgeted() -> ResourceProfile {
    return ResourceProfile {
        max_decoded_bytes: usize::MAX,
        ..EMBEDDED
    };
}

#[derive(Clone, Copy)]
struct Shape {
    dst: usize,
    src: usize,
}

type Hashes = (Option<Vec<u8>>, Option<Vec<u8>>);

fn cycled(i: usize) -> Hashes {
    return match i % 3 {
        0 => (Some(vec![7; 32]), Some(vec![9; 32])),
        1 => (Some(Vec::new()), None),
        _ => (None, Some(Vec::new())),
    };
}

fn both(_: usize) -> Hashes {
    return (Some(vec![7; 32]), Some(vec![9; 32]));
}

fn manifest_of(
    shapes: &[Shape],
    hashes: fn(usize) -> Hashes,
    name_len: usize,
    patch_len: usize,
) -> Vec<u8> {
    let mut ops = Vec::new();
    for (i, s) in shapes.iter().enumerate() {
        let dst: Vec<Extent> = (0..s.dst)
            .map(|k| Extent {
                start_byte: Some(((i * 256 + k) as u64 * 2 + 1) * 4096),
                num_bytes: Some((1 + (k % 3) as u64) * 4096),
            })
            .collect();
        let src: Vec<Extent> = (0..s.src)
            .map(|k| Extent {
                start_byte: Some((k as u64) * 4096),
                num_bytes: Some((1) * 4096),
            })
            .collect();
        let (data_sha256_hash, src_sha256_hash) = hashes(i);
        ops.push(Operation {
            r#type: TYPE_REPLACE,
            data_offset: Some(0),
            data_length: Some(0),
            src_extents: src,
            dst_extents: dst,
            data_sha256_hash,
            src_sha256_hash,
            ..Default::default()
        });
    }
    let part = PartitionUpdate {
        partition_name: "p".repeat(name_len),
        operations: ops,
        ..Default::default()
    };
    let manifest = Manifest {
        block_size: Some(4096),
        minor_version: Some(4),
        partitions: vec![part],
        security_patch_level: if patch_len == 0 {
            None
        } else {
            Some("s".repeat(patch_len))
        },
        ..Default::default()
    };
    return manifest.encode_to_vec();
}

fn measure_decode(label: &str, bytes: &[u8], profile: &ResourceProfile) -> (usize, usize) {
    let usage = match scan_manifest(bytes, profile) {
        Err(e) => panic!("{}: refused by the scan: {}", label, e.msg),
        Ok(u) => u,
    };
    let window = Window::new();
    let (decoded, peak) = window.measure(|| decode_manifest(bytes, profile));
    assert!(
        decoded.is_ok(),
        "{}: prost refused a scanned manifest",
        label
    );
    drop(decoded);
    let modeled = usage.accounted();
    assert!(
        peak <= modeled,
        "{}: measured peak {} exceeds the modeled {}",
        label,
        peak,
        modeled
    );
    return (modeled, peak);
}

fn check_packed_fields() -> usize {
    let table = |start: u64| Operation {
        r#type: TYPE_REPLACE_FRAMES,
        data_offset: Some(0),
        data_length: Some(0),
        dst_extents: vec![Extent {
            start_byte: Some((start) * 4096),
            num_bytes: Some((1) * 4096),
        }],
        frame_table: Some(vec![1u8; EMBEDDED.max_frame_table_bytes]),
        data_sha256_hash: Some(vec![7; 32]),
        ..Default::default()
    };
    let encode = |ops: Vec<Operation>| {
        return Manifest {
            block_size: Some(4096),
            minor_version: Some(delta_update_payload::layout::PROJECT_MINOR),
            partitions: vec![PartitionUpdate {
                partition_name: "p".into(),
                operations: ops,
                ..Default::default()
            }],
            ..Default::default()
        }
        .encode_to_vec();
    };
    let one = encode(vec![table(1)]);
    let (modeled, measured) = measure_decode("one full frame table", &one, &EMBEDDED);
    println!(
        "packed one full frame table: modeled={} measured={}",
        modeled, measured
    );
    let two = encode(vec![table(1), table(3)]);
    match scan_manifest(&two, &EMBEDDED) {
        Ok(_) => panic!("two full frame tables fit the encoded limit"),
        Err(e) => assert_eq!(e.code, Code::manifest_too_big),
    };
    return 2;
}

fn check_codec_workspace() -> usize {
    let window = Window::new();
    let (codec, peak) = window.measure(BlockDecoder::new);
    let model = DECODER_STATE_BYTES + ALLOC_OVERHEAD;
    println!("codec state: measured={} model={}", peak, model);
    assert!(
        peak <= model,
        "decoder workspace measured {} exceeds its model {}",
        peak,
        model
    );
    drop(codec);
    return peak;
}

fn growth_grid() -> Vec<(usize, usize)> {
    let counts = [1, 4, 5, 8, 9, 16, 17, 32, 33, 64];
    let extents = [1, 4, 5, 8, 9, 16, 17, 32, 33, 64, 65, 128];
    let mut out = Vec::new();
    for &ops in &counts {
        for &ext in &extents {
            if ops * ext <= EMBEDDED.max_total_extents && ext <= EMBEDDED.max_extents_per_op {
                out.push((ops, ext));
            }
        }
    }
    return out;
}

fn check_growth_grid() -> usize {
    let mut measured = 0;
    for (ops, ext) in growth_grid() {
        let shapes = vec![Shape { dst: ext, src: 0 }; ops];
        let bytes = manifest_of(&shapes, cycled, 3, 0);
        let label = format!("{} operations x {} extents", ops, ext);
        let (modeled, peak) = measure_decode(&label, &bytes, &unbudgeted());
        println!(
            "grid  {:<28} modeled={:>6} measured={:>6}",
            label, modeled, peak
        );
        measured += 1;
    }
    return measured;
}

fn uneven_shapes() -> Vec<(&'static str, Vec<Shape>)> {
    let staircase = (0..15).map(|i| Shape { dst: i + 1, src: 0 }).collect();
    let alternating = (0..16)
        .map(|i| Shape {
            dst: if i % 2 == 0 { 1 } else { 7 },
            src: 0,
        })
        .collect();
    let source_heavy = (0..9).map(|_| Shape { dst: 1, src: 3 }).collect();
    let straddling = (0..33)
        .map(|i| Shape {
            dst: [1, 4, 5][i % 3],
            src: 0,
        })
        .collect();
    let single_heavy = vec![Shape { dst: 128, src: 0 }];
    let mixed = (0..17)
        .map(|i| Shape {
            dst: 1 + i % 4,
            src: i % 5,
        })
        .collect();
    return vec![
        ("staircase", staircase),
        ("alternating", alternating),
        ("source-heavy", source_heavy),
        ("straddling", straddling),
        ("single-heavy", single_heavy),
        ("mixed", mixed),
    ];
}

fn check_uneven_shapes() -> usize {
    let mut measured = 0;
    for (name, shapes) in uneven_shapes() {
        for &name_len in &[1usize, 7, 8, 9, 255, 256] {
            for &patch_len in &[0usize, 1, 8, 9, 256] {
                let bytes = manifest_of(&shapes, cycled, name_len, patch_len);
                let label = format!("{} name={} patch={}", name, name_len, patch_len);
                let (modeled, peak) = measure_decode(&label, &bytes, &unbudgeted());
                println!(
                    "uneven {:<40} modeled={:>6} measured={:>6}",
                    label, modeled, peak
                );
                measured += 1;
            }
        }
    }
    return measured;
}

fn admitted(ops: usize, dst: usize) -> bool {
    let shapes = vec![Shape { dst: dst, src: 0 }; ops];
    return scan_manifest(&manifest_of(&shapes, both, 3, 0), &EMBEDDED).is_ok();
}

fn largest_admitted(dst: usize) -> usize {
    let (mut lo, mut hi) = (0usize, EMBEDDED.max_operations);
    while lo < hi {
        let mid = (lo + hi + 1) / 2;
        if admitted(mid, dst) {
            lo = mid;
        } else {
            hi = mid - 1;
        }
    }
    return lo;
}

fn check_budget_boundary() {
    for dst in [1usize, 2] {
        let edge = largest_admitted(dst);
        println!(
            "budget: {} extent(s) per op, both hashes: largest admitted = {} operations",
            dst, edge
        );

        assert!(edge < EMBEDDED.max_operations);
        let over = vec![Shape { dst: dst, src: 0 }; edge + 1];
        match scan_manifest(&manifest_of(&over, both, 3, 0), &EMBEDDED) {
            Ok(_) => panic!("one past the admitted edge was accepted"),
            Err(e) => assert_eq!(e.code, Code::resource_limit),
        }

        let shapes = vec![Shape { dst: dst, src: 0 }; edge];
        let bytes = manifest_of(&shapes, both, 3, 0);
        let label = format!("admitted edge, {} extent(s) per op", dst);
        let (modeled, peak) = measure_decode(&label, &bytes, &EMBEDDED);
        println!(
            "edge  {:<28} modeled={:>6} measured={:>6}",
            label, modeled, peak
        );
    }
}

fn rsa_allowance_of(pem: &[u8]) -> usize {
    let key = match parse_privkey_pem(pem) {
        Err(e) => panic!("cannot parse key: {}", e.msg),
        Ok(k) => k,
    };
    let hash = sha256::hash_bytes(b"peak measurement");
    return rsa_allowance(&public_key_of(&key), &sign(&key, &hash), &hash);
}

fn signature_shapes() -> Vec<(&'static str, Vec<Entry>, bool)> {
    return vec![
        ("one RSA-4096 entry", vec![Entry::Valid], false),
        (
            "three full-size entries",
            vec![Entry::Junk(512), Entry::Junk(512), Entry::Valid],
            false,
        ),
        (
            "four entries, fitted",
            vec![
                Entry::Junk(480),
                Entry::Junk(480),
                Entry::Junk(480),
                Entry::Valid,
            ],
            false,
        ),
        (
            "mixed, valid entry last",
            vec![Entry::Empty, Entry::Junk(512), Entry::Valid],
            false,
        ),
        (
            "three, padded to the limit",
            vec![Entry::Junk(512), Entry::Junk(512), Entry::Valid],
            true,
        ),
    ];
}

fn check_signature_shape(label: &str, msg: &[u8], hash: &[u8], keys: &[PubKey], allowance: usize) {
    let modeled = match scan_signatures(msg, &EMBEDDED) {
        Err(e) => panic!("{}: refused by the scan: {}", label, e.msg),
        Ok(u) => u.accounted(),
    };
    let (decoded, decode_peak) = Window::new().measure(|| decode_signatures(msg, &EMBEDDED));
    assert!(
        decoded.is_ok(),
        "{}: prost refused a scanned message",
        label
    );
    drop(decoded);
    assert!(
        decode_peak <= modeled,
        "{}: decode measured {} exceeds modeled {}",
        label,
        decode_peak,
        modeled
    );
    let (verified, verify_peak) =
        Window::new().measure(|| verify_message(msg, hash, keys, &EMBEDDED));
    assert!(
        verified.is_ok(),
        "{}: a valid entry was not authenticated",
        label
    );
    assert!(
        verify_peak <= modeled + allowance,
        "{}: verification measured {} exceeds decode {} plus RSA {}",
        label,
        verify_peak,
        modeled,
        allowance
    );
    println!(
        "sig   {:<28} bytes={:>5} modeled={:>5} decode={:>5} verify={:>6} rsa={:>6}",
        label,
        msg.len(),
        modeled,
        decode_peak,
        verify_peak,
        allowance
    );
}

fn check_signature_shapes(pem: &[u8], allowance: usize) -> usize {
    let key = match parse_privkey_pem(pem) {
        Err(e) => panic!("cannot parse key: {}", e.msg),
        Ok(k) => k,
    };
    let hash = sha256::hash_bytes(b"peak measurement");
    let valid = sign(&key, &hash);
    let keys = [public_key_of(&key)];
    let mut checked = 0;
    for (label, entries, padded) in signature_shapes() {
        let mut msg = encode(&entries, &valid);
        if padded {
            pad_to(&mut msg, EMBEDDED.max_signature_message, 15);
        }
        assert!(
            msg.len() <= EMBEDDED.max_signature_message,
            "{}: over the encoded limit",
            label
        );
        check_signature_shape(label, &msg, &hash, &keys, allowance);
        checked += 1;
    }

    let entries = [
        Entry::Junk(512),
        Entry::Junk(512),
        Entry::Junk(488),
        Entry::Valid,
    ];
    let msg = encode_without_sizes(&entries, &valid);
    assert_eq!(msg.len(), EMBEDDED.max_signature_message);
    assert_eq!(scan_signatures(&msg, &EMBEDDED).unwrap().accounted(), 2_904);
    let decoded = decode_signatures(&msg, &EMBEDDED).unwrap();
    assert!(
        decoded
            .signatures
            .iter()
            .all(|entry| entry.unpadded_signature_size.is_none())
    );
    drop(decoded);
    check_signature_shape("four entries, sizes omitted", &msg, &hash, &keys, allowance);
    return checked + 1;
}

fn validatable(ops: usize, dst: usize) -> Manifest {
    let mut list = Vec::new();
    for i in 0..ops {
        let dst_extents: Vec<Extent> = (0..dst)
            .map(|k| Extent {
                start_byte: Some(((i * dst + k) * 4096) as u64),
                num_bytes: Some(4096),
            })
            .collect();
        list.push(Operation {
            r#type: TYPE_REPLACE,
            data_offset: Some((i * dst * 4096) as u64),
            data_length: Some((dst * 4096) as u64),
            dst_extents: dst_extents,
            data_sha256_hash: Some(vec![7; 32]),
            ..Default::default()
        });
    }
    let total = (ops * dst * 4096) as u64;
    return Manifest {
        block_size: Some(4096),
        minor_version: Some(PROJECT_MINOR),
        partitions: vec![PartitionUpdate {
            partition_name: "A/B".into(),
            operations: list,
            new_partition_info: Some(PartInfo {
                size: Some(total),
                hash: None,
            }),
            ..Default::default()
        }],
        ..Default::default()
    };
}

fn copy_validatable(n: usize) -> Manifest {
    let source: Vec<Extent> = (0..n)
        .map(|k| Extent {
            start_byte: Some((k * 4096) as u64),
            num_bytes: Some(4096),
        })
        .collect();
    let target: Vec<Extent> = (0..n)
        .map(|k| Extent {
            start_byte: Some(((n + k) * 4096) as u64),
            num_bytes: Some(4096),
        })
        .collect();
    let op = Operation {
        r#type: TYPE_TARGET_COPY,
        data_offset: Some(0),
        data_length: Some(0),
        src_extents: source,
        dst_extents: target,
        src_sha256_hash: Some(vec![7; 32]),
        ..Default::default()
    };
    return Manifest {
        block_size: Some(4096),
        minor_version: Some(PROJECT_MINOR),
        partitions: vec![PartitionUpdate {
            partition_name: "A/B".into(),
            operations: vec![op],
            new_partition_info: Some(PartInfo {
                size: Some((2 * n * 4096) as u64),
                hash: None,
            }),
            ..Default::default()
        }],
        ..Default::default()
    };
}

fn check_copy_workspace() -> usize {
    let shapes = [1usize, 16, 64];
    let profile = unbudgeted();
    for n in shapes {
        let bytes = copy_validatable(n).encode_to_vec();
        let manifest = match decode_manifest(&bytes, &profile) {
            Err(e) => panic!("copy {}x{}: decode refused: {}", n, n, e.msg),
            Ok(m) => m,
        };
        let window = Window::new();
        let (validated, peak) = window.measure(|| validate(manifest, &profile));
        assert!(validated.is_ok(), "copy {}x{}: validation refused", n, n);
        drop(validated);
        let workspace = 2 * n * SPAN_COST;
        assert!(
            peak >= workspace,
            "copy {}x{}: measured {} bytes, below the {}-byte workspace",
            n,
            n,
            peak,
            workspace
        );
        assert!(
            peak <= workspace + ALLOC_OVERHEAD,
            "copy {}x{}: measured {} bytes, above the {}-byte charge",
            n,
            n,
            peak,
            workspace + ALLOC_OVERHEAD
        );
    }
    return shapes.len();
}

fn check_contract_workspace() -> usize {
    let shapes = [(1usize, 1usize), (1, 2), (2, 64), (1, 128), (8, 16)];
    let profile = unbudgeted();
    for (ops, dst) in shapes {
        let total = ops * dst;
        let bytes = validatable(ops, dst).encode_to_vec();
        let manifest = match decode_manifest(&bytes, &profile) {
            Err(e) => panic!("{}x{}: decode refused: {}", ops, dst, e.msg),
            Ok(m) => m,
        };
        let window = Window::new();
        let (validated, peak) = window.measure(|| validate(manifest, &profile));
        assert!(validated.is_ok(), "{}x{}: validation refused", ops, dst);
        drop(validated);
        let workspace = total * SPAN_COST;
        assert!(
            peak >= workspace,
            "{}x{}: measured {} bytes, below the {}-byte workspace",
            ops,
            dst,
            peak,
            workspace
        );
        assert!(
            peak <= workspace + ALLOC_OVERHEAD,
            "{}x{}: measured {} bytes, above the {}-byte charge",
            ops,
            dst,
            peak,
            workspace + ALLOC_OVERHEAD
        );
    }
    return shapes.len();
}

#[test]
fn decode_and_verify_peaks_stay_below_the_model() {
    let grid = check_growth_grid();
    let uneven = check_uneven_shapes();
    check_budget_boundary();
    let packed = check_packed_fields();
    let codec = check_codec_workspace();
    let contract = check_contract_workspace();
    let copy = check_copy_workspace();
    let rsa_2048 = rsa_allowance_of(PRIVATE);
    let rsa_4096 = rsa_allowance_of(PRIVATE_4096);
    println!(
        "rsa   host-measured allowance: RSA-2048={} RSA-4096={}",
        rsa_2048, rsa_4096
    );
    let signatures = check_signature_shapes(PRIVATE_4096, rsa_4096);
    println!(
        "measured {} manifest shapes across the growth grid and uneven mixes, {} packed shapes, the codec state at {} bytes, {} contract workspaces, {} copy-check workspaces, and {} signature shapes",
        grid + uneven,
        packed,
        codec,
        contract,
        copy,
        signatures
    );
}
