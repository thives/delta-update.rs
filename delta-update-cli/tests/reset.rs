mod support;

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::process::Output;

use delta_update_cli::filecheckpoint::FileCheckpoint;
use delta_update_cli::filedevice::FileDevice;
use delta_update_cli::filelock::LockDir;
use delta_update_client::apply;
use delta_update_client::apply::Trust;
use delta_update_generator::SearchConfig;
use delta_update_generator::build_full_payload_with_release;
use delta_update_generator::build_payload_with;
use delta_update_payload::error::Code;
use delta_update_payload::profile::EMBEDDED;
use delta_update_payload::rsa::parse_pubkey_pem;
use support::hold_device_lock;
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
    let dir = std::env::temp_dir().join(format!("ota-reset-{}-{}", std::process::id(), name));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    return dir;
}

fn run(args: &[&str]) -> Output {
    return Command::new(env!("CARGO_BIN_EXE_ota-apply"))
        .args(args)
        .output()
        .unwrap();
}

fn new_image() -> Vec<u8> {
    return (0..3 * BS as u32).map(|i| (i * 7 % 251) as u8).collect();
}

fn full_image(release: u64) -> Vec<u8> {
    return match build_full_payload_with_release(
        &new_image(),
        b"A/B",
        PRIVATE,
        BS,
        &EMBEDDED,
        release,
    ) {
        Err(e) => panic!("build failed: {}", e.msg),
        Ok(v) => v,
    };
}

fn old_image() -> Vec<u8> {
    return vec![0x11; 3 * BS];
}

fn delta_image() -> Vec<u8> {
    let mut image = old_image();
    image[BS..2 * BS].fill(0x22);
    return image;
}

fn delta() -> Vec<u8> {
    let payload = match build_payload_with(
        &old_image(),
        &delta_image(),
        b"A/B",
        PRIVATE,
        BS,
        &EMBEDDED,
        &SearchConfig::quick(),
    ) {
        Err(e) => panic!("build failed: {}", e.msg),
        Ok(v) => v,
    };
    let prepared =
        delta_update_client::apply::prepare(&payload, Trust::Unverified, &EMBEDDED).unwrap();
    assert_eq!(
        prepared.contract(),
        delta_update_payload::plan::Contract::BaseDependent
    );
    return payload;
}

struct Case {
    dir: PathBuf,
    lock_dir: PathBuf,
    payload: PathBuf,
    device: PathBuf,
    checkpoint: PathBuf,
}

impl Case {
    fn new(name: &str, payload: &[u8], device: &[u8]) -> Case {
        let dir = scratch_dir(name);
        let lock_dir = namespace(&dir);
        let payload_path = dir.join("p.bin");
        fs::write(&payload_path, payload).unwrap();
        let device_path = dir.join("dev.img");
        fs::write(&device_path, device).unwrap();
        return Case {
            checkpoint: dir.join("ck"),
            dir: dir,
            lock_dir: lock_dir,
            payload: payload_path,
            device: device_path,
        };
    }

    fn path(p: &Path) -> &str {
        return p.to_str().unwrap();
    }

    fn apply(&self) -> Output {
        return run(&[
            "-k",
            PUBLIC_PATH,
            "--lock-dir",
            Case::path(&self.lock_dir),
            "--checkpoint",
            Case::path(&self.checkpoint),
            "--target-id",
            TARGET_ID,
            Case::path(&self.payload),
            Case::path(&self.device),
        ]);
    }

    fn reset(&self, target: &str) -> Output {
        return self.reset_with(target, &[]);
    }

    fn reset_with(&self, target: &str, extra: &[&str]) -> Output {
        let mut args = vec![
            "-k",
            PUBLIC_PATH,
            "--lock-dir",
            Case::path(&self.lock_dir),
            "--reset-checkpoint",
            "--checkpoint",
            Case::path(&self.checkpoint),
            "--target-id",
            target,
        ];
        args.extend_from_slice(extra);
        args.push(Case::path(&self.payload));
        args.push(Case::path(&self.device));
        return run(&args);
    }
}

fn stderr_of(out: &Output) -> String {
    return String::from_utf8_lossy(&out.stderr).to_string();
}

