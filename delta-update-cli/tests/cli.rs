use std::fs;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::process::Output;

mod support;

use support::namespace;

use delta_update_generator::SearchConfig;
use delta_update_generator::build_full_payload;
use delta_update_generator::build_payload_with;
use delta_update_payload::profile::EMBEDDED;

const BS: usize = 8;

fn varint(out: &mut Vec<u8>, mut v: u64) {
    loop {
        let b = (v & 0x7f) as u8;
        v >>= 7;
        if v == 0 {
            out.push(b);
            return;
        }
        out.push(b | 0x80);
    }
}

fn put_varint(out: &mut Vec<u8>, field: u64, v: u64) {
    varint(out, field << 3);
    varint(out, v);
}

fn put_bytes(out: &mut Vec<u8>, field: u64, data: &[u8]) {
    varint(out, (field << 3) | 2);
    varint(out, data.len() as u64);
    out.extend_from_slice(data);
}

fn operation(typ: u64, offset: u64, length: u64, start: u64, num: u64, blob: &[u8]) -> Vec<u8> {
    let mut ext = Vec::new();
    put_varint(&mut ext, 1, start * BS as u64);
    put_varint(&mut ext, 2, num * BS as u64);
    let mut o = Vec::new();
    put_varint(&mut o, 1, typ);
    put_varint(&mut o, 2, offset);
    put_varint(&mut o, 3, length);
    put_bytes(&mut o, 6, &ext);
    if typ == 0 {
        put_bytes(&mut o, 8, &delta_update_payload::sha256::hash_bytes(blob));
    }
    o
}

fn manifest(ops: &[Vec<u8>]) -> Vec<u8> {
    let mut part = Vec::new();
    put_bytes(&mut part, 1, b"A/B");
    for op in ops {
        put_bytes(&mut part, 8, op);
    }
    let mut m = Vec::new();
    put_varint(&mut m, 3, BS as u64);
    put_varint(
        &mut m,
        12,
        delta_update_payload::layout::PROJECT_MINOR as u64,
    );
    put_bytes(&mut m, 13, &part);
    m
}

fn frame(manifest: &[u8], meta_sig_size: u32, blobs: &[u8]) -> Vec<u8> {
    let mut p = Vec::new();
    p.extend_from_slice(b"CrAU");
    p.extend_from_slice(&2u64.to_be_bytes());
    p.extend_from_slice(&(manifest.len() as u64).to_be_bytes());
    p.extend_from_slice(&meta_sig_size.to_be_bytes());
    p.extend_from_slice(manifest);
    p.extend_from_slice(blobs);
    p
}

fn scratch_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("delta-update-cli-{}-{}", std::process::id(), name));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn apply_bin() -> &'static str {
    env!("CARGO_BIN_EXE_ota-apply")
}

const TARGET_ID: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
const OTHER_TARGET_ID: &str = "fedcba9876543210fedcba9876543210fedcba9876543210fedcba9876543210";

fn with_required_options(args: &[&str]) -> Vec<String> {
    let mut all: Vec<String> = Vec::new();
    if !args.contains(&"--checkpoint") {
        let device = args[args.len() - 1];
        all.push("--checkpoint".to_string());
        all.push(format!("{}.ckpt", device));
    }
    if !args.contains(&"--target-id") {
        all.push("--target-id".to_string());
        all.push(TARGET_ID.to_string());
    }
    if !args.contains(&"--lock-dir") {
        let device = Path::new(args[args.len() - 1]);
        all.push("--lock-dir".to_string());
        all.push(
            namespace(device.parent().unwrap())
                .to_str()
                .unwrap()
                .to_string(),
        );
    }
    if !args.contains(&"-k") && !args.contains(&"--development") {
        all.push("--development".to_string());
    }
    all.extend(args.iter().map(|a| a.to_string()));
    return all;
}

fn run(args: &[&str]) -> Output {
    Command::new(apply_bin())
        .args(with_required_options(args))
        .output()
        .unwrap()
}

fn run_limited(args: &[&str]) -> Output {
    Command::new("sh")
        .arg("-c")
        .arg("ulimit -v 262144 && exec \"$0\" \"$@\"")
        .arg(apply_bin())
        .args(with_required_options(args))
        .output()
        .unwrap()
}

