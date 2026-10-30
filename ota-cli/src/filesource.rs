//! A regular host file treated as a payload [`Source`](ota-client): sequential positional reads with an internal cursor,
//! touches files, and payload *transport* (fetching or storing the package) remains out of project scope: a host that streams
//! from a download, a decompressed chunk chain or a RAM buffer implements the same one-method trait.
//!
//! A streamed run pins its payload with [`PayloadPin`], which takes the payload's entry lock and its shared identity lock
//! before the first byte is read, and keeps a handle of its own for the whole run. That handle, and so both locks, outlives
//! `Prepared::apply`: a payload replaced or removed while the final verification or the checkpoint's completion record is

use std::fs;
use std::io;

use crate::filelock::Entry;
use crate::filelock::FileLock;
use crate::filelock::LockDir;
use crate::filelock::LockMode;
use crate::filelock::PinnedEntry;
use crate::filelock::lock_role;
use crate::roles::role_entry;
use ota_client::sink::io_error;
use ota_client::source::Source;
use ota_payload::error::PayloadError;

use std::os::unix::fs::FileExt;

pub struct FileSource {
    pub file: fs::File,
    pub offset: u64,
}

impl FileSource {
    /// Open an existing payload file for reading; the cursor starts at 0.
    pub fn open(path: String) -> Result<FileSource, io::Error> {
        return FileSource::open_at(path, 0);
    }

    /// Open an existing payload file with the cursor at `offset`. A resumed run
    /// starts here, at the byte its checkpoint names.
    pub fn open_at(path: String, offset: u64) -> Result<FileSource, io::Error> {
        let file = match fs::OpenOptions::new().read(true).open(path) {
            Err(e) => return Err(e),
            Ok(f) => f,
        };
        return Ok(FileSource::from_file(file, offset));
    }

