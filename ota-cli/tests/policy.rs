
use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::process::Output;

mod support;

use ota_generator::build_full_payload_with_release;
use ota_payload::profile::EMBEDDED;

use support::hold_device_lock;
use support::namespace;

const PRIVATE: &[u8] = include_bytes!("../../ota-payload/tests/fixtures/private.pem");
const PUBLIC_PATH: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../ota-payload/tests/fixtures/public.pem"
);
const TARGET_ID: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

fn apply_bin() -> &'static str {
    return env!("CARGO_BIN_EXE_ota-apply");
}

fn scratch_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("ota-policy-{}-{}", std::process::id(), name));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    return dir;
}

fn new_image() -> Vec<u8> {
    return (0..4096u32 * 3).map(|i| (i * 7 % 251) as u8).collect();
}

fn full_payload(release: u64) -> Vec<u8> {
    return match build_full_payload_with_release(
        &new_image(),
        b"A/B",
        PRIVATE,
        4096,
        &EMBEDDED,
        release,
    ) {
        Err(e) => panic!("build failed: {}", e.msg),
        Ok(v) => v,
    };
}

fn apply(args: &[&str]) -> Output {
    return Command::new(apply_bin()).args(args).output().unwrap();
}

#[test]
fn strict_is_the_default_and_refuses_a_run_with_no_key() {
    let dir = scratch_dir("no-key");
    let lock_dir = namespace(&dir);
    let payload = dir.join("p.bin");
    let device = dir.join("dev.img");
    fs::write(&payload, full_payload(0)).unwrap();
    fs::write(&device, vec![0u8; new_image().len()]).unwrap();
    let out = apply(&[
        "--lock-dir",
        lock_dir.to_str().unwrap(),
        "--checkpoint",
        dir.join("ck").to_str().unwrap(),
        "--target-id",
        TARGET_ID,
        payload.to_str().unwrap(),
        device.to_str().unwrap(),
    ]);
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stderr).contains("--key"));
    assert_eq!(fs::read(&device).unwrap(), vec![0u8; new_image().len()]);
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn a_verified_run_reports_verified_and_the_development_run_reports_applied() {
    let dir = scratch_dir("verified");
    let lock_dir = namespace(&dir);
    let payload = dir.join("p.bin");
    fs::write(&payload, full_payload(0)).unwrap();
    let device = dir.join("dev.img");
    fs::write(&device, vec![0u8; new_image().len()]).unwrap();
    let out = apply(&[
        "-k",
        PUBLIC_PATH,
        "--lock-dir",
        lock_dir.to_str().unwrap(),
        "--checkpoint",
        dir.join("ck1").to_str().unwrap(),
        "--target-id",
        TARGET_ID,
        payload.to_str().unwrap(),
        device.to_str().unwrap(),
    ]);
    let err = String::from_utf8_lossy(&out.stderr).to_string();
    assert!(out.status.success(), "{err}");
    assert!(err.contains("verified against the signed new image"), "{err}");
    assert_eq!(fs::read(&device).unwrap(), new_image());

    let device2 = dir.join("dev2.img");
    fs::write(&device2, vec![0u8; new_image().len()]).unwrap();
    let out = apply(&[
        "--development",
        "--lock-dir",
        lock_dir.to_str().unwrap(),
        "--checkpoint",
        dir.join("ck2").to_str().unwrap(),
        "--target-id",
        TARGET_ID,
        payload.to_str().unwrap(),
        device2.to_str().unwrap(),
    ]);
    let err = String::from_utf8_lossy(&out.stderr).to_string();
    assert!(out.status.success(), "{err}");
    assert!(err.contains("NOT verified"), "{err}");
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn a_release_below_the_floor_is_refused_before_any_write() {
    let dir = scratch_dir("floor");
    let lock_dir = namespace(&dir);
    let payload = dir.join("p.bin");
    fs::write(&payload, full_payload(3)).unwrap();
    let device = dir.join("dev.img");
    fs::write(&device, vec![0u8; new_image().len()]).unwrap();
    let checkpoint = dir.join("ck");
    let out = apply(&[
        "-k",
        PUBLIC_PATH,
        "--release-floor",
        "4",
        "--lock-dir",
        lock_dir.to_str().unwrap(),
        "--checkpoint",
        checkpoint.to_str().unwrap(),
        "--target-id",
        TARGET_ID,
        payload.to_str().unwrap(),
        device.to_str().unwrap(),
    ]);
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("rollback floor"));
    assert_eq!(fs::read(&device).unwrap(), vec![0u8; new_image().len()]);
    assert!(!checkpoint.exists());
    fs::remove_dir_all(dir).unwrap();
}