#[test]
fn device_too_small_for_the_update_is_rejected_by_both_paths_without_a_write() {
    let dir = scratch_dir("too-small");
    let payload = dir.join("payload.bin");
    let device = dir.join("device.img");
    let blob = vec![0x11u8; BS];
    let bytes = frame(
        &manifest(&[operation(0, 0, BS as u64, 0, 1, &blob)]),
        0,
        &blob,
    );
    fs::write(&payload, &bytes).unwrap();
    let original = vec![0x22u8; BS / 2];
    fs::write(&device, &original).unwrap();

    for extra in [&[][..], &["--stream"][..]] {
        let mut args: Vec<&str> = extra.to_vec();
        args.push(payload.to_str().unwrap());
        args.push(device.to_str().unwrap());
        let out = run(&args);
        assert!(
            !out.status.success(),
            "ota-apply {:?} wrote past the end of a device",
            extra
        );
        assert_eq!(
            fs::read(&device).unwrap(),
            original,
            "device changed during a rejected run {:?}",
            extra
        );
    }
    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn the_checkpoint_option_is_required() {
    let dir = scratch_dir("no-checkpoint");
    let payload = dir.join("payload.bin");
    let device = dir.join("device.img");
    let blob = vec![0x11u8; BS];
    fs::write(
        &payload,
        frame(
            &manifest(&[operation(0, 0, BS as u64, 0, 1, &blob)]),
            0,
            &blob,
        ),
    )
    .unwrap();
    fs::write(&device, vec![0x22u8; BS]).unwrap();
    let out = Command::new(apply_bin())
        .args(["--target-id", TARGET_ID])
        .arg(payload.to_str().unwrap())
        .arg(device.to_str().unwrap())
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    assert_eq!(fs::read(&device).unwrap(), vec![0x22u8; BS]);
    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn invalid_later_operation_leaves_device_untouched() {
    let dir = scratch_dir("late");
    let payload = dir.join("payload.bin");
    let device = dir.join("device.img");
    let blobs = [vec![0x33u8; BS], vec![0x44u8; BS]].concat();
    let ops = [
        operation(0, 0, BS as u64, 0, 1, &blobs[..BS]),
        operation(8, BS as u64, BS as u64, 1, 1, &blobs[BS..]),
    ];
    fs::write(&payload, frame(&manifest(&ops), 0, &blobs)).unwrap();
    let original = vec![0x55u8; 4 * BS];
    fs::write(&device, &original).unwrap();

    for extra in [&[][..], &["--stream"][..]] {
        let mut args: Vec<&str> = extra.to_vec();
        args.push(payload.to_str().unwrap());
        args.push(device.to_str().unwrap());
        let out = run(&args);
        assert!(
            !out.status.success(),
            "invalid payload was accepted {:?}",
            extra
        );
        assert_eq!(
            fs::read(&device).unwrap(),
            original,
            "device was written before the invalid operation was rejected {:?}",
            extra
        );
    }
    fs::remove_dir_all(&dir).unwrap();
}

const PRIVATE: &[u8] = include_bytes!("../../delta-update-payload/tests/fixtures/private.pem");
const PUBLIC_PATH: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../delta-update-payload/tests/fixtures/public.pem"
);

#[test]
fn trailing_byte_after_a_signed_payload_is_refused_with_and_without_keys() {
    let dir = scratch_dir("trailer");
    let payload = dir.join("payload.bin");
    let device = dir.join("device.img");
    let old = vec![0x11u8; 4 * BS];
    let mut new = old.clone();
    new[BS..2 * BS].fill(0x22);
    let signed = match build_payload_with(
        &old,
        &new,
        b"A/B",
        PRIVATE,
        BS,
        &EMBEDDED,
        &SearchConfig::quick(),
    ) {
        Err(e) => panic!("generator refused the fixture: {}", e.msg),
        Ok(v) => v,
    };

    fs::write(&payload, &signed).unwrap();
    fs::write(&device, &old).unwrap();
    let out = run(&[payload.to_str().unwrap(), device.to_str().unwrap()]);
    assert!(
        out.status.success(),
        "fixture refused: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(fs::read(&device).unwrap(), new);

    let mut trailing = signed.clone();
    trailing.push(0);
    fs::write(&payload, &trailing).unwrap();
    for extra in [&[][..], &["-k", PUBLIC_PATH][..]] {
        fs::write(&device, &old).unwrap();
        let mut args: Vec<&str> = extra.to_vec();
        args.push(payload.to_str().unwrap());
        args.push(device.to_str().unwrap());
        let out = run(&args);
        assert!(!out.status.success(), "trailing byte accepted {:?}", extra);
        assert_eq!(
            fs::read(&device).unwrap(),
            old,
            "device written {:?}",
            extra
        );
    }
    for extra in [&["--stream"][..], &["-k", PUBLIC_PATH, "--stream"][..]] {
        fs::write(&device, &old).unwrap();
        let mut args: Vec<&str> = extra.to_vec();
        args.push(payload.to_str().unwrap());
        args.push(device.to_str().unwrap());
        let out = run(&args);
        assert!(!out.status.success(), "trailing byte accepted {:?}", extra);
    }
    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn oversized_metadata_signature_declaration_fails_cleanly_under_memory_limit() {
    let dir = scratch_dir("oversized");
    let payload = dir.join("payload.bin");
    let device = dir.join("device.img");
    let bytes = frame(
        &manifest(&[operation(0, 0, BS as u64, 0, 1, &vec![0u8; BS])]),
        u32::MAX,
        &vec![0u8; BS],
    );
    fs::write(&payload, &bytes).unwrap();
    fs::write(&device, vec![0u8; 4 * BS]).unwrap();

    let out = run_limited(&[
        "--stream",
        payload.to_str().unwrap(),
        device.to_str().unwrap(),
    ]);
    assert_eq!(
        out.status.code(),
        Some(1),
        "expected a controlled error, got status {:?} (an allocation abort has no exit code); stderr: {}",
        out.status,
        String::from_utf8_lossy(&out.stderr)
    );
    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn oversized_manifest_declaration_fails_cleanly_under_memory_limit_on_both_paths() {
    let dir = scratch_dir("manifest-size");
    let payload = dir.join("payload.bin");
    let device = dir.join("device.img");
    let mut bytes = Vec::new();
    bytes.extend_from_slice(b"CrAU");
    bytes.extend_from_slice(&2u64.to_be_bytes());
    bytes.extend_from_slice(&(1u64 << 40).to_be_bytes());
    bytes.extend_from_slice(&0u32.to_be_bytes());
    bytes.extend_from_slice(&[0u8; 64]);
    fs::write(&payload, &bytes).unwrap();
    fs::write(&device, vec![0u8; 4 * BS]).unwrap();

    for extra in [&[][..], &["--stream"][..]] {
        let mut args: Vec<&str> = extra.to_vec();
        args.push(payload.to_str().unwrap());
        args.push(device.to_str().unwrap());
        let out = run_limited(&args);
        assert_eq!(
            out.status.code(),
            Some(1),
            "{:?}: expected a controlled error, got {:?}; stderr: {}",
            extra,
            out.status,
            String::from_utf8_lossy(&out.stderr)
        );
    }
    fs::remove_dir_all(&dir).unwrap();
}

fn copy_operation(src: u64, dst: u64, nb: u64, source_hash: &[u8]) -> Vec<u8> {
    let mut src_ext = Vec::new();
    put_varint(&mut src_ext, 1, src * BS as u64);
    put_varint(&mut src_ext, 2, nb * BS as u64);
    let mut dst_ext = Vec::new();
    put_varint(&mut dst_ext, 1, dst * BS as u64);
    put_varint(&mut dst_ext, 2, nb * BS as u64);
    let mut o = Vec::new();
    put_varint(&mut o, 1, 4);
    put_varint(&mut o, 2, 0);
    put_varint(&mut o, 3, 0);
    put_bytes(&mut o, 4, &src_ext);
    put_bytes(&mut o, 6, &dst_ext);
    put_bytes(&mut o, 9, source_hash);
    o
}

fn project_manifest(ops: &[Vec<u8>]) -> Vec<u8> {
    let mut part = Vec::new();
    put_bytes(&mut part, 1, b"A/B");
    for op in ops {
        put_bytes(&mut part, 8, op);
    }
    let mut m = Vec::new();
    put_varint(&mut m, 3, BS as u64);
    put_varint(
        &mut m,
        12,
        delta_update_payload::layout::PROJECT_MINOR as u64,
    );
    put_bytes(&mut m, 13, &part);
    return m;
}

fn swap_fixture() -> (Vec<u8>, Vec<u8>, Vec<u8>) {
    let old: Vec<u8> = (0..4 * BS)
        .map(|i| (i as u8).wrapping_mul(37).wrapping_add(11))
        .collect();
    let mut new = Vec::new();
    new.extend_from_slice(&old[(2 * BS)..]);
    new.extend_from_slice(&old[..(2 * BS)]);
    let h_lo = delta_update_payload::sha256::hash_bytes(&old[..(2 * BS)]);
    let h_hi = delta_update_payload::sha256::hash_bytes(&old[(2 * BS)..]);
    let ops = [
        copy_operation(2, 0, 2, &h_hi),
        copy_operation(0, 2, 2, &h_lo),
    ];
    let payload = frame(&project_manifest(&ops), 0, &[]);
    return (old, new, payload);
}

#[test]
fn source_copy_applies_from_a_separate_old_image_on_both_paths() {
    let dir = scratch_dir("copy-ok");
    let (old, new, payload) = swap_fixture();
    let payload_path = dir.join("payload.bin");
    let source_path = dir.join("old.img");
    fs::write(&payload_path, payload).unwrap();
    fs::write(&source_path, &old).unwrap();
    for extra in [&[][..], &["--stream"][..]] {
        let device = dir.join(format!("device-{}.img", extra.len()));
        fs::write(&device, &old).unwrap();
        let mut args: Vec<&str> = extra.to_vec();
        args.extend_from_slice(&["-s", source_path.to_str().unwrap()]);
        args.push(payload_path.to_str().unwrap());
        args.push(device.to_str().unwrap());
        let out = run(&args);
        assert!(
            out.status.success(),
            "copy refused {:?}: {}",
            extra,
            String::from_utf8_lossy(&out.stderr)
        );
        assert_eq!(
            fs::read(&device).unwrap(),
            new,
            "device diverged {:?}",
            extra
        );
    }
    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn source_copy_without_an_old_image_is_refused_with_the_device_untouched() {
    let dir = scratch_dir("copy-missing");
    let (old, _new, payload) = swap_fixture();
    let payload_path = dir.join("payload.bin");
    let device = dir.join("device.img");
    fs::write(&payload_path, payload).unwrap();
    fs::write(&device, &old).unwrap();
    for extra in [&[][..], &["--stream"][..]] {
        let mut args: Vec<&str> = extra.to_vec();
        args.push(payload_path.to_str().unwrap());
        args.push(device.to_str().unwrap());
        let out = run(&args);
        assert!(
            !out.status.success(),
            "copy accepted without an image {:?}",
            extra
        );
        assert!(String::from_utf8_lossy(&out.stderr).contains("old image"));
        assert_eq!(
            fs::read(&device).unwrap(),
            old,
            "device written {:?}",
            extra
        );
    }
    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn an_old_image_that_is_the_device_is_refused_by_name_and_by_symlink() {
    let dir = scratch_dir("copy-alias");
    let (old, _new, payload) = swap_fixture();
    let payload_path = dir.join("payload.bin");
    let device = dir.join("device.img");
    let link = dir.join("link.img");
    fs::write(&payload_path, payload).unwrap();
    fs::write(&device, &old).unwrap();
    std::os::unix::fs::symlink(&device, &link).unwrap();
    for source in [&device, &link] {
        let out = run(&[
            "-s",
            source.to_str().unwrap(),
            payload_path.to_str().unwrap(),
            device.to_str().unwrap(),
        ]);
        assert!(!out.status.success());
        let stderr = String::from_utf8_lossy(&out.stderr);
        if source == &device {
            assert!(stderr.contains("same file"), "{}", stderr);
        } else {
            assert!(
                stderr.contains("Too many levels of symbolic links"),
                "{}",
                stderr
            );
        }
        assert_eq!(fs::read(&device).unwrap(), old);
    }
    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn the_target_id_is_required_and_malformed_values_are_usage_errors() {
    let dir = scratch_dir("target-id");
    let payload = dir.join("payload.bin");
    let device = dir.join("device.img");
    let blob = vec![0x11u8; BS];
    fs::write(
        &payload,
        frame(
            &manifest(&[operation(0, 0, BS as u64, 0, 1, &blob)]),
            0,
            &blob,
        ),
    )
    .unwrap();
    fs::write(&device, vec![0x22u8; BS]).unwrap();
    let payload_arg = payload.to_str().unwrap();
    let device_arg = device.to_str().unwrap();
    let checkpoint = dir.join("device.ckpt");
    let checkpoint_arg = checkpoint.to_str().unwrap();

    let missing = Command::new(apply_bin())
        .args(["--checkpoint", checkpoint_arg, payload_arg, device_arg])
        .output()
        .unwrap();
    assert_eq!(
        missing.status.code(),
        Some(2),
        "a run without --target-id was accepted"
    );

    let malformed = [
        String::new(),
        "0123".to_string(),
        format!("{}0", TARGET_ID),
        format!("0x{}", &TARGET_ID[2..]),
        format!("g{}", &TARGET_ID[1..]),
        "é".repeat(32),
    ];
    for bad in &malformed {
        let out = run(&["--target-id", bad.as_str(), payload_arg, device_arg]);
        assert_eq!(out.status.code(), Some(2), "accepted --target-id {:?}", bad);
        assert!(String::from_utf8_lossy(&out.stderr).contains("--target-id"));
    }
    assert_eq!(fs::read(&device).unwrap(), vec![0x22u8; BS]);
    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn an_interrupted_stream_resumes_under_its_original_target_id_only() {
    let dir = scratch_dir("target-resume");
    let payload = dir.join("payload.bin");
    let cut = dir.join("cut.bin");
    let device = dir.join("device.img");
    let checkpoint = dir.join("device.ckpt");
    let blobs = [vec![0x33u8; BS], vec![0x44u8; BS]].concat();
    let ops = [
        operation(0, 0, BS as u64, 0, 1, &blobs[..BS]),
        operation(0, BS as u64, BS as u64, 1, 1, &blobs[BS..]),
    ];
    let whole = frame(&manifest(&ops), 0, &blobs);
    fs::write(&payload, &whole).unwrap();
    let header = whole.len() - blobs.len();
    fs::write(&cut, &whole[..header + BS]).unwrap();
    let original = vec![0x55u8; 4 * BS];
    fs::write(&device, &original).unwrap();
    let payload_arg = payload.to_str().unwrap();
    let cut_arg = cut.to_str().unwrap();
    let device_arg = device.to_str().unwrap();
    let checkpoint_arg = checkpoint.to_str().unwrap();

    let first = run_stream_with(TARGET_ID, checkpoint_arg, cut_arg, device_arg);
    assert_eq!(first.status.code(), Some(1), "the cut stream was accepted");
    let mut partial = original.clone();
    partial[..BS].copy_from_slice(&blobs[..BS]);
    assert_eq!(fs::read(&device).unwrap(), partial);

    let other = run_stream_with(OTHER_TARGET_ID, checkpoint_arg, payload_arg, device_arg);
    assert_eq!(other.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&other.stderr).contains("another target"));
    assert_eq!(fs::read(&device).unwrap(), partial);

    let resumed = run_stream_with(
        &TARGET_ID.to_uppercase(),
        checkpoint_arg,
        payload_arg,
        device_arg,
    );
    assert!(
        resumed.status.success(),
        "resume refused: {}",
        String::from_utf8_lossy(&resumed.stderr)
    );
    let mut done = original;
    done[..2 * BS].copy_from_slice(&blobs);
    assert_eq!(fs::read(&device).unwrap(), done);
    fs::remove_dir_all(&dir).unwrap();
}

fn run_stream_with(target: &str, checkpoint: &str, payload: &str, device: &str) -> Output {
    let lock_dir = namespace(Path::new(device).parent().unwrap());
    return Command::new(apply_bin())
        .args([
            "--stream",
            "--development",
            "--lock-dir",
            lock_dir.to_str().unwrap(),
            "--target-id",
            target,
            "--checkpoint",
            checkpoint,
            payload,
            device,
        ])
        .output()
        .unwrap();
}

#[test]
fn a_sparse_checkpoint_of_any_size_fails_cleanly_under_the_memory_limit() {
    let dir = scratch_dir("sparse-checkpoint");
    let payload = dir.join("payload.bin");
    let device = dir.join("device.img");
    let checkpoint = dir.join("device.ckpt");
    let blob = vec![0x11u8; BS];
    fs::write(
        &payload,
        frame(
            &manifest(&[operation(0, 0, BS as u64, 0, 1, &blob)]),
            0,
            &blob,
        ),
    )
    .unwrap();
    fs::write(&device, vec![0x22u8; BS]).unwrap();
    fs::File::create(&checkpoint)
        .unwrap()
        .set_len(1 << 40)
        .unwrap();

    for extra in [&[][..], &["--stream"][..]] {
        let mut args: Vec<&str> = vec!["--checkpoint", checkpoint.to_str().unwrap()];
        args.extend_from_slice(extra);
        args.push(payload.to_str().unwrap());
        args.push(device.to_str().unwrap());
        let out = run_limited(&args);
        assert_eq!(
            out.status.code(),
            Some(1),
            "{:?}: expected a controlled error, got {:?}; stderr: {}",
            extra,
            out.status,
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(String::from_utf8_lossy(&out.stderr).contains("wrong length"));
        assert_eq!(
            fs::read(&device).unwrap(),
            vec![0x22u8; BS],
            "device written {:?}",
            extra
        );
    }
    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn a_completed_update_is_checked_again_and_refused_once_its_target_has_changed() {
    let dir = scratch_dir("complete-altered");
    let payload = dir.join("payload.bin");
    let old = vec![0x11u8; 4 * BS];
    let mut new = old.clone();
    new[BS..2 * BS].fill(0x22);
    let signed = match build_payload_with(
        &old,
        &new,
        b"A/B",
        PRIVATE,
        BS,
        &EMBEDDED,
        &SearchConfig::quick(),
    ) {
        Err(e) => panic!("generator refused the fixture: {}", e.msg),
        Ok(v) => v,
    };
    fs::write(&payload, &signed).unwrap();
    let payload_arg = payload.to_str().unwrap();

    for extra in [&[][..], &["--stream"][..]] {
        let device = dir.join(format!("device-{}.img", extra.len()));
        let device_arg = device.to_str().unwrap();
        let apply = |device: &str| {
            let mut args: Vec<&str> = extra.to_vec();
            args.push(payload_arg);
            args.push(device);
            run(&args)
        };
        fs::write(&device, &old).unwrap();
        let first = apply(device_arg);
        assert!(
            first.status.success(),
            "{:?}: {}",
            extra,
            String::from_utf8_lossy(&first.stderr)
        );
        assert_eq!(fs::read(&device).unwrap(), new);

        let again = apply(device_arg);
        assert!(
            again.status.success(),
            "{:?}: {}",
            extra,
            String::from_utf8_lossy(&again.stderr)
        );
        assert_eq!(fs::read(&device).unwrap(), new);

        let mut altered = new.clone();
        altered[0] ^= 0xff;
        fs::write(&device, &altered).unwrap();
        let refused = apply(device_arg);
        assert_eq!(
            refused.status.code(),
            Some(1),
            "{:?}: altered target accepted",
            extra
        );
        assert!(String::from_utf8_lossy(&refused.stderr).contains("new partition hash"));
        assert_eq!(
            fs::read(&device).unwrap(),
            altered,
            "refused re-run wrote {:?}",
            extra
        );
    }
    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn a_full_image_payload_applies_with_no_old_image_on_both_paths() {
    let dir = scratch_dir("full-image-apply");
    let new: Vec<u8> = (0..9000u32).map(|k| (k % 97) as u8).collect();
    let private = include_bytes!("../../delta-update-payload/tests/fixtures/private.pem");
    let payload = build_full_payload(&new, b"A/B", private, 4096, &EMBEDDED)
        .unwrap_or_else(|e| panic!("{}", e.msg));
    let payload_path = dir.join("full.payload");
    fs::write(&payload_path, &payload).unwrap();
    let public = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../delta-update-payload/tests/fixtures/public.pem"
    );
    let payload_str = payload_path.to_str().unwrap();

    for stream in [false, true] {
        let device = dir.join(if stream {
            "streamed.img"
        } else {
            "buffered.img"
        });
        let unrelated: Vec<u8> = (0..new.len()).map(|i| (i * 131 + 7) as u8).collect();
        fs::write(&device, &unrelated).unwrap();
        let device_str = device.to_str().unwrap();
        let out = if stream {
            let checkpoint = format!("{}.ckpt", device_str);
            let lock_dir = namespace(device.parent().unwrap());
            Command::new(apply_bin())
                .args([
                    "--stream",
                    "-k",
                    public,
                    "--lock-dir",
                    lock_dir.to_str().unwrap(),
                    "--target-id",
                    TARGET_ID,
                    "--checkpoint",
                    &checkpoint,
                    payload_str,
                    device_str,
                ])
                .output()
                .unwrap()
        } else {
            run(&["-k", public, payload_str, device_str])
        };
        assert!(
            out.status.success(),
            "stream={} exit {:?}: {}",
            stream,
            out.status.code(),
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(
            fs::read(&device).unwrap() == new,
            "stream={}: the device is not the new image",
            stream
        );
    }
    fs::remove_dir_all(&dir).unwrap();
}
