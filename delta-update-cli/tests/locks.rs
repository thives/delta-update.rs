mod support;

use std::fs;
use std::os::unix::fs::FileTypeExt;
use std::os::unix::fs::MetadataExt;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::fs::symlink;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::process::Output;

use delta_update_cli::filelock::LockDir;
use delta_update_cli::filelock::LockMode;
use delta_update_cli::filelock::lock_role;
use delta_update_cli::identity::Identity;
use delta_update_cli::roles::refuse_aliases;
use delta_update_client::checkpoint::Phase;
use delta_update_client::checkpoint::Record;
use delta_update_generator::build_full_payload_with_release;
use delta_update_payload::profile::EMBEDDED;
use support::hold_checkpoint_lock;
use support::hold_device_lock;
use support::hold_role_entry;
use support::hold_shared_lock;
use support::namespace;

const PRIVATE: &[u8] = include_bytes!("../../delta-update-payload/tests/fixtures/private.pem");
const PUBLIC_PATH: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../delta-update-payload/tests/fixtures/public.pem"
);
const TARGET_ID: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
const OTHER_TARGET_ID: &str = "fedcba9876543210fedcba9876543210fedcba9876543210fedcba9876543210";
const BS: usize = 4096;

fn scratch_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("ota-locks-{}-{}", std::process::id(), name));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    return dir;
}

fn new_image() -> Vec<u8> {
    return (0..3 * BS as u32).map(|i| (i * 7 % 251) as u8).collect();
}

fn zeros() -> Vec<u8> {
    return vec![0u8; new_image().len()];
}

fn payload() -> Vec<u8> {
    return match build_full_payload_with_release(&new_image(), b"A/B", PRIVATE, BS, &EMBEDDED, 0) {
        Err(e) => panic!("build failed: {}", e.msg),
        Ok(v) => v,
    };
}

fn run(args: &[&str]) -> Output {
    return Command::new(env!("CARGO_BIN_EXE_ota-apply"))
        .args(args)
        .output()
        .unwrap();
}

fn stderr_of(out: &Output) -> String {
    return String::from_utf8_lossy(&out.stderr).to_string();
}

fn path(p: &Path) -> &str {
    return p.to_str().unwrap();
}

fn apply(
    lock_dir: &str,
    checkpoint: &str,
    target: &str,
    payload: &str,
    device: &str,
    extra: &[&str],
) -> Output {
    let mut args = vec![
        "-k",
        PUBLIC_PATH,
        "--lock-dir",
        lock_dir,
        "--checkpoint",
        checkpoint,
        "--target-id",
        target,
    ];
    args.extend_from_slice(extra);
    args.push(payload);
    args.push(device);
    return run(&args);
}

fn reset(lock_dir: &str, checkpoint: &str, target: &str, payload: &str, device: &str) -> Output {
    return run(&[
        "-k",
        PUBLIC_PATH,
        "--lock-dir",
        lock_dir,
        "--reset-checkpoint",
        "--checkpoint",
        checkpoint,
        "--target-id",
        target,
        payload,
        device,
    ]);
}

fn case(dir: &Path, device: &str) -> (PathBuf, PathBuf) {
    let payload_path = dir.join("p.bin");
    fs::write(&payload_path, payload()).unwrap();
    let device_path = dir.join(device);
    fs::write(&device_path, zeros()).unwrap();
    return (payload_path, device_path);
}

fn checkpoint_lock_file(lock_dir: &Path) -> PathBuf {
    let found: Vec<PathBuf> = fs::read_dir(lock_dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| {
            p.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("checkpoint-")
        })
        .collect();
    assert_eq!(found.len(), 1, "{:?}", found);
    return found.into_iter().next().unwrap();
}