fn verified_args<'a>(lock_dir: &'a str, checkpoint: &'a str, payload: &'a str, device: &'a str) -> Vec<&'a str> {
    return vec![
        "-k",
        PUBLIC_PATH,
        "--lock-dir",
        lock_dir,
        "--checkpoint",
        checkpoint,
        "--target-id",
        TARGET_ID,
        payload,
        device,
    ];
}

#[test]
fn a_device_held_by_another_process_refuses_every_checkpoint_name_and_leaves_it_untouched() {
    let dir = scratch_dir("lock");
    let lock_dir = namespace(&dir);
    let payload = dir.join("p.bin");
    fs::write(&payload, full_payload(0)).unwrap();
    let device = dir.join("dev.img");
    fs::write(&device, vec![0u8; new_image().len()]).unwrap();
    let held = hold_device_lock(&device, &dir);
    for name in ["ck", "other-ck"] {
        let checkpoint = dir.join(name);
        let out = apply(&verified_args(
            lock_dir.to_str().unwrap(),
            checkpoint.to_str().unwrap(),
            payload.to_str().unwrap(),
            device.to_str().unwrap(),
        ));
        assert_eq!(out.status.code(), Some(1), "{name}");
        assert!(String::from_utf8_lossy(&out.stderr).contains("another update holds"), "{name}");
        assert!(!checkpoint.exists(), "{name} was created while the device was held");
    }
    assert_eq!(fs::read(&device).unwrap(), vec![0u8; new_image().len()]);
    drop(held);
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn a_hard_link_to_a_locked_device_shares_its_lock() {
    let dir = scratch_dir("lock-link");
    let lock_dir = namespace(&dir);
    let payload = dir.join("p.bin");
    fs::write(&payload, full_payload(0)).unwrap();
    let device = dir.join("dev.img");
    fs::write(&device, vec![0u8; new_image().len()]).unwrap();
    let link = dir.join("dev-link.img");
    fs::hard_link(&device, &link).unwrap();
    let held = hold_device_lock(&device, &dir);
    let out = apply(&verified_args(
        lock_dir.to_str().unwrap(),
        dir.join("ck").to_str().unwrap(),
        payload.to_str().unwrap(),
        link.to_str().unwrap(),
    ));
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("another update holds"));
    assert_eq!(fs::read(&device).unwrap(), vec![0u8; new_image().len()]);
    drop(held);
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn a_different_device_is_not_blocked_by_another_devices_lock() {
    let dir = scratch_dir("lock-separate");
    let lock_dir = namespace(&dir);
    let payload = dir.join("p.bin");
    fs::write(&payload, full_payload(0)).unwrap();
    let locked = dir.join("locked.img");
    fs::write(&locked, vec![0u8; new_image().len()]).unwrap();
    let free = dir.join("free.img");
    fs::write(&free, vec![0u8; new_image().len()]).unwrap();
    let held = hold_device_lock(&locked, &dir);
    let out = apply(&verified_args(
        lock_dir.to_str().unwrap(),
        dir.join("ck").to_str().unwrap(),
        payload.to_str().unwrap(),
        free.to_str().unwrap(),
    ));
    let err = String::from_utf8_lossy(&out.stderr).to_string();
    assert!(out.status.success(), "{err}");
    assert_eq!(fs::read(&free).unwrap(), new_image());
    drop(held);
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn a_checkpoint_that_is_a_symbolic_link_is_refused_and_its_target_is_untouched() {
    let dir = scratch_dir("ck-symlink");
    let lock_dir = namespace(&dir);
    let payload = dir.join("p.bin");
    fs::write(&payload, full_payload(0)).unwrap();
    let device = dir.join("dev.img");
    fs::write(&device, vec![0u8; new_image().len()]).unwrap();
    let victim = dir.join("victim");
    fs::write(&victim, b"another record").unwrap();
    let checkpoint = dir.join("ck");
    std::os::unix::fs::symlink(&victim, &checkpoint).unwrap();
    let out = apply(&verified_args(
        lock_dir.to_str().unwrap(),
        checkpoint.to_str().unwrap(),
        payload.to_str().unwrap(),
        device.to_str().unwrap(),
    ));
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("symbolic link"));
    assert_eq!(fs::read(&victim).unwrap(), b"another record");
    assert_eq!(fs::read(&device).unwrap(), vec![0u8; new_image().len()]);
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn a_checkpoint_with_a_second_name_is_refused_before_it_is_read() {
    let dir = scratch_dir("ck-linked");
    let lock_dir = namespace(&dir);
    let payload = dir.join("p.bin");
    fs::write(&payload, full_payload(0)).unwrap();
    let device = dir.join("dev.img");
    fs::write(&device, vec![0u8; new_image().len()]).unwrap();
    let checkpoint = dir.join("ck");
    fs::write(&checkpoint, [0u8; 221]).unwrap();
    let other = dir.join("other-name");
    fs::hard_link(&checkpoint, &other).unwrap();
    let out = apply(&verified_args(
        lock_dir.to_str().unwrap(),
        checkpoint.to_str().unwrap(),
        payload.to_str().unwrap(),
        device.to_str().unwrap(),
    ));
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("more than one name"));
    assert_eq!(fs::read(&device).unwrap(), vec![0u8; new_image().len()]);
    assert_eq!(fs::read(&other).unwrap(), vec![0u8; 221]);
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn a_checkpoint_that_is_the_device_by_hard_link_is_refused_by_identity() {
    let dir = scratch_dir("alias");
    let lock_dir = namespace(&dir);
    let payload = dir.join("p.bin");
    fs::write(&payload, full_payload(0)).unwrap();
    let device = dir.join("dev.img");
    fs::write(&device, vec![0u8; new_image().len()]).unwrap();
    let checkpoint = dir.join("ck");
    fs::hard_link(&device, &checkpoint).unwrap();
    let out = apply(&[
        "-k",
        PUBLIC_PATH,
        "--lock-dir",
        lock_dir.to_str().unwrap(),
        "--checkpoint",
        checkpoint.to_str().unwrap(),
        "--target-id",
        TARGET_ID,
        payload.to_str().unwrap(),
        device.to_str().unwrap(),
    ]);
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("same file"));
    assert_eq!(fs::read(&device).unwrap(), vec![0u8; new_image().len()]);
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn a_checkpoint_that_is_the_payload_by_hard_link_is_refused_on_both_paths_before_any_write() {
    let dir = scratch_dir("alias-payload");
    let lock_dir = namespace(&dir);
    let payload = dir.join("p.bin");
    fs::write(&payload, full_payload(0)).unwrap();
    let device = dir.join("dev.img");
    fs::write(&device, vec![0u8; new_image().len()]).unwrap();
    let checkpoint = dir.join("ck");
    fs::hard_link(&payload, &checkpoint).unwrap();
    for extra in [&[][..], &["--stream"][..]] {
        let mut args = vec![
            "-k",
            PUBLIC_PATH,
            "--lock-dir",
            lock_dir.to_str().unwrap(),
            "--checkpoint",
            checkpoint.to_str().unwrap(),
            "--target-id",
            TARGET_ID,
        ];
        args.extend_from_slice(extra);
        args.push(payload.to_str().unwrap());
        args.push(device.to_str().unwrap());
        let out = apply(&args);
        assert_eq!(out.status.code(), Some(1), "{:?}: {}", extra, String::from_utf8_lossy(&out.stderr));
        assert!(
            String::from_utf8_lossy(&out.stderr).contains("the payload and the checkpoint are the same file"),
            "{:?}: {}",
            extra,
            String::from_utf8_lossy(&out.stderr)
        );
        assert_eq!(fs::read(&device).unwrap(), vec![0u8; new_image().len()]);
    }
    fs::remove_dir_all(dir).unwrap();
}
