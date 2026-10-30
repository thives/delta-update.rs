
#![allow(dead_code)]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::path::PathBuf;
use std::process::Child;
use std::process::Command;
use std::process::Stdio;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::time::Duration;
use std::time::Instant;

use ota_cli::filecheckpoint::FileCheckpoint;
use ota_cli::filedevice::FileDevice;
use ota_cli::filelock::LockDir;
use ota_cli::filelock::LockMode;
use ota_cli::filelock::lock_role;
use ota_cli::roles::role_entry;

pub fn namespace(dir: &Path) -> PathBuf {
    let ns = dir.join("lock-dir");
    fs::create_dir_all(&ns).unwrap();
    fs::set_permissions(&ns, fs::Permissions::from_mode(0o700)).unwrap();
    return ns;
}

pub struct Holder {
    child: Child,
    release: PathBuf,
}

pub fn hold_device_lock(device: &Path, dir: &Path) -> Holder {
    return hold(&[("OTA_TEST_HOLD_DEVICE", device)], dir);
}

pub fn hold_checkpoint_lock(checkpoint: &Path, dir: &Path) -> Holder {
    return hold(&[("OTA_TEST_HOLD_CHECKPOINT", checkpoint)], dir);
}

pub fn hold_shared_lock(file: &Path, dir: &Path) -> Holder {
    return hold(&[("OTA_TEST_HOLD_SHARED", file)], dir);
}

pub fn hold_role_entry(file: &Path, dir: &Path, shared: bool) -> Holder {
    let name = if shared {
        "OTA_TEST_HOLD_READER_ENTRY"
    } else {
        "OTA_TEST_HOLD_WRITER_ENTRY"
    };
    return hold(&[(name, file)], dir);
}

fn hold(env: &[(&str, &Path)], dir: &Path) -> Holder {
    static HOLDERS: AtomicUsize = AtomicUsize::new(0);
    let n = HOLDERS.fetch_add(1, Ordering::SeqCst);
    let release = dir.join(format!("holder-{}-{}-release", std::process::id(), n));
    let ready = dir.join(format!("holder-{}-{}-ready", std::process::id(), n));
    let lock_dir = namespace(dir);
    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .args(["--exact", "support::lock_holder_child", "--nocapture"])
        .env("OTA_TEST_HOLD_RELEASE", &release)
        .env("OTA_TEST_HOLD_READY", &ready)
        .env("OTA_TEST_HOLD_LOCK_DIR", &lock_dir)
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    for (name, value) in env {
        command.env(name, value);
    }
    let child = command.spawn().unwrap();
    let start = Instant::now();
    while !ready.exists() {
        assert!(
            start.elapsed() < Duration::from_secs(30),
            "the lock holder never took the lock"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    return Holder {
        child: child,
        release: release,
    };
}

impl Drop for Holder {
    fn drop(&mut self) {
        let _ = fs::write(&self.release, b"");
        let _ = self.child.wait();
    }
}

#[test]
fn lock_holder_child() {
    let device = std::env::var("OTA_TEST_HOLD_DEVICE").ok();
    let checkpoint = std::env::var("OTA_TEST_HOLD_CHECKPOINT").ok();
    let shared = std::env::var("OTA_TEST_HOLD_SHARED").ok();
    let reader_entry = std::env::var("OTA_TEST_HOLD_READER_ENTRY").ok();
    let writer_entry = std::env::var("OTA_TEST_HOLD_WRITER_ENTRY").ok();
    if device.is_none()
        && checkpoint.is_none()
        && shared.is_none()
        && reader_entry.is_none()
        && writer_entry.is_none()
    {
        return;
    }
    let release = PathBuf::from(std::env::var("OTA_TEST_HOLD_RELEASE").unwrap());
    let ready = PathBuf::from(std::env::var("OTA_TEST_HOLD_READY").unwrap());
    let lock_dir =
        LockDir::open(Path::new(&std::env::var("OTA_TEST_HOLD_LOCK_DIR").unwrap())).unwrap();
    let _role_guard = reader_entry.or(writer_entry.clone()).map(|name| {
        let mode = if writer_entry.is_some() {
            LockMode::Exclusive
        } else {
            LockMode::Shared
        };
        let entry = role_entry(&name).unwrap();
        let entry_lock = lock_dir.take_entry(&entry.entry, mode).unwrap();
        let file_lock = if Path::new(&name).exists() {
            let file = fs::File::open(&name).unwrap();
            let lock = lock_role(&file, &name, mode, &lock_dir).unwrap();
            Some((file, lock))
        } else {
            None
        };
        (entry, entry_lock, file_lock)
    });
    let _guard: Option<Box<dyn std::any::Any>> = match (device, checkpoint, shared) {
        (Some(d), _, _) => Some(Box::new(
            FileDevice::open_for_update(&d, &lock_dir).unwrap(),
        )),
        (None, Some(c), _) => Some(Box::new(FileCheckpoint::open(&lock_dir, &c).unwrap())),
        (None, None, Some(s)) => {
            let file = std::fs::File::open(&s).unwrap();
            let lock = lock_role(&file, &s, LockMode::Shared, &lock_dir).unwrap();
            Some(Box::new((file, lock)))
        }
        (None, None, None) => None,
    };
    fs::write(&ready, b"").unwrap();
    let start = Instant::now();
    while !release.exists() && start.elapsed() < Duration::from_secs(120) {
        std::thread::sleep(Duration::from_millis(10));
    }
}
