
use ota_client::policy::Policy;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use ota_cli::filecheckpoint::FileCheckpoint;
use ota_cli::filedevice::FileDevice;
use ota_cli::filelock::LockDir;
use ota_cli::filesource::FileSource;
use ota_cli::ramdevice::RamDevice;
use ota_client::apply;
use ota_client::apply::NoProgress;
use ota_client::apply::Trust;
use ota_client::checkpoint::MemoryStore;
use ota_client::checkpoint::TargetId;
use ota_client::stream;
use ota_generator::SearchConfig;
use ota_generator::build_payload_with;
use ota_payload::error::Code;
use ota_payload::layout;
use ota_payload::manifest::TYPE_REPLACE;
use ota_payload::manifest::TYPE_REPLACE_FRAMES;
use ota_payload::manifest::TYPE_REPLACE_SPARSE;
use ota_payload::manifest::TYPE_SOURCECOPY;
use ota_payload::manifest::TYPE_TARGET_COPY;
use ota_payload::manifest::TYPE_ZERO;
use ota_payload::profile::EMBEDDED;
use ota_payload::rsa::PubKey;
use ota_payload::rsa::parse_pubkey_pem;
use ota_payload::verify::verify_payload;

const BS: usize = 512;
const TARGET: TargetId = [0x77; 32];
const NB: usize = 10;

fn priv_pem() -> Vec<u8> {
    let pem = include_bytes!("../../ota-payload/tests/fixtures/private.pem");
    let mut out: Vec<u8> = Vec::new();
    out.extend(pem);
    return out;
}

fn pub_pem() -> Vec<u8> {
    let pem = include_bytes!("../../ota-payload/tests/fixtures/public.pem");
    let mut out: Vec<u8> = Vec::new();
    out.extend(pem);
    return out;
}

fn one_key() -> Vec<PubKey> {
    let mut keys: Vec<PubKey> = Vec::new();
    let key = match parse_pubkey_pem(pub_pem().as_ref()) {
        Err(e) => panic!("bad public key: {}", e.msg),
        Ok(v) => v,
    };
    keys.push(key);
    return keys;
}

fn block_val(b: usize, i: usize) -> u8 {
    return ((b * 1000003) ^ (i * 262147) ^ (b * 17 + i)) as u8;
}

fn remove_device(path: String) {
    match fs::remove_file(path) {
        Err(e) => eprintln!("e2e: cannot remove test device: {}", e),
        Ok(()) => {}
    };
}

fn remove_payload(path: String) {
    match fs::remove_file(path) {
        Err(e) => eprintln!("e2e: cannot remove test payload: {}", e),
        Ok(()) => {}
    };
}

fn old_image() -> Vec<u8> {
    let mut out: Vec<u8> = Vec::new();
    let mut b = 0;
    while b < NB {
        let mut i = 0;
        while i < BS {
            out.push(block_val(b, i));
            i += 1;
        }
        b += 1;
    }
    return out;
}

fn new_image() -> Vec<u8> {
    let mut out = old_image();
    let mut i = 0;
    while i < BS {
        out[1 * BS + i] = block_val(1, i) ^ 0x2a;
        out[2 * BS + i] = 0;
        out[3 * BS + i] = block_val(3, i) ^ 0x2a;
        out[6 * BS + i] = block_val(6, i) ^ 0x2a;
        out[7 * BS + i] = 0;
        i += 1;
    }
    return out;
}

fn signed_payload() -> Vec<u8> {
    let old = old_image();
    let new = new_image();
    return match build_payload_with(&old, &new, b"A/B", &priv_pem(), BS, &EMBEDDED, &SearchConfig::quick()) {
        Err(e) => panic!("build_payload failed: {}", e.msg),
        Ok(v) => v,
    };
}

fn ram(data: Vec<u8>) -> RamDevice {
    return RamDevice::new(data);
}


#[test]
fn delta_classes_and_roundtrip() {
    let payload = signed_payload();
    let keys = one_key();
    let verified = match verify_payload(&payload, &keys, &EMBEDDED) {
        Err(e) => panic!("verify_payload failed: {}", e.msg),
        Ok(v) => v,
    };

    let pu = &verified.parsed().manifest.partitions[0];
    let mut cursor = 0;
    for op in &pu.operations {
        let known = op.r#type == TYPE_REPLACE
            || op.r#type == TYPE_REPLACE_FRAMES
            || op.r#type == TYPE_REPLACE_SPARSE
            || op.r#type == TYPE_ZERO
            || op.r#type == TYPE_SOURCECOPY
            || op.r#type == TYPE_TARGET_COPY;
        assert!(known, "unexpected operation type {}", op.r#type);
        assert!(op.data_offset.unwrap_or(0) == cursor);
        if op.r#type == TYPE_ZERO {
            assert!(op.data_length.unwrap_or(0) == 0);
        }
        cursor += op.data_length.unwrap_or(0);
    }
    assert!(cursor == pu.operations.iter().map(|o| o.data_length.unwrap_or(0)).sum::<u64>());

    let mut dev = ram(old_image());
    let nop = NoProgress {};
    let prepared = match apply::prepare(&payload, Trust::Verified(&keys), &EMBEDDED) {
        Err(e) => panic!("prepare failed: {}", e.msg),
        Ok(v) => v,
    };
    match prepared.apply_to_device(&mut dev, &mut MemoryStore::new(), &TARGET, Policy::Development, &nop) {
        Err(e) => panic!("apply_to_device failed: {}", e.msg),
        Ok(_) => {}
    };
    assert!(dev.bytes() == new_image().as_slice());
}

