
use std::fs;
use std::fs::DirBuilder;
use std::fs::OpenOptions;
use std::os::unix::fs::DirBuilderExt;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use ota_cli::filecheckpoint::FileCheckpoint;
use ota_cli::filelock::LockDir;
use ota_client::checkpoint::CheckpointStore;
use ota_client::checkpoint::RECORD_SIZE;

const STAGING_INIT: &str = ".ota-checkpoint.staging.init";
const STAGING: &str = ".ota-checkpoint.staging";
const MARKER: &str = "owner";
const MARKER_CONTENT: &[u8] = b"ota-checkpoint staging directory v1\n";

struct UmaskGuard(libc::mode_t);

impl Drop for UmaskGuard {
    fn drop(&mut self) {
        unsafe {
            libc::umask(self.0);
        }
    }
}

#[derive(Clone, Copy, Debug)]
enum State {
    Created,
    MarkerCreated,
    MarkerTorn,
    MarkerWritten,
}

const STATES: [State; 4] = [
    State::Created,
    State::MarkerCreated,
    State::MarkerTorn,
    State::MarkerWritten,
];

fn interrupted(dir: &Path, state: State) {
    let init = dir.join(STAGING_INIT);
    DirBuilder::new().mode(0o700).create(&init).unwrap();
    let marker = init.join(MARKER);
    let contents: &[u8] = match state {
        State::Created => return,
        State::MarkerCreated => &[],
        State::MarkerTorn => &MARKER_CONTENT[..5],
        State::MarkerWritten => MARKER_CONTENT,
    };
    fs::set_permissions(&init, fs::Permissions::from_mode(0o700)).unwrap();
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&marker)
        .unwrap();
    std::io::Write::write_all(&mut file, contents).unwrap();
}

fn mode(path: &Path) -> u32 {
    return fs::symlink_metadata(path).unwrap().permissions().mode() & 0o777;
}

#[test]
fn a_staging_directory_interrupted_under_a_restrictive_umask_is_completed_by_the_next_open() {
    let root = std::env::temp_dir().join(format!("ota-staging-umask-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    let ns_path = root.join("locks");
    fs::create_dir_all(&ns_path).unwrap();
    fs::set_permissions(&ns_path, fs::Permissions::from_mode(0o700)).unwrap();
    let ns = LockDir::open(&ns_path).unwrap();
    for umask in [0o022, 0o277, 0o577, 0o377, 0o777] {
        for (i, state) in STATES.into_iter().enumerate() {
            let dir = root.join(format!("{umask:o}-{i}"));
            fs::create_dir_all(&dir).unwrap();
            let record = dir.join("ckpt").to_str().unwrap().to_string();
            let _restore = UmaskGuard(unsafe { libc::umask(umask) });
            interrupted(&dir, state);
            let mut store = FileCheckpoint::open(&ns, &record)
                .unwrap_or_else(|e| panic!("umask {umask:o}, {state:?}: {e}"));
            store.replace(&[7u8; RECORD_SIZE]).unwrap();
            let mut out = [0u8; RECORD_SIZE];
            assert!(store.load(&mut out).unwrap(), "umask {umask:o}, {state:?}");
            assert_eq!(out, [7u8; RECORD_SIZE], "umask {umask:o}, {state:?}");
            drop(store);
            assert!(!dir.join(STAGING_INIT).exists(), "umask {umask:o}, {state:?}");
            assert_eq!(mode(&dir.join(STAGING)), 0o700, "umask {umask:o}, {state:?}");
            assert_eq!(
                mode(&dir.join(STAGING).join(MARKER)),
                0o600,
                "umask {umask:o}, {state:?}"
            );
            assert_eq!(mode(&dir.join("ckpt")), 0o600, "umask {umask:o}, {state:?}");
            let store = FileCheckpoint::open(&ns, &record)
                .unwrap_or_else(|e| panic!("umask {umask:o}, {state:?}, again: {e}"));
            assert!(store.load(&mut out).unwrap());
        }
    }
    fs::remove_dir_all(&root).unwrap();
}