#[test]
fn a_checkpoint_held_by_another_run_refuses_another_target_on_both_paths_and_writes_nothing() {
    let dir = scratch_dir("ck-held-apply");
    let lock_dir = namespace(&dir);
    let (payload_path, second) = case(&dir, "second.img");
    let checkpoint = dir.join("ck");
    let held = hold_checkpoint_lock(&checkpoint, &dir);
    for extra in [&[][..], &["--stream"][..]] {
        let out = apply(
            path(&lock_dir),
            path(&checkpoint),
            OTHER_TARGET_ID,
            path(&payload_path),
            path(&second),
            extra,
        );
        assert_eq!(out.status.code(), Some(1), "{}", stderr_of(&out));
        assert!(
            stderr_of(&out).contains("another update holds the checkpoint"),
            "{}",
            stderr_of(&out)
        );
        assert_eq!(fs::read(&second).unwrap(), zeros());
        assert!(!checkpoint.exists());
    }
    drop(held);
    let out = apply(
        path(&lock_dir),
        path(&checkpoint),
        OTHER_TARGET_ID,
        path(&payload_path),
        path(&second),
        &[],
    );
    assert!(out.status.success(), "{}", stderr_of(&out));
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn a_checkpoint_held_by_another_run_refuses_a_reset_and_keeps_the_record() {
    let dir = scratch_dir("ck-held-reset");
    let lock_dir = namespace(&dir);
    let (payload_path, device) = case(&dir, "dev.img");
    let checkpoint = dir.join("ck");
    let out = apply(
        path(&lock_dir),
        path(&checkpoint),
        TARGET_ID,
        path(&payload_path),
        path(&device),
        &[],
    );
    assert!(out.status.success(), "{}", stderr_of(&out));
    let before = fs::read(&checkpoint).unwrap();
    let held = hold_checkpoint_lock(&checkpoint, &dir);
    let out = reset(
        path(&lock_dir),
        path(&checkpoint),
        TARGET_ID,
        path(&payload_path),
        path(&device),
    );
    assert_eq!(out.status.code(), Some(1), "{}", stderr_of(&out));
    assert!(
        stderr_of(&out).contains("another update holds the checkpoint"),
        "{}",
        stderr_of(&out)
    );
    assert_eq!(fs::read(&checkpoint).unwrap(), before);
    drop(held);
    let out = reset(
        path(&lock_dir),
        path(&checkpoint),
        TARGET_ID,
        path(&payload_path),
        path(&device),
    );
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert!(!checkpoint.exists());
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn a_checkpoint_reached_through_a_symbolic_link_to_its_directory_is_the_same_lock() {
    let dir = scratch_dir("ck-alias");
    let lock_dir = namespace(&dir);
    let real = dir.join("real");
    fs::create_dir(&real).unwrap();
    let link = dir.join("link");
    symlink(&real, &link).unwrap();
    let (payload_path, device) = case(&dir, "dev.img");
    let held = hold_checkpoint_lock(&real.join("ck"), &dir);
    let through_link = link.join("ck");
    let out = apply(
        path(&lock_dir),
        path(&through_link),
        TARGET_ID,
        path(&payload_path),
        path(&device),
        &[],
    );
    assert_eq!(out.status.code(), Some(1), "{}", stderr_of(&out));
    assert!(
        stderr_of(&out).contains("another update holds the checkpoint"),
        "{}",
        stderr_of(&out)
    );
    assert_eq!(fs::read(&device).unwrap(), zeros());
    drop(held);
    let out = apply(
        path(&lock_dir),
        path(&through_link),
        TARGET_ID,
        path(&payload_path),
        path(&device),
        &[],
    );
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert!(real.join("ck").exists());
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn a_run_refused_on_its_device_leaves_the_checkpoint_free_for_the_next_run() {
    let dir = scratch_dir("device-refusal");
    let lock_dir = namespace(&dir);
    let (payload_path, held_device) = case(&dir, "held.img");
    let free_device = dir.join("free.img");
    fs::write(&free_device, zeros()).unwrap();
    let checkpoint = dir.join("ck");
    let held = hold_device_lock(&held_device, &dir);
    let out = apply(
        path(&lock_dir),
        path(&checkpoint),
        TARGET_ID,
        path(&payload_path),
        path(&held_device),
        &[],
    );
    assert_eq!(out.status.code(), Some(1), "{}", stderr_of(&out));
    assert!(
        stderr_of(&out).contains("another update holds"),
        "{}",
        stderr_of(&out)
    );
    assert!(!checkpoint.exists());
    drop(held);
    let out = apply(
        path(&lock_dir),
        path(&checkpoint),
        TARGET_ID,
        path(&payload_path),
        path(&free_device),
        &[],
    );
    assert!(out.status.success(), "{}", stderr_of(&out));
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn an_unrelated_checkpoint_and_device_pair_proceeds_while_another_pair_is_held() {
    let dir = scratch_dir("unrelated");
    let lock_dir = namespace(&dir);
    let (payload_path, held_device) = case(&dir, "held.img");
    let (_, free_device) = case(&dir, "free.img");
    fs::create_dir(dir.join("held-dir")).unwrap();
    fs::create_dir(dir.join("free-dir")).unwrap();
    let held_checkpoint = hold_checkpoint_lock(&dir.join("held-dir").join("ck"), &dir);
    let held_device_lock = hold_device_lock(&held_device, &dir);
    let out = apply(
        path(&lock_dir),
        path(&dir.join("free-dir").join("ck")),
        TARGET_ID,
        path(&payload_path),
        path(&free_device),
        &[],
    );
    let err = stderr_of(&out);
    assert!(out.status.success(), "{}", err);
    assert_eq!(fs::read(&free_device).unwrap(), new_image());
    drop(held_device_lock);
    drop(held_checkpoint);
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn a_checkpoint_named_like_a_lock_file_is_refused_while_its_directory_is_held_and_writes_nothing() {
    let dir = scratch_dir("ck-collision");
    let lock_dir = namespace(&dir);
    let (payload_path, device) = case(&dir, "dev.img");
    let (_, second) = case(&dir, "second.img");
    let state = dir.join("state");
    let state_lock_named = dir.join("state.lock");
    let held = hold_checkpoint_lock(&state, &dir);
    let out = apply(
        path(&lock_dir),
        path(&state_lock_named),
        TARGET_ID,
        path(&payload_path),
        path(&device),
        &[],
    );
    assert_eq!(out.status.code(), Some(1), "{}", stderr_of(&out));
    assert!(
        stderr_of(&out).contains("another update holds the checkpoint"),
        "{}",
        stderr_of(&out)
    );
    assert!(!state_lock_named.exists());
    assert_eq!(fs::read(&device).unwrap(), zeros());
    let out = apply(
        path(&lock_dir),
        path(&state),
        TARGET_ID,
        path(&payload_path),
        path(&second),
        &[],
    );
    assert_eq!(out.status.code(), Some(1), "{}", stderr_of(&out));
    assert!(
        stderr_of(&out).contains("another update holds the checkpoint"),
        "{}",
        stderr_of(&out)
    );
    assert!(!state.exists());
    assert_eq!(fs::read(&second).unwrap(), zeros());
    drop(held);
    let out = apply(
        path(&lock_dir),
        path(&state_lock_named),
        TARGET_ID,
        path(&payload_path),
        path(&device),
        &[],
    );
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert!(state_lock_named.exists());
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn a_checkpoint_named_with_the_reserved_prefix_is_refused_before_any_lock_or_write() {
    let dir = scratch_dir("ck-reserved");
    let lock_dir = namespace(&dir);
    let (payload_path, device) = case(&dir, "dev.img");
    let staged = dir.join(".ota-checkpoint.stage.1.0");
    let out = apply(
        path(&lock_dir),
        path(&staged),
        TARGET_ID,
        path(&payload_path),
        path(&device),
        &[],
    );
    assert_eq!(out.status.code(), Some(1), "{}", stderr_of(&out));
    assert!(stderr_of(&out).contains("reserved"), "{}", stderr_of(&out));
    assert_eq!(fs::read_dir(&lock_dir).unwrap().count(), 0);
    assert!(!staged.exists());
    assert_eq!(fs::read(&device).unwrap(), zeros());
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn a_missing_or_group_accessible_lock_namespace_refuses_the_run_before_any_write() {
    let dir = scratch_dir("ns-refused");
    let (payload_path, device) = case(&dir, "dev.img");
    let checkpoint = dir.join("ck");
    let missing = dir.join("no-such-dir");
    let out = apply(
        path(&missing),
        path(&checkpoint),
        TARGET_ID,
        path(&payload_path),
        path(&device),
        &[],
    );
    assert_eq!(out.status.code(), Some(1), "{}", stderr_of(&out));
    assert!(
        stderr_of(&out).contains("cannot use lock directory"),
        "{}",
        stderr_of(&out)
    );
    assert_eq!(fs::read(&device).unwrap(), zeros());
    assert!(!checkpoint.exists());
    let open_ns = dir.join("open-ns");
    fs::create_dir(&open_ns).unwrap();
    fs::set_permissions(&open_ns, fs::Permissions::from_mode(0o755)).unwrap();
    let out = apply(
        path(&open_ns),
        path(&checkpoint),
        TARGET_ID,
        path(&payload_path),
        path(&device),
        &[],
    );
    assert_eq!(out.status.code(), Some(1), "{}", stderr_of(&out));
    assert!(
        stderr_of(&out).contains("accessible to group or others"),
        "{}",
        stderr_of(&out)
    );
    assert_eq!(fs::read(&device).unwrap(), zeros());
    assert!(!checkpoint.exists());
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn a_checkpoint_record_inside_the_lock_namespace_is_refused_with_no_write() {
    let dir = scratch_dir("ck-in-namespace");
    let lock_dir = namespace(&dir);
    let (payload_path, device) = case(&dir, "dev.img");
    let inside = lock_dir.join("ck");
    let out = apply(
        path(&lock_dir),
        path(&inside),
        TARGET_ID,
        path(&payload_path),
        path(&device),
        &[],
    );
    assert_eq!(out.status.code(), Some(1), "{}", stderr_of(&out));
    assert!(
        stderr_of(&out).contains("names a file in the lock directory"),
        "{}",
        stderr_of(&out)
    );
    assert_eq!(fs::read(&device).unwrap(), zeros());
    assert!(!inside.exists());
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn a_checkpoint_lock_file_that_is_a_symbolic_link_is_refused_with_no_write() {
    let dir = scratch_dir("lock-symlink");
    let lock_dir = namespace(&dir);
    let (payload_path, device) = case(&dir, "dev.img");
    let checkpoint = dir.join("ck");
    let out = apply(
        path(&lock_dir),
        path(&checkpoint),
        TARGET_ID,
        path(&payload_path),
        path(&device),
        &[],
    );
    assert!(out.status.success(), "{}", stderr_of(&out));
    let lock = checkpoint_lock_file(&lock_dir);
    fs::remove_file(&lock).unwrap();
    let victim = dir.join("victim");
    fs::write(&victim, b"keep").unwrap();
    symlink(&victim, &lock).unwrap();
    let (payload_path, device) = case(&dir, "dev2.img");
    let out = apply(
        path(&lock_dir),
        path(&checkpoint),
        TARGET_ID,
        path(&payload_path),
        path(&device),
        &[],
    );
    assert_eq!(out.status.code(), Some(1), "{}", stderr_of(&out));
    assert!(
        stderr_of(&out).contains("cannot open lock file"),
        "{}",
        stderr_of(&out)
    );
    assert_eq!(fs::read(&victim).unwrap(), b"keep");
    assert_eq!(fs::read(&device).unwrap(), zeros());
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn the_checkpoint_lock_is_a_private_kept_file_in_the_namespace_and_never_beside_the_record() {
    let dir = scratch_dir("lock-file");
    let lock_dir = namespace(&dir);
    let (payload_path, device) = case(&dir, "dev.img");
    let checkpoint = dir.join("ck");
    let out = apply(
        path(&lock_dir),
        path(&checkpoint),
        TARGET_ID,
        path(&payload_path),
        path(&device),
        &[],
    );
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert!(!dir.join("ck.lock").exists());
    let lock = checkpoint_lock_file(&lock_dir);
    assert_eq!(
        fs::metadata(&lock).unwrap().permissions().mode() & 0o777,
        0o600
    );
    let out = apply(
        path(&lock_dir),
        path(&checkpoint),
        TARGET_ID,
        path(&payload_path),
        path(&device),
        &[],
    );
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert!(lock.exists());
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn an_old_sidecar_lock_beside_the_record_is_neither_used_nor_removed() {
    let dir = scratch_dir("old-sidecar");
    let lock_dir = namespace(&dir);
    let (payload_path, device) = case(&dir, "dev.img");
    let victim = dir.join("victim");
    fs::write(&victim, b"keep").unwrap();
    let sidecar = dir.join("ck.lock");
    symlink(&victim, &sidecar).unwrap();
    let out = apply(
        path(&lock_dir),
        path(&dir.join("ck")),
        TARGET_ID,
        path(&payload_path),
        path(&device),
        &[],
    );
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert_eq!(fs::read(&victim).unwrap(), b"keep");
    assert!(
        fs::symlink_metadata(&sidecar)
            .unwrap()
            .file_type()
            .is_symlink()
    );
    fs::remove_dir_all(dir).unwrap();
}

fn old_image(dir: &Path, name: &str) -> PathBuf {
    let path = dir.join(name);
    fs::write(&path, zeros()).unwrap();
    return path;
}

#[test]
fn an_old_image_that_another_run_writes_is_refused_on_both_paths_and_nothing_is_written() {
    let dir = scratch_dir("old-held-writer");
    let lock_dir = namespace(&dir);
    let (payload_path, device) = case(&dir, "dev.img");
    let old = old_image(&dir, "old.img");
    let source = path(&old);
    let checkpoint = dir.join("ck");
    let held = hold_device_lock(&old, &dir);
    for extra in [&["-s", source][..], &["--stream", "-s", source][..]] {
        let out = apply(
            path(&lock_dir),
            path(&checkpoint),
            TARGET_ID,
            path(&payload_path),
            path(&device),
            extra,
        );
        assert_eq!(out.status.code(), Some(1), "{}", stderr_of(&out));
        assert!(
            stderr_of(&out).contains("another update holds"),
            "{}",
            stderr_of(&out)
        );
        assert_eq!(fs::read(&device).unwrap(), zeros());
        assert!(!checkpoint.exists());
    }
    assert_eq!(fs::read(&old).unwrap(), zeros());
    drop(held);
    let out = apply(
        path(&lock_dir),
        path(&checkpoint),
        TARGET_ID,
        path(&payload_path),
        path(&device),
        &["-s", source],
    );
    assert!(out.status.success(), "{}", stderr_of(&out));
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn an_old_image_that_another_run_reads_is_shared_with_this_run_and_refuses_a_writer_of_it() {
    let dir = scratch_dir("old-held-reader");
    let lock_dir = namespace(&dir);
    let (payload_path, device) = case(&dir, "dev.img");
    let old = old_image(&dir, "old.img");
    let source = path(&old);
    let reader = hold_shared_lock(&old, &dir);
    let out = apply(
        path(&lock_dir),
        path(&dir.join("ck")),
        TARGET_ID,
        path(&payload_path),
        path(&device),
        &["-s", source],
    );
    assert!(out.status.success(), "{}", stderr_of(&out));
    let out = apply(
        path(&lock_dir),
        path(&dir.join("ck2")),
        OTHER_TARGET_ID,
        path(&payload_path),
        source,
        &[],
    );
    assert_eq!(out.status.code(), Some(1), "{}", stderr_of(&out));
    assert!(
        stderr_of(&out).contains("another update holds"),
        "{}",
        stderr_of(&out)
    );
    assert_eq!(fs::read(&old).unwrap(), zeros());
    drop(reader);
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn an_old_image_and_a_device_that_no_other_run_names_proceed_while_unrelated_files_are_held() {
    let dir = scratch_dir("old-unrelated");
    let lock_dir = namespace(&dir);
    let (payload_path, device) = case(&dir, "dev.img");
    let old = old_image(&dir, "old.img");
    let read_elsewhere = old_image(&dir, "elsewhere.img");
    let written_elsewhere = old_image(&dir, "third.img");
    let held_reader = hold_shared_lock(&read_elsewhere, &dir);
    let held_writer = hold_device_lock(&written_elsewhere, &dir);
    let out = apply(
        path(&lock_dir),
        path(&dir.join("ck")),
        TARGET_ID,
        path(&payload_path),
        path(&device),
        &["-s", path(&old)],
    );
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert_eq!(fs::read(&device).unwrap(), new_image());
    drop(held_writer);
    drop(held_reader);
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn a_streamed_payload_that_another_run_writes_is_refused_before_it_is_read() {
    let dir = scratch_dir("payload-held-writer");
    let lock_dir = namespace(&dir);
    let (payload_path, device) = case(&dir, "dev.img");
    let checkpoint = dir.join("ck");
    let held = hold_device_lock(&payload_path, &dir);
    let out = apply(
        path(&lock_dir),
        path(&checkpoint),
        TARGET_ID,
        path(&payload_path),
        path(&device),
        &["--stream"],
    );
    assert_eq!(out.status.code(), Some(1), "{}", stderr_of(&out));
    assert!(
        stderr_of(&out).contains("another update holds"),
        "{}",
        stderr_of(&out)
    );
    assert!(
        !stderr_of(&out).contains("payload: revision"),
        "{}",
        stderr_of(&out)
    );
    assert_eq!(fs::read(&device).unwrap(), zeros());
    assert!(!checkpoint.exists());
    drop(held);
    let out = apply(
        path(&lock_dir),
        path(&checkpoint),
        TARGET_ID,
        path(&payload_path),
        path(&device),
        &["--stream"],
    );
    assert!(out.status.success(), "{}", stderr_of(&out));
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn a_streamed_payload_that_another_run_reads_is_shared_and_the_streamed_update_proceeds() {
    let dir = scratch_dir("payload-held-reader");
    let lock_dir = namespace(&dir);
    let (payload_path, device) = case(&dir, "dev.img");
    let reader = hold_shared_lock(&payload_path, &dir);
    let out = apply(
        path(&lock_dir),
        path(&dir.join("ck")),
        TARGET_ID,
        path(&payload_path),
        path(&device),
        &["--stream"],
    );
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert_eq!(fs::read(&device).unwrap(), new_image());
    drop(reader);
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn another_runs_checkpoint_entry_refuses_each_role_before_it_is_used() {
    for role in ["device", "old", "payload"] {
        for exists in [false, true] {
            if role == "payload" && !exists {
                continue;
            }
            let dir = scratch_dir(&format!("checkpoint-as-{role}-{exists}"));
            let lock_dir = namespace(&dir);
            let (payload_path, device) = case(&dir, "dev.img");
            let held_checkpoint = dir.join("held-record");
            let contents = if role == "payload" {
                payload()
            } else {
                zeros()
            };
            if exists {
                fs::write(&held_checkpoint, &contents).unwrap();
            }
            let held = hold_checkpoint_lock(&held_checkpoint, &dir);
            let own_dir = dir.join("own");
            fs::create_dir(&own_dir).unwrap();
            let own_checkpoint = own_dir.join("ck");
            let extras = match role {
                "old" => vec![
                    vec!["-s", path(&held_checkpoint)],
                    vec!["--stream", "-s", path(&held_checkpoint)],
                ],
                "payload" => vec![vec!["--stream"]],
                _ => vec![vec![], vec!["--stream"]],
            };
            let selected_payload = if role == "payload" {
                &held_checkpoint
            } else {
                &payload_path
            };
            let selected_device = if role == "device" {
                &held_checkpoint
            } else {
                &device
            };
            for extra in &extras {
                let out = apply(
                    path(&lock_dir),
                    path(&own_checkpoint),
                    TARGET_ID,
                    path(selected_payload),
                    path(selected_device),
                    extra,
                );
                assert_eq!(
                    out.status.code(),
                    Some(1),
                    "{role} exists={exists}: {}",
                    stderr_of(&out)
                );
                assert!(
                    stderr_of(&out).contains("another update holds"),
                    "{role} exists={exists}: {}",
                    stderr_of(&out)
                );
                if role == "payload" {
                    assert!(
                        !stderr_of(&out).contains("payload: revision"),
                        "{}",
                        stderr_of(&out)
                    );
                }
                assert_eq!(fs::read(&device).unwrap(), zeros());
                assert!(!own_checkpoint.exists());
                if exists {
                    assert_eq!(fs::read(&held_checkpoint).unwrap(), contents);
                } else {
                    assert!(!held_checkpoint.exists());
                }
            }
            drop(held);
            if exists {
                let out = apply(
                    path(&lock_dir),
                    path(&own_checkpoint),
                    TARGET_ID,
                    path(selected_payload),
                    path(selected_device),
                    &extras[0],
                );
                assert!(out.status.success(), "{role}: {}", stderr_of(&out));
                assert_eq!(fs::read(selected_device).unwrap(), new_image());
            }
            fs::remove_dir_all(dir).unwrap();
        }
    }
}

#[test]
fn role_entry_locks_exclude_checkpoint_creation_replacement_and_reset_but_not_other_entries() {
    for shared in [false, true] {
        let dir = scratch_dir(if shared {
            "reader-as-checkpoint"
        } else {
            "writer-as-checkpoint"
        });
        let lock_dir = namespace(&dir);
        let (payload_path, device) = case(&dir, "dev.img");
        let checkpoint = dir.join("ck");
        let other_checkpoint = dir.join("other-ck");
        let other_device = dir.join("other.img");
        fs::write(&other_device, zeros()).unwrap();

        let held = hold_role_entry(&checkpoint, &dir, shared);
        let out = apply(
            path(&lock_dir),
            path(&checkpoint),
            TARGET_ID,
            path(&payload_path),
            path(&device),
            &[],
        );
        assert_eq!(out.status.code(), Some(1), "{}", stderr_of(&out));
        assert!(
            stderr_of(&out).contains("another update holds"),
            "{}",
            stderr_of(&out)
        );
        assert!(!checkpoint.exists());
        assert_eq!(fs::read(&device).unwrap(), zeros());
        let out = apply(
            path(&lock_dir),
            path(&other_checkpoint),
            OTHER_TARGET_ID,
            path(&payload_path),
            path(&other_device),
            &[],
        );
        assert!(out.status.success(), "{}", stderr_of(&out));
        assert_eq!(fs::read(&other_device).unwrap(), new_image());
        assert!(!checkpoint.exists());
        drop(held);

        let out = apply(
            path(&lock_dir),
            path(&checkpoint),
            TARGET_ID,
            path(&payload_path),
            path(&device),
            &[],
        );
        assert!(out.status.success(), "{}", stderr_of(&out));
        let bytes = fs::read(&checkpoint).unwrap();
        let mut record = Record::decode(bytes.as_slice().try_into().unwrap()).unwrap();
        record.phase = Phase::AwaitingFinal;
        let before = record.encode();
        fs::write(&checkpoint, before).unwrap();
        let original_inode = fs::metadata(&checkpoint).unwrap().ino();
        let held = hold_role_entry(&checkpoint, &dir, shared);
        for out in [
            apply(
                path(&lock_dir),
                path(&checkpoint),
                TARGET_ID,
                path(&payload_path),
                path(&device),
                &[],
            ),
            reset(
                path(&lock_dir),
                path(&checkpoint),
                TARGET_ID,
                path(&payload_path),
                path(&device),
            ),
        ] {
            assert_eq!(out.status.code(), Some(1), "{}", stderr_of(&out));
            assert!(
                stderr_of(&out).contains("another update holds"),
                "{}",
                stderr_of(&out)
            );
            assert_eq!(fs::read(&checkpoint).unwrap(), before);
            assert_eq!(fs::metadata(&checkpoint).unwrap().ino(), original_inode);
            assert_eq!(fs::read(&device).unwrap(), new_image());
        }
        drop(held);
        let out = apply(
            path(&lock_dir),
            path(&checkpoint),
            TARGET_ID,
            path(&payload_path),
            path(&device),
            &[],
        );
        assert!(out.status.success(), "{}", stderr_of(&out));
        assert_ne!(fs::metadata(&checkpoint).unwrap().ino(), original_inode);
        let bytes = fs::read(&checkpoint).unwrap();
        assert_eq!(
            Record::decode(bytes.as_slice().try_into().unwrap())
                .unwrap()
                .phase,
            Phase::Complete
        );
        let out = reset(
            path(&lock_dir),
            path(&checkpoint),
            TARGET_ID,
            path(&payload_path),
            path(&device),
        );
        assert!(out.status.success(), "{}", stderr_of(&out));
        assert!(!checkpoint.exists());
        assert!(other_checkpoint.exists());
        fs::remove_dir_all(dir).unwrap();
    }
}

#[test]
fn two_device_nodes_for_one_block_device_contend_through_the_namespace() {
    let Some(device) = std::env::var_os("OTA_TEST_BLOCK_DEVICE") else {
        eprintln!(
            "skipped: set OTA_TEST_BLOCK_DEVICE to a block device this test may lock (for example a loop device)"
        );
        return;
    };
    let file_type = fs::metadata(&device).unwrap().file_type();
    assert!(
        file_type.is_block_device(),
        "OTA_TEST_BLOCK_DEVICE is not a block device"
    );
    let rdev = fs::metadata(&device).unwrap().rdev();
    let dir = scratch_dir("block-nodes");
    let lock_dir = namespace(&dir);
    let first = dir.join("node-a");
    let second = dir.join("node-b");
    for node in [&first, &second] {
        let c_node = std::ffi::CString::new(path(node)).unwrap();
        if unsafe { libc::mknod(c_node.as_ptr(), libc::S_IFBLK | 0o600, rdev) } != 0 {
            let e = std::io::Error::last_os_error();
            fs::remove_dir_all(&dir).unwrap();
            if e.raw_os_error() == Some(libc::EPERM) {
                eprintln!("skipped: creating device nodes needs CAP_MKNOD: {}", e);
                return;
            }
            panic!("cannot create a device node: {}", e);
        }
    }
    let open_node = |node: &Path| {
        fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_PATH)
            .open(node)
            .unwrap()
    };
    let first_file = open_node(&first);
    let second_file = open_node(&second);
    let first_id = Identity::of(&first_file).unwrap();
    let second_id = Identity::of(&second_file).unwrap();
    assert_eq!(first_id, Identity::Block { rdev });
    assert_eq!(first_id, second_id);
    let refused = refuse_aliases(&[("device", first_id), ("old image", second_id)])
        .err()
        .unwrap();
    assert_eq!(
        refused,
        "the device and the old image are the same block device"
    );

    let ns = LockDir::open(&lock_dir).unwrap();
    let held = lock_role(&first_file, path(&first), LockMode::Exclusive, &ns).unwrap();
    let refused = lock_role(&second_file, path(&second), LockMode::Exclusive, &ns)
        .err()
        .unwrap();
    assert!(refused.contains("another update holds"), "{}", refused);
    drop(held);
    assert!(lock_role(&second_file, path(&second), LockMode::Exclusive, &ns).is_ok());
    let reader = lock_role(&first_file, path(&first), LockMode::Shared, &ns).unwrap();
    let other_reader = lock_role(&second_file, path(&second), LockMode::Shared, &ns).unwrap();
    let refused = lock_role(&first_file, path(&first), LockMode::Exclusive, &ns)
        .err()
        .unwrap();
    assert!(refused.contains("another update holds"), "{}", refused);
    drop(other_reader);
    drop(reader);
    fs::remove_dir_all(dir).unwrap();
}