#[test]
fn tampered_blob_is_detected() {
    let payload = signed_payload();
    let parsed = match layout::parse_unverified(&payload, &EMBEDDED) {
        Err(e) => panic!("parse failed: {}", e.msg),
        Ok(v) => v,
    };
    let blob_off = parsed.parsed().info.blobs_start as usize + 10;
    let mut tampered = payload;
    tampered[blob_off] ^= 0x01;

    let keys = one_key();
    match verify_payload(&tampered, &keys, &EMBEDDED) {
        Ok(_) => panic!("tampered payload verified"),
        Err(e) => assert!(e.code == Code::signature_mismatch),
    }
    match apply::prepare(&tampered, Trust::Unverified, &EMBEDDED) {
        Ok(_) => panic!("tampered payload prepared"),
        Err(e) => assert!(e.code == Code::blob_mismatch),
    }
}

#[test]
fn tampered_signature_is_detected() {
    let payload = signed_payload();
    let idx = payload.len() - 40;
    let mut tampered = payload;
    tampered[idx] ^= 0x01;
    let keys = one_key();
    match verify_payload(&tampered, &keys, &EMBEDDED) {
        Ok(_) => panic!("tampered signature verified"),
        Err(e) => assert!(e.code == Code::signature_mismatch),
    }
}

#[test]
fn device_capacity_larger_than_the_new_image_is_accepted() {
    let payload = signed_payload();
    let keys = one_key();
    let mut dev = ram([old_image(), old_image()].concat());
    let nop = NoProgress {};
    let prepared = match apply::prepare(&payload, Trust::Verified(&keys), &EMBEDDED) {
        Err(e) => panic!("prepare failed: {}", e.msg),
        Ok(v) => v,
    };
    prepared
        .apply_to_device(&mut dev, &mut MemoryStore::new(), &TARGET, Policy::Development, &nop)
        .unwrap_or_else(|e| panic!("apply failed: {}", e.msg));
    let expected = [new_image(), old_image()].concat();
    assert_eq!(dev.bytes(), expected.as_slice());
}

#[test]
fn file_device_roundtrip() {
    let payload = signed_payload();
    let keys = one_key();

    let path = "e2e-file-device.img";
    match fs::write(path, old_image().as_slice()) {
        Err(e) => panic!("cannot write test device: {}", e),
        Ok(()) => {}
    }
    let mut dev = match FileDevice::open(path.to_string()) {
        Err(e) => {
            remove_device(path.to_string());
            panic!("cannot open device: {}", e);
        }
        Ok(v) => v,
    };
    let nop = NoProgress {};
    let prepared = match apply::prepare(&payload, Trust::Verified(&keys), &EMBEDDED) {
        Err(e) => {
            remove_device(path.to_string());
            panic!("prepare failed: {}", e.msg);
        }
        Ok(v) => v,
    };
    let apply_result = prepared.apply_to_device(&mut dev, &mut MemoryStore::new(), &TARGET, Policy::Development, &nop);
    match fs::read(path) {
        Err(e) => {
            remove_device(path.to_string());
            panic!("cannot read back device: {}", e);
        }
        Ok(v) => {
            remove_device(path.to_string());
            assert!(v == new_image());
        }
    }
    match apply_result {
        Err(e) => panic!("apply to file device failed: {}", e.msg),
        Ok(_) => {}
    }
}


fn write_payload_file(path: String, data: &[u8]) {
    match fs::write(path, data) {
        Err(e) => panic!("cannot write test payload: {}", e),
        Ok(()) => {}
    };
}

#[test]
fn streaming_roundtrip_on_file_bindings() {
    let payload = signed_payload();
    let payload_path = "e2e-stream-payload.bin";
    let device_path = "e2e-stream-device.img";
    write_payload_file(payload_path.to_string(), payload.as_slice());
    match fs::write(device_path, old_image().as_slice()) {
        Err(e) => panic!("cannot write test device: {}", e),
        Ok(()) => {}
    }

    let keys = one_key();
    let mut dev = match FileDevice::open(device_path.to_string()) {
        Err(e) => {
            remove_payload(payload_path.to_string());
            remove_device(device_path.to_string());
            panic!("cannot open device: {}", e);
        }
        Ok(v) => v,
    };
    let mut src = match FileSource::open(payload_path.to_string()) {
        Err(e) => {
            remove_payload(payload_path.to_string());
            remove_device(device_path.to_string());
            panic!("cannot open payload: {}", e);
        }
        Ok(v) => v,
    };
    let nop = NoProgress {};
    let first = match stream::prepare(&mut src, Trust::Verified(&keys), &EMBEDDED) {
        Err(e) => Err(e),
        Ok(prepared) => prepared.apply(
            |offset: u64| {
                src.offset = offset;
                &mut src
            },
            &mut dev,
            &mut MemoryStore::new(),
            &TARGET,
            Policy::Development,
            &nop,
        ),
    };

    let read = match fs::read(device_path) {
        Err(e) => {
            remove_payload(payload_path.to_string());
            remove_device(device_path.to_string());
            panic!("cannot read back device: {}", e);
        }
        Ok(v) => v,
    };
    remove_payload(payload_path.to_string());
    remove_device(device_path.to_string());
    match first {
        Err(e) => panic!("streaming apply failed: {}", e.msg),
        Ok(_) => {}
    }
    assert!(read == new_image());
    assert!(src.offset == (payload.len() as u64));
}