#[test]
fn a_completed_destination_independent_update_resets_and_the_next_run_applies_again() {
    let case = Case::new("independent", &full_image(0), &vec![0u8; new_image().len()]);
    let out = case.apply();
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert!(case.checkpoint.exists());
    fs::write(&case.device, vec![0xa5u8; new_image().len()]).unwrap();
    let out = case.reset(TARGET_ID);
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert!(
        stderr_of(&out).contains("reset the checkpoint in phase Complete"),
        "{}",
        stderr_of(&out)
    );
    assert!(!case.checkpoint.exists());
    let out = case.apply();
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert_eq!(fs::read(&case.device).unwrap(), new_image());
    fs::remove_dir_all(&case.dir).unwrap();
}

#[test]
fn a_base_dependent_reset_is_refused_while_the_device_holds_the_new_image_and_keeps_the_checkpoint()
{
    let case = Case::new("refused", &delta(), &old_image());
    let out = case.apply();
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert_eq!(fs::read(&case.device).unwrap(), delta_image());
    let before = fs::read(&case.checkpoint).unwrap();
    let out = case.reset(TARGET_ID);
    assert_eq!(out.status.code(), Some(1));
    assert!(
        stderr_of(&out).contains("does not match old partition hash"),
        "{}",
        stderr_of(&out)
    );
    assert_eq!(fs::read(&case.checkpoint).unwrap(), before);
    assert_eq!(fs::read(&case.device).unwrap(), delta_image());
    fs::remove_dir_all(&case.dir).unwrap();
}

#[test]
fn a_base_dependent_reset_succeeds_once_the_old_image_is_restored_and_the_next_run_starts_over() {
    let case = Case::new("restored", &delta(), &old_image());
    let out = case.apply();
    assert!(out.status.success(), "{}", stderr_of(&out));
    fs::write(&case.device, old_image()).unwrap();
    let out = case.reset(TARGET_ID);
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert!(!case.checkpoint.exists());
    assert_eq!(fs::read(&case.device).unwrap(), old_image());
    let out = case.apply();
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert_eq!(fs::read(&case.device).unwrap(), delta_image());
    fs::remove_dir_all(&case.dir).unwrap();
}

#[test]
fn a_reset_for_another_target_or_another_payload_is_refused_and_the_checkpoint_is_kept() {
    let case = Case::new("identity", &full_image(0), &vec![0u8; new_image().len()]);
    assert!(case.apply().status.success());
    let before = fs::read(&case.checkpoint).unwrap();

    let out = case.reset(OTHER_TARGET_ID);
    assert_eq!(out.status.code(), Some(1));
    assert!(
        stderr_of(&out).contains("another payload or target"),
        "{}",
        stderr_of(&out)
    );
    assert_eq!(fs::read(&case.checkpoint).unwrap(), before);

    let other = case.dir.join("other.bin");
    fs::write(&other, full_image(1)).unwrap();
    let out = run(&[
        "-k",
        PUBLIC_PATH,
        "--lock-dir",
        Case::path(&case.lock_dir),
        "--reset-checkpoint",
        "--checkpoint",
        Case::path(&case.checkpoint),
        "--target-id",
        TARGET_ID,
        Case::path(&other),
        Case::path(&case.device),
    ]);
    assert_eq!(out.status.code(), Some(1));
    assert!(
        stderr_of(&out).contains("another payload or target"),
        "{}",
        stderr_of(&out)
    );
    assert_eq!(fs::read(&case.checkpoint).unwrap(), before);
    fs::remove_dir_all(&case.dir).unwrap();
}

#[test]
fn a_reset_with_no_checkpoint_says_so_and_changes_nothing() {
    let case = Case::new("none", &full_image(0), &vec![0x5au8; new_image().len()]);
    let out = case.reset(TARGET_ID);
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert!(
        stderr_of(&out).contains("no checkpoint to reset"),
        "{}",
        stderr_of(&out)
    );
    assert!(!case.checkpoint.exists());
    assert_eq!(
        fs::read(&case.device).unwrap(),
        vec![0x5au8; new_image().len()]
    );
    fs::remove_dir_all(&case.dir).unwrap();
}

#[test]
fn a_reset_is_refused_while_another_process_holds_the_device() {
    let case = Case::new("held", &full_image(0), &vec![0u8; new_image().len()]);
    assert!(case.apply().status.success());
    let before = fs::read(&case.checkpoint).unwrap();
    let held = hold_device_lock(&case.device, &case.dir);
    let out = case.reset(TARGET_ID);
    assert_eq!(out.status.code(), Some(1));
    assert!(
        stderr_of(&out).contains("another update holds"),
        "{}",
        stderr_of(&out)
    );
    assert_eq!(fs::read(&case.checkpoint).unwrap(), before);
    drop(held);
    fs::remove_dir_all(&case.dir).unwrap();
}

