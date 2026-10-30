
mod support;

use std::fs;
use std::path::Path;
use std::path::PathBuf;

use ota_cli::filecheckpoint::FileCheckpoint;
use ota_cli::filedevice::FileDevice;
use ota_cli::filelock::LockDir;
use ota_client::apply::NoProgress;
use ota_client::apply::Outcome;
use ota_client::apply::Trust;
use ota_client::apply::prepare;
use ota_client::checkpoint::TargetId;
use ota_client::policy::Policy;
use ota_client::policy::VolatileFloor;
use ota_generator::build_full_payload_with_release;
use ota_payload::error::Code;
use ota_payload::error::PayloadError;
use ota_payload::profile::EMBEDDED;
use ota_payload::rsa::parse_pubkey_pem;
use support::namespace;

const PRIVATE: &[u8] = include_bytes!("../../ota-payload/tests/fixtures/private.pem");
const PUBLIC: &[u8] = include_bytes!("../../ota-payload/tests/fixtures/public.pem");
const BS: usize = 4096;
const TARGET: TargetId = [0x5a; 32];

fn scratch_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("ota-durability-{}-{}", std::process::id(), name));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    return dir;
}

fn new_image() -> Vec<u8> {
    return (0..3 * BS as u32).map(|i| (i * 7 % 251) as u8).collect();
}

fn payload() -> Vec<u8> {
    return match build_full_payload_with_release(&new_image(), b"A/B", PRIVATE, BS, &EMBEDDED, 0) {
        Err(e) => panic!("build failed: {}", e.msg),
        Ok(v) => v,
    };
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

fn apply(
    ns: &LockDir,
    payload: &[u8],
    device: &Path,
    checkpoint: &Path,
    fail: bool,
) -> Result<Outcome, PayloadError> {
    let keys = vec![parse_pubkey_pem(PUBLIC).unwrap()];
    let prepared = prepare(payload, Trust::Verified(&keys), &EMBEDDED).unwrap();
    let floor = VolatileFloor::new(0);
    let mut dev = FileDevice::open_for_update(device.to_str().unwrap(), ns).unwrap();
    SYNCS_FAIL.set(false);
    let mut store =
        FileCheckpoint::open_with_dir_sync(ns, checkpoint.to_str().unwrap(), switchable_dir_sync)
            .unwrap();
    SYNCS_FAIL.set(fail);
    return prepared.apply_to_device(&mut dev, &mut store, &TARGET, Policy::Strict(&floor), &NoProgress {});
}

#[test]
fn a_record_whose_rename_is_not_confirmed_is_refused_by_retries_until_a_sync_works_and_writes_nothing_meanwhile() {
    let dir = scratch_dir("confirm");
    let ns = LockDir::open(&namespace(&dir)).unwrap();
    let device = dir.join("dev.img");
    let checkpoint = dir.join("ck");
    let payload = payload();
    fs::write(&device, vec![0x11u8; new_image().len()]).unwrap();
    let err = apply(&ns, &payload, &device, &checkpoint, true).err().unwrap();
    assert_eq!(err.code, Code::device_io);
    assert_eq!(fs::read(&device).unwrap(), vec![0x11u8; new_image().len()]);
    assert!(checkpoint.exists());
    for _ in 0..2 {
        let err = apply(&ns, &payload, &device, &checkpoint, true).err().unwrap();
        assert_eq!(err.code, Code::checkpoint_unconfirmed);
        assert_eq!(fs::read(&device).unwrap(), vec![0x11u8; new_image().len()]);
    }
    assert_eq!(
        apply(&ns, &payload, &device, &checkpoint, false).unwrap(),
        Outcome::Verified
    );
    assert_eq!(fs::read(&device).unwrap(), new_image());
    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn a_power_cut_that_loses_the_first_rename_leaves_no_record_and_the_next_run_completes_the_update() {
    let dir = scratch_dir("rename-lost");
    let ns = LockDir::open(&namespace(&dir)).unwrap();
    let device = dir.join("dev.img");
    let checkpoint = dir.join("ck");
    let payload = payload();
    fs::write(&device, vec![0x22u8; new_image().len()]).unwrap();
    assert!(apply(&ns, &payload, &device, &checkpoint, true).is_err());
    fs::remove_file(&checkpoint).unwrap();
    assert_eq!(fs::read(&device).unwrap(), vec![0x22u8; new_image().len()]);
    assert_eq!(
        apply(&ns, &payload, &device, &checkpoint, false).unwrap(),
        Outcome::Verified
    );
    assert_eq!(fs::read(&device).unwrap(), new_image());
    fs::remove_dir_all(&dir).unwrap();
}