fn checkpoint_namespace(checkpoint_path: &str) -> LockDir {
    let dir = format!("{}.locks", checkpoint_path);
    fs::create_dir_all(&dir).unwrap();
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).unwrap();
    return LockDir::open(Path::new(&dir)).unwrap();
}

#[test]
fn streaming_resume_converges_after_a_lost_stream() {
    let payload = signed_payload();
    let cut_path = "e2e-stream-cut.bin";
    let device_path = "e2e-stream-resume.img";
    let checkpoint_path = "e2e-stream-resume.ckpt";
    let _ = fs::remove_file(checkpoint_path);

    let parsed = match layout::parse_unverified(&payload, &EMBEDDED) {
        Err(e) => panic!("parse failed: {}", e.msg),
        Ok(v) => v,
    };
    let blob_total = parsed.parsed().info.sig_offset_blobs as usize;
    let cut = parsed.parsed().info.blobs_start as usize + blob_total / 2;
    write_payload_file(cut_path.to_string(), &payload[0..cut]);
    match fs::write(device_path, old_image().as_slice()) {
        Err(e) => panic!("cannot write test device: {}", e),
        Ok(()) => {}
    }

    let keys = one_key();
    let nop = NoProgress {};
    let ns = checkpoint_namespace(checkpoint_path);
    let mut checkpoint = FileCheckpoint::open(&ns, checkpoint_path).unwrap();
    let first = {
        let mut dev = match FileDevice::open(device_path.to_string()) {
            Err(e) => panic!("cannot open device: {}", e),
            Ok(v) => v,
        };
        let mut src = match FileSource::open(cut_path.to_string()) {
            Err(e) => panic!("cannot open truncated payload: {}", e),
            Ok(v) => v,
        };
        match stream::prepare(&mut src, Trust::Verified(&keys), &EMBEDDED) {
            Err(e) => Err(e),
            Ok(prepared) => prepared.apply(
                |offset: u64| {
                    FileSource::open_at(cut_path.to_string(), offset).unwrap_or_else(|e| {
                        panic!("cannot open truncated payload at {}: {}", offset, e)
                    })
                },
                &mut dev,
                &mut checkpoint,
                &TARGET,
                Policy::Development,
                &nop,
            ),
        }
    };
    match first {
        Ok(_) => panic!("truncated stream accepted"),
        Err(e) => assert!(e.code == Code::parse),
    }

    let new = new_image();
    let partial = match fs::read(device_path) {
        Err(e) => panic!("cannot read back device: {}", e),
        Ok(v) => v,
    };
    assert!(partial != new, "an interrupted run must not produce the complete update");

    write_payload_file(cut_path.to_string(), payload.as_slice());
    drop(checkpoint);
    let mut checkpoint = FileCheckpoint::open(&ns, checkpoint_path).unwrap();
    let second = {
        let mut dev = match FileDevice::open(device_path.to_string()) {
            Err(e) => panic!("cannot reopen device: {}", e),
            Ok(v) => v,
        };
        let mut src = match FileSource::open(cut_path.to_string()) {
            Err(e) => panic!("cannot reopen payload: {}", e),
            Ok(v) => v,
        };
        match stream::prepare(&mut src, Trust::Verified(&keys), &EMBEDDED) {
            Err(e) => Err(e),
            Ok(prepared) => prepared.apply(
                |offset: u64| {
                    FileSource::open_at(cut_path.to_string(), offset).unwrap_or_else(|e| {
                        panic!("cannot reopen payload at {}: {}", offset, e)
                    })
                },
                &mut dev,
                &mut checkpoint,
                &TARGET,
                Policy::Development,
                &nop,
            ),
        }
    };
    let done = match fs::read(device_path) {
        Err(e) => panic!("cannot read back device: {}", e),
        Ok(v) => v,
    };
    remove_payload(cut_path.to_string());
    remove_device(device_path.to_string());
    let _ = fs::remove_file(checkpoint_path);
    let _ = fs::remove_dir_all(format!("{}.locks", checkpoint_path));
    let _ = fs::remove_dir_all(".ota-checkpoint.staging");
    match second {
        Err(e) => panic!("resumed streaming apply failed: {}", e.msg),
        Ok(_) => {}
    }
    assert!(done == new);
}