#[test]
fn a_failed_clear_keeps_the_record_and_a_retry_succeeds() {
    let case = Case::new("clear-fails", &full_image(0), &vec![0u8; new_image().len()]);
    assert!(case.apply().status.success());
    fs::set_permissions(&case.dir, fs::Permissions::from_mode(0o555)).unwrap();
    if fs::write(case.dir.join("probe"), b"").is_ok() {
        fs::set_permissions(&case.dir, fs::Permissions::from_mode(0o755)).unwrap();
        fs::remove_dir_all(&case.dir).unwrap();
        return;
    }
    let out = case.reset(TARGET_ID);
    fs::set_permissions(&case.dir, fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(out.status.code(), Some(1), "{}", stderr_of(&out));
    assert!(case.checkpoint.exists());
    let out = case.reset(TARGET_ID);
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert!(!case.checkpoint.exists());
    fs::remove_dir_all(&case.dir).unwrap();
}

#[test]
fn a_damaged_checkpoint_is_refused_by_reset_and_never_deleted() {
    let case = Case::new("damaged", &full_image(0), &vec![0u8; new_image().len()]);
    assert!(case.apply().status.success());
    let mut bytes = fs::read(&case.checkpoint).unwrap();
    bytes[40] ^= 0x01;
    fs::write(&case.checkpoint, &bytes).unwrap();
    let out = case.reset(TARGET_ID);
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(fs::read(&case.checkpoint).unwrap(), bytes);
    fs::remove_dir_all(&case.dir).unwrap();
}

#[test]
fn a_reset_does_not_open_the_old_image_so_a_missing_source_path_does_not_fail_it() {
    let case = Case::new(
        "source-missing",
        &full_image(0),
        &vec![0u8; new_image().len()],
    );
    assert!(case.apply().status.success());
    let missing = case.dir.join("no-such-old-image");
    let out = case.reset_with(TARGET_ID, &["-s", Case::path(&missing)]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert!(
        stderr_of(&out).contains("reset the checkpoint in phase Complete"),
        "{}",
        stderr_of(&out)
    );
    assert!(!case.checkpoint.exists());
    fs::remove_dir_all(&case.dir).unwrap();
}

#[test]
fn a_reset_does_not_refuse_a_source_that_aliases_the_device() {
    let case = Case::new(
        "source-alias",
        &full_image(0),
        &vec![0u8; new_image().len()],
    );
    assert!(case.apply().status.success());
    let alias = case.dir.join("alias.img");
    fs::hard_link(&case.device, &alias).unwrap();
    let out = case.reset_with(TARGET_ID, &["-s", Case::path(&alias)]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert!(!case.checkpoint.exists());
    fs::remove_dir_all(&case.dir).unwrap();
}

#[test]
fn a_base_dependent_reset_still_refuses_for_its_own_base_whatever_source_is_named() {
    let case = Case::new("source-base", &delta(), &old_image());
    let out = case.apply();
    assert!(out.status.success(), "{}", stderr_of(&out));
    let before = fs::read(&case.checkpoint).unwrap();
    let missing = case.dir.join("no-such-old-image");
    let out = case.reset_with(TARGET_ID, &["-s", Case::path(&missing)]);
    assert_eq!(out.status.code(), Some(1), "{}", stderr_of(&out));
    assert!(
        stderr_of(&out).contains("does not match old partition hash"),
        "{}",
        stderr_of(&out)
    );
    assert_eq!(fs::read(&case.checkpoint).unwrap(), before);
    fs::remove_dir_all(&case.dir).unwrap();
}

fn target_bytes() -> [u8; 32] {
    return std::array::from_fn(|i| u8::from_str_radix(&TARGET_ID[2 * i..2 * i + 2], 16).unwrap());
}

thread_local! {
    static SYNCS_FAIL: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

fn switchable_dir_sync(dir: &fs::File) -> std::io::Result<()> {
    if SYNCS_FAIL.get() {
        return Err(std::io::Error::other("injected directory sync failure"));
    }
    return dir.sync_all();
}

fn open_with_failing_syncs(ns: &LockDir, checkpoint: &Path) -> FileCheckpoint {
    SYNCS_FAIL.set(false);
    let store = FileCheckpoint::open_with_dir_sync(ns, Case::path(checkpoint), switchable_dir_sync)
        .unwrap();
    SYNCS_FAIL.set(true);
    return store;
}

#[test]
fn a_removal_not_confirmed_durable_fails_the_reset_and_leaves_the_record_absent() {
    let case = Case::new("uncertain", &full_image(0), &vec![0u8; new_image().len()]);
    assert!(case.apply().status.success());
    let keys = vec![parse_pubkey_pem(&fs::read(PUBLIC_PATH).unwrap()).unwrap()];
    let payload = fs::read(&case.payload).unwrap();
    let prepared = apply::prepare(&payload, Trust::Verified(&keys), &EMBEDDED).unwrap();
    let dev = FileDevice::open(Case::path(&case.device).to_string()).unwrap();
    let device_before = fs::read(&case.device).unwrap();
    let ns = LockDir::open(&case.lock_dir).unwrap();
    let mut store = open_with_failing_syncs(&ns, &case.checkpoint);
    let err = prepared
        .reset_checkpoint(&dev, &mut store, &target_bytes())
        .err()
        .unwrap();
    assert_eq!(err.code, Code::checkpoint_removal_uncertain);
    assert!(!case.checkpoint.exists());
    drop(store);
    let mut still_failing = open_with_failing_syncs(&ns, &case.checkpoint);
    let err = prepared
        .reset_checkpoint(&dev, &mut still_failing, &target_bytes())
        .err()
        .unwrap();
    assert_eq!(err.code, Code::checkpoint_removal_uncertain);
    assert!(!case.checkpoint.exists());
    drop(still_failing);
    let mut retry = FileCheckpoint::open(&ns, Case::path(&case.checkpoint)).unwrap();
    assert!(
        prepared
            .reset_checkpoint(&dev, &mut retry, &target_bytes())
            .unwrap()
            .is_none()
    );
    assert!(!case.checkpoint.exists());
    assert_eq!(fs::read(&case.device).unwrap(), device_before);
    fs::remove_dir_all(&case.dir).unwrap();
}

impl Case {
    fn reset_streamed(&self, target: &str) -> Output {
        return self.reset_with(target, &["--stream"]);
    }
}

#[test]
fn a_streamed_reset_clears_the_checkpoint_writes_nothing_and_the_next_run_applies_again() {
    let case = Case::new(
        "streamed-independent",
        &full_image(0),
        &vec![0u8; new_image().len()],
    );
    assert!(case.apply().status.success());
    fs::write(&case.device, vec![0xa5u8; new_image().len()]).unwrap();
    let out = case.reset_streamed(TARGET_ID);
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert!(
        stderr_of(&out).contains("reset the checkpoint in phase Complete"),
        "{}",
        stderr_of(&out)
    );
    assert!(!case.checkpoint.exists());
    assert_eq!(
        fs::read(&case.device).unwrap(),
        vec![0xa5u8; new_image().len()]
    );
    let out = case.apply();
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert_eq!(fs::read(&case.device).unwrap(), new_image());
    fs::remove_dir_all(&case.dir).unwrap();
}

#[test]
fn a_streamed_reset_refuses_another_target_or_payload_and_keeps_the_checkpoint() {
    let case = Case::new(
        "streamed-identity",
        &full_image(0),
        &vec![0u8; new_image().len()],
    );
    assert!(case.apply().status.success());
    let before = fs::read(&case.checkpoint).unwrap();
    let out = case.reset_streamed(OTHER_TARGET_ID);
    assert_eq!(out.status.code(), Some(1), "{}", stderr_of(&out));
    assert!(
        stderr_of(&out).contains("another payload or target"),
        "{}",
        stderr_of(&out)
    );
    assert_eq!(fs::read(&case.checkpoint).unwrap(), before);
    let other = case.dir.join("other.bin");
    fs::write(&other, full_image(1)).unwrap();
    let out = run(&[
        "-k",
        PUBLIC_PATH,
        "--lock-dir",
        Case::path(&case.lock_dir),
        "--reset-checkpoint",
        "--stream",
        "--checkpoint",
        Case::path(&case.checkpoint),
        "--target-id",
        TARGET_ID,
        Case::path(&other),
        Case::path(&case.device),
    ]);
    assert_eq!(out.status.code(), Some(1), "{}", stderr_of(&out));
    assert!(
        stderr_of(&out).contains("another payload or target"),
        "{}",
        stderr_of(&out)
    );
    assert_eq!(fs::read(&case.checkpoint).unwrap(), before);
    fs::remove_dir_all(&case.dir).unwrap();
}

#[test]
fn a_streamed_base_dependent_reset_is_refused_while_the_device_holds_the_new_image_and_allowed_once_restored()
 {
    let case = Case::new("streamed-base", &delta(), &old_image());
    assert!(case.apply().status.success());
    let before = fs::read(&case.checkpoint).unwrap();
    let out = case.reset_streamed(TARGET_ID);
    assert_eq!(out.status.code(), Some(1), "{}", stderr_of(&out));
    assert!(
        stderr_of(&out).contains("does not match old partition hash"),
        "{}",
        stderr_of(&out)
    );
    assert_eq!(fs::read(&case.checkpoint).unwrap(), before);
    assert_eq!(fs::read(&case.device).unwrap(), delta_image());
    fs::write(&case.device, old_image()).unwrap();
    let out = case.reset_streamed(TARGET_ID);
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert!(!case.checkpoint.exists());
    assert_eq!(fs::read(&case.device).unwrap(), old_image());
    fs::remove_dir_all(&case.dir).unwrap();
}

#[test]
fn a_streamed_reset_is_refused_while_another_process_holds_the_device_and_keeps_the_checkpoint() {
    let case = Case::new(
        "streamed-held",
        &full_image(0),
        &vec![0u8; new_image().len()],
    );
    assert!(case.apply().status.success());
    let before = fs::read(&case.checkpoint).unwrap();
    let held = hold_device_lock(&case.device, &case.dir);
    let out = case.reset_streamed(TARGET_ID);
    assert_eq!(out.status.code(), Some(1), "{}", stderr_of(&out));
    assert!(
        stderr_of(&out).contains("another update holds"),
        "{}",
        stderr_of(&out)
    );
    assert_eq!(fs::read(&case.checkpoint).unwrap(), before);
    drop(held);
    fs::remove_dir_all(&case.dir).unwrap();
}

#[test]
fn a_streamed_reset_needs_no_payload_body_while_a_buffered_reset_must_read_the_whole_payload() {
    let case = Case::new(
        "streamed-body",
        &full_image(0),
        &vec![0u8; new_image().len()],
    );
    assert!(case.apply().status.success());
    let bytes = fs::read(&case.payload).unwrap();
    let blobs_start = delta_update_payload::layout::parse_unverified(&bytes, &EMBEDDED)
        .ok()
        .unwrap()
        .parsed()
        .info
        .blobs_start as usize;
    fs::write(&case.payload, &bytes[..blobs_start]).unwrap();
    let out = case.reset(TARGET_ID);
    assert_eq!(out.status.code(), Some(1), "{}", stderr_of(&out));
    assert!(case.checkpoint.exists());
    let out = case.reset_streamed(TARGET_ID);
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert!(!case.checkpoint.exists());
    fs::remove_dir_all(&case.dir).unwrap();
}

#[test]
fn a_streamed_reset_of_a_payload_over_the_buffered_limit_succeeds_where_the_buffered_reset_is_refused()
 {
    let case = Case::new(
        "streamed-over-limit",
        &full_image(0),
        &vec![0u8; new_image().len()],
    );
    assert!(case.apply().status.success());
    let out = case.reset_with(TARGET_ID, &["--max-buffered-payload", "1024"]);
    assert_eq!(out.status.code(), Some(1), "{}", stderr_of(&out));
    assert!(
        stderr_of(&out).contains("more than the limit of 1024 bytes"),
        "{}",
        stderr_of(&out)
    );
    assert!(case.checkpoint.exists());
    let out = case.reset_with(TARGET_ID, &["--stream", "--max-buffered-payload", "1024"]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert!(!case.checkpoint.exists());
    fs::remove_dir_all(&case.dir).unwrap();
}

#[test]
fn a_streamed_reset_with_no_checkpoint_confirms_the_absence_and_changes_nothing() {
    let case = Case::new(
        "streamed-none",
        &full_image(0),
        &vec![0x5au8; new_image().len()],
    );
    let out = case.reset_streamed(TARGET_ID);
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert!(
        stderr_of(&out).contains("no checkpoint to reset"),
        "{}",
        stderr_of(&out)
    );
    assert!(!case.checkpoint.exists());
    assert_eq!(
        fs::read(&case.device).unwrap(),
        vec![0x5au8; new_image().len()]
    );
    fs::remove_dir_all(&case.dir).unwrap();
}