    /// The payload read from a file that is already open, with the cursor at `offset`. A run that authenticates a
    /// payload opens every later pass through a copy of that handle, so the bytes read are the bytes that were checked,
    /// whatever the path names by then.
    pub fn from_file(file: fs::File, offset: u64) -> FileSource {
        return FileSource {
            file: file,
            offset: offset,
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_copy_of_the_pinned_handle_reads_the_file_that_was_opened_after_its_path_is_replaced() {
        let dir = std::env::temp_dir().join(format!("ota-filesource-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("payload");
        fs::write(&path, b"authenticated").unwrap();
        let pinned = fs::File::open(&path).unwrap();
        fs::write(dir.join("other"), b"substituted").unwrap();
        fs::rename(dir.join("other"), &path).unwrap();
        let mut src = FileSource::from_file(pinned.try_clone().unwrap(), 0);
        let mut out = [0u8; 13];
        let n = src.read(&mut out).unwrap();
        assert_eq!(&out[..n], b"authenticated");
        fs::remove_dir_all(&dir).unwrap();
    }

    use crate::filecheckpoint::FileCheckpoint;
    use crate::roles;
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;

    fn pinned_case(name: &str) -> (PathBuf, LockDir) {
        let dir =
            std::env::temp_dir().join(format!("ota-filesource-{}-{}", std::process::id(), name));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("payload"), b"authenticated payload").unwrap();
        let ns_path = dir.join("lock-dir");
        fs::create_dir_all(&ns_path).unwrap();
        fs::set_permissions(&ns_path, fs::Permissions::from_mode(0o700)).unwrap();
        return (dir, LockDir::open(&ns_path).unwrap());
    }

    #[test]
    fn payload_pin_opens_the_locked_parent_after_it_is_retargeted() {
        use std::os::unix::fs::symlink;
        let (dir, ns) = pinned_case("pin-parent-retarget");
        fs::create_dir(dir.join("first")).unwrap();
        fs::create_dir(dir.join("second")).unwrap();
        fs::write(dir.join("first/payload"), b"original").unwrap();
        fs::write(dir.join("second/payload"), b"substitute").unwrap();
        symlink(dir.join("first"), dir.join("link")).unwrap();
        let path = dir.join("link/payload").to_str().unwrap().to_string();
        let entry = role_entry(&path).unwrap();
        fs::remove_file(dir.join("link")).unwrap();
        symlink(dir.join("second"), dir.join("link")).unwrap();
        let pin = PayloadPin::open_entry(&path, &ns, entry).unwrap();
        let mut source = pin.source_at(0).unwrap();
        let mut bytes = [0u8; 10];
        let n = source.read(&mut bytes).unwrap();
        assert_eq!(&bytes[..n], b"original");
        assert!(FileCheckpoint::open(&ns, dir.join("first/payload").to_str().unwrap()).is_err());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn payload_pin_refuses_a_final_symlink_without_touching_the_target() {
        use std::os::unix::fs::symlink;
        let (dir, ns) = pinned_case("pin-final-symlink");
        symlink(dir.join("payload"), dir.join("alias")).unwrap();
        let path = dir.join("alias").to_str().unwrap().to_string();
        assert!(PayloadPin::open(&path, &ns).is_err());
        assert_eq!(
            fs::read(dir.join("payload")).unwrap(),
            b"authenticated payload"
        );
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_pin_holds_the_payloads_identity_lock_until_it_is_dropped() {
        let (dir, ns) = pinned_case("pin-identity-lock");
        let payload = dir.join("payload").to_str().unwrap().to_string();
        let pin = PayloadPin::open(&payload, &ns).unwrap();
        let writer = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&dir.join("payload"))
            .unwrap();
        let refused = lock_role(&writer, &payload, LockMode::Exclusive, &ns)
            .err()
            .unwrap();
        assert!(refused.contains("another update holds"), "{}", refused);
        let reader = fs::File::open(&dir.join("payload")).unwrap();
        assert!(
            lock_role(&reader, &payload, LockMode::Shared, &ns)
                .unwrap()
                .is_none()
        );
        let source = pin.source_at(0).unwrap();
        let copied = lock_role(&source.file, &payload, LockMode::Exclusive, &ns)
            .err()
            .unwrap();
        assert!(copied.contains("another update holds"), "{}", copied);
        drop(source);
        drop(reader);
        drop(pin);
        assert!(lock_role(&writer, &payload, LockMode::Exclusive, &ns).is_ok());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_pin_holds_the_payloads_entry_lock_both_ways_against_another_run() {
        let (dir, ns) = pinned_case("pin-entry-lock");
        let payload = dir.join("payload").to_str().unwrap().to_string();
        let pin = PayloadPin::open(&payload, &ns).unwrap();
        let refused = FileCheckpoint::open(&ns, &payload).err().unwrap();
        assert!(refused.contains("another update holds"), "{}", refused);
        drop(pin);
        assert!(FileCheckpoint::open(&ns, &payload).is_ok());
        let store = FileCheckpoint::open(&ns, &payload).unwrap();
        let refused = PayloadPin::open(&payload, &ns).err().unwrap();
        assert!(refused.contains("another update holds"), "{}", refused);
        drop(store);
        assert!(PayloadPin::open(&payload, &ns).is_ok());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_payload_may_name_no_entry_under_the_stores_reserved_prefix() {
        let (dir, ns) = pinned_case("pin-reserved");
        let planted = dir.join(".ota-checkpoint.stage.4242-1-2");
        fs::write(&planted, b"another store's staging file").unwrap();
        let name = planted.to_str().unwrap().to_string();
        let refused = PayloadPin::open(&name, &ns).err().unwrap();
        assert!(refused.contains("updater's own namespace"), "{}", refused);
        assert_eq!(fs::read(&planted).unwrap(), b"another store's staging file");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_device_that_is_the_pinned_payload_is_refused_as_an_alias_and_not_as_a_held_lock() {
        let (dir, ns) = pinned_case("pin-alias");
        let payload = dir.join("payload").to_str().unwrap().to_string();
        let pin = PayloadPin::open(&payload, &ns).unwrap();
        let ck = dir.join("ck").to_str().unwrap().to_string();
        let names = roles::Names {
            checkpoint: &ck,
            device: &payload,
            source: None,
        };
        let refused = roles::open(&ns, pin.file(), &names, Some(&pin.entry()))
            .err()
            .unwrap();
        assert!(
            refused.contains("the payload and the device are the same file"),
            "{}",
            refused
        );
        fs::remove_dir_all(&dir).unwrap();
    }
}

impl Source for FileSource {
    fn read(self: &mut FileSource, out: &mut [u8]) -> Result<usize, PayloadError> {
        let n = match self.file.read_at(out, self.offset) {
            Err(_e) => return Err(io_error()),
            Ok(v) => v,
        };
        self.offset += n as u64;
        return Ok(n);
    }
}

/// The payload of a streamed run, pinned for the whole run: opened once, and locked by name and by identity before its
/// first byte is read. Both locks, and the handle that carries them, are held until the run's `apply` has returned, which
pub struct PayloadPin {
    file: fs::File,
    entry: Entry,
    _entry_lock: FileLock,
    _role_lock: Option<FileLock>,
}

impl PayloadPin {
    /// Open `path` for reading as a run's payload and take its locks, without waiting. The entry lock is taken first, so
    /// the entry cannot be created, renamed over or removed under this run, and then the file behind that entry is opened;
    /// the identity lock follows the file that was opened, not the name (`filelock.rs`). A payload may not name an entry
    /// under the store's reserved prefix, like any other role (`roles.rs::role_entry`).
    pub fn open(path: &str, ns: &LockDir) -> Result<PayloadPin, String> {
        let entry = match role_entry(path) {
            Err(msg) => return Err(msg),
            Ok(entry) => entry,
        };
        return Self::open_entry(path, ns, entry);
    }

    fn open_entry(path: &str, ns: &LockDir, entry: PinnedEntry) -> Result<PayloadPin, String> {
        let entry_lock = match ns.take_entry(&entry.entry, LockMode::Shared) {
            Err(e) => return Err(e.message(path)),
            Ok(lock) => lock,
        };
        let file = match entry.open(libc::O_RDONLY) {
            Err(e) => return Err(format!("cannot open the payload {}: {}", path, e)),
            Ok(f) => f,
        };
        let role_lock = match lock_role(&file, path, LockMode::Shared, ns) {
            Err(msg) => return Err(msg),
            Ok(lock) => lock,
        };
        return Ok(PayloadPin {
            file: file,
            entry: entry.entry,
            _entry_lock: entry_lock,
            _role_lock: role_lock,
        });
    }

    /// The pinned handle. A run compares it with its other roles by identity (`roles::open`), and reads every later pass
    /// through a copy of it, so the bytes read are the bytes that were authenticated whatever the path becomes.
    pub fn file(self: &PayloadPin) -> &fs::File {
        return &self.file;
    }

    /// The entry this pin holds the lock of, for `roles::open`, which must not take that name twice against one run.
    pub fn entry(self: &PayloadPin) -> Entry {
        return Entry {
            dir: self.entry.dir,
            name: self.entry.name.clone(),
        };
    }

    /// A source that reads the payload from `offset`, through a copy of the pinned handle. The locks stay with the pin, so
    /// a source made here, and the run that drops it, neither take nor release anything.
    pub fn source_at(self: &PayloadPin, offset: u64) -> Result<FileSource, String> {
        return match self.file.try_clone() {
            Err(e) => Err(format!("cannot read the payload again: {}", e)),
            Ok(copy) => Ok(FileSource::from_file(copy, offset)),
        };
    }
}
