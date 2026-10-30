use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use delta_update_cli::filecheckpoint::FileCheckpoint;
use delta_update_cli::filelock::LockDir;
use delta_update_client::checkpoint::CheckpointStore;
use delta_update_client::checkpoint::RECORD_SIZE;

struct UmaskGuard(libc::mode_t);

impl Drop for UmaskGuard {
    fn drop(&mut self) {
        unsafe {
            libc::umask(self.0);
        }
    }
}

fn mode(path: &Path) -> u32 {
    return fs::metadata(path).unwrap().permissions().mode() & 0o777;
}

fn refuse_mode(_file: &fs::File) -> std::io::Result<()> {
    return Err(std::io::Error::other("injected mode failure"));
}

#[test]
fn lock_and_record_files_are_0600_and_reopen_under_a_restrictive_umask() {
    let root = std::env::temp_dir().join(format!("ota-umask-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    let ns_path = root.join("locks");
    let state = root.join("state");
    fs::create_dir_all(&ns_path).unwrap();
    fs::create_dir_all(&state).unwrap();
    fs::set_permissions(&ns_path, fs::Permissions::from_mode(0o700)).unwrap();
    let ns = LockDir::open(&ns_path).unwrap();
    let record = state.join("ckpt").to_str().unwrap().to_string();
    let _restore = UmaskGuard(unsafe { libc::umask(0o277) });
    assert!(ns.take_with("y.lock", refuse_mode).is_err());
    assert!(!ns_path.join("y.lock").exists());
    drop(ns.take("x.lock").unwrap());
    assert_eq!(mode(&ns_path.join("x.lock")), 0o600);
    drop(ns.take("x.lock").unwrap());
    assert_eq!(fs::read_dir(&ns_path).unwrap().count(), 1);
    {
        let mut store = FileCheckpoint::open(&ns, &record).unwrap();
        store.replace(&[1u8; RECORD_SIZE]).unwrap();
    }
    assert_eq!(mode(&state.join("ckpt")), 0o600);
    let store = FileCheckpoint::open(&ns, &record).unwrap();
    let mut out = [0u8; RECORD_SIZE];
    assert!(store.load(&mut out).unwrap());
    assert_eq!(out, [1u8; RECORD_SIZE]);
    drop(store);
    drop(_restore);
    fs::remove_dir_all(&root).unwrap();
}
