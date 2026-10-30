//! A regular host file treated as byte-addressed storage: a write names a byte
//! offset in the file. This is how the reference `update_engine` A/B flow drives
//! disk partitions on devices that expose no block API. On real platforms a block
//! adapter (dm-verity, MBR-style handlers) plugs into the same
//! [`Sink`]/[`Readable`](delta-update-client) traits, and owns the alignment and
//! read-modify-write that a block device needs.
//!
//! The capacity is the file's size, read by seeking to the end, so a block device
//! reports its size too. `flush` is a real persistence barrier (`fsync`), not a
//! userspace flush, because `File` has no userspace buffer to drain.
//!
//! An update opens its device in two steps. [`UnlockedDevice::open`] opens it without a lock, so the run can read its
//! identity and compare it with the other roles (`roles.rs`). [`UnlockedDevice::lock_exclusive`] then takes the device's
//! and a block device through its device number, in the host's lock namespace. Before either step the run takes the lock of
//! the *entry* the device names (`filelock.rs::Entry`), so the entry cannot be created, replaced or removed under it

use std::fs;
use std::io;
use std::io::Seek;

use crate::filelock::FileLock;
use crate::filelock::LockDir;
use crate::filelock::LockMode;
use crate::filelock::PinnedEntry;
use crate::filelock::key_of;
use crate::filelock::lock_role;
use crate::identity::Identity;
use crate::roles::role_entry;
use delta_update_client::sink::Readable;
use delta_update_client::sink::Sink;
use delta_update_client::sink::check_range;
use delta_update_client::sink::io_error;
use delta_update_payload::error::PayloadError;

use std::os::unix::fs::FileExt;

/// The persistence primitive used by [`FileDevice::flush`]. A plain function
/// pointer so tests can inject a failing or counting implementation.
pub type SyncFn = fn(&fs::File) -> io::Result<()>;

fn sync_all(file: &fs::File) -> io::Result<()> {
    return file.sync_all();
}

pub struct FileDevice {
    file: fs::File,
    capacity: u64,
    sync: SyncFn,
    _lock: Option<FileLock>,
    _entry: Option<FileLock>,
}

impl FileDevice {
    /// Open an existing file for read-write access. Its size is the capacity. This takes no lock,
    /// so it is for the library's own tests and for dry runs. An update uses
    /// [`UnlockedDevice::open`] and then [`UnlockedDevice::lock_exclusive`].
    pub fn open(path: String) -> Result<FileDevice, io::Error> {
        return FileDevice::open_with_sync(path, sync_all);
    }

    /// As [`open`](FileDevice::open), with the persistence primitive supplied by
    /// the caller.
    pub fn open_with_sync(path: String, sync: SyncFn) -> Result<FileDevice, io::Error> {
        let mut file = fs::OpenOptions::new().read(true).write(true).open(path)?;
        let size = file.seek(io::SeekFrom::End(0))?;
        return Ok(FileDevice {
            file: file,
            capacity: size,
            sync: sync,
            _lock: None,
            _entry: None,
        });
    }

    /// Open the device for an update and take its locks, without waiting. This names the entry the path leads to and takes
    /// its entry lock first ([`role_entry`](roles.rs)), so the entry cannot be created, replaced or removed under this run
    /// A run that names other roles must compare their identities between the last two steps, so it uses them directly
    /// (`roles.rs`).
    pub fn open_for_update(path: &str, lock_dir: &LockDir) -> Result<FileDevice, String> {
        let entry = match role_entry(path) {
            Err(msg) => return Err(msg),
            Ok(entry) => entry,
        };
        let entry_lock = match lock_dir.take_entry(&entry.entry, LockMode::Exclusive) {
            Err(e) => return Err(e.message(path)),
            Ok(lock) => lock,
        };
        let device = match UnlockedDevice::open_entry(path, &entry)?.lock_exclusive(lock_dir) {
            Err(msg) => return Err(msg),
            Ok(device) => device,
        };
        return Ok(FileDevice {
            file: device.file,
            capacity: device.capacity,
            sync: device.sync,
            _lock: device._lock,
            _entry: Some(entry_lock),
        });
    }
}

/// A device opened for an update, before its lock is taken. Its identity is read from this handle, so a run compares it with
/// its other roles before it attempts any lock. A lock attempted on another role's file would meet that role's lock, and be
/// reported as another update, when it is really the alias that must be refused (`roles.rs`).
pub struct UnlockedDevice {
    file: fs::File,
    path: String,
}

impl UnlockedDevice {
    /// Open the device read-write. It must be a regular file or a block device, since those are the only files with a lock.
    /// `O_NONBLOCK` stops the open from waiting for a writer when the name is a named pipe, which is then refused.
    pub fn open(path: &str) -> Result<UnlockedDevice, String> {
        let entry = role_entry(path)?;
        return Self::open_entry(path, &entry);
    }

    /// Open the device through the same pinned directory used for its entry lock.
    pub fn open_entry(path: &str, entry: &PinnedEntry) -> Result<UnlockedDevice, String> {
        let file = match entry.open(libc::O_RDWR) {
            Err(e) => return Err(e.to_string()),
            Ok(f) => f,
        };
        key_of(&file, path)?;
        return Ok(UnlockedDevice {
            file: file,
            path: path.to_string(),
        });
    }

    /// The identity of the open device, read from its handle. A regular file and a block device are compared by their own rules
    /// (`identity.rs`).
    pub fn identity(&self) -> Result<Identity, String> {
        return Identity::of(&self.file).map_err(|e| format!("device {}: {}", self.path, e));
    }

    /// Take the device's exclusive lock, without waiting, and return the device ready for writing. The lock is taken on the
    /// handle that is then written, so it follows the device and not the name:
    ///
    /// - A regular file is locked on its own open file. Every path and hard link to it contends.
    /// - A block device is locked through the file `block-<major>-<minor>.lock` in the lock namespace `lock_dir`. Every node
    ///   for one device shares the lock, and a different device, or another partition, does not.
    ///
    /// A held lock is reported as `another update holds <path>`.
    pub fn lock_exclusive(self, lock_dir: &LockDir) -> Result<FileDevice, String> {
        let lock = lock_role(&self.file, &self.path, LockMode::Exclusive, lock_dir)?;
        let mut file = self.file;
        let capacity = match file.seek(io::SeekFrom::End(0)) {
            Err(e) => return Err(e.to_string()),
            Ok(s) => s,
        };
        return Ok(FileDevice {
            file: file,
            capacity: capacity,
            sync: sync_all,
            _lock: lock,
            _entry: None,
        });
    }
}

impl Sink for FileDevice {
    fn capacity(self: &FileDevice) -> u64 {
        return self.capacity;
    }

    fn write_at(self: &mut FileDevice, offset: u64, data: &[u8]) -> Result<(), PayloadError> {
        check_range(offset, data.len(), self.capacity)?;
        return match self.file.write_all_at(data, offset) {
            Err(_e) => Err(io_error()),
            Ok(()) => Ok(()),
        };
    }

    fn flush(self: &mut FileDevice) -> Result<(), PayloadError> {
        return match (self.sync)(&self.file) {
            Err(_e) => Err(io_error()),
            Ok(()) => Ok(()),
        };
    }
}

impl Readable for FileDevice {
    fn read_at(self: &FileDevice, offset: u64, out: &mut [u8]) -> Result<(), PayloadError> {
        check_range(offset, out.len(), self.capacity)?;
        return match self.file.read_exact_at(out, offset) {
            Err(_e) => Err(io_error()),
            Ok(()) => Ok(()),
        };
    }
}

#[cfg(test)]
mod tests {
    extern crate std;

    use super::*;
    use std::format;
    use std::string::String;
    use std::sync::atomic::AtomicUsize;
    use std::sync::atomic::Ordering;

    fn temp_file(name: &str, len: usize) -> String {
        let path =
            std::env::temp_dir().join(format!("ota-filedevice-{}-{}", std::process::id(), name));
        fs::write(&path, vec![0x5au8; len]).unwrap();
        return path.to_str().unwrap().to_string();
    }

    fn failing_sync(_file: &fs::File) -> io::Result<()> {
        return Err(io::Error::other("injected sync failure"));
    }

    static SYNCS: AtomicUsize = AtomicUsize::new(0);

    fn counting_sync(file: &fs::File) -> io::Result<()> {
        SYNCS.fetch_add(1, Ordering::SeqCst);
        return file.sync_all();
    }

    #[test]
    fn capacity_is_the_file_size_whatever_its_length() {
        let path = temp_file("odd-size", 4 * 8 + 1);
        let dev = FileDevice::open(path).unwrap();
        assert_eq!(dev.capacity(), 33);
    }

    #[test]
    fn unaligned_writes_land_at_their_byte_offsets_and_nowhere_else() {
        let path = temp_file("unaligned", 32);
        let mut dev = FileDevice::open(path.clone()).unwrap();
        dev.write_at(5, &[1, 2, 3]).unwrap();
        let mut back = [0u8; 3];
        dev.read_at(5, &mut back).unwrap();
        assert_eq!(back, [1, 2, 3]);
        let all = fs::read(&path).unwrap();
        assert_eq!(&all[..5], &[0x5a; 5]);
        assert_eq!(&all[8..], &[0x5a; 24]);
    }

    #[test]
    fn a_write_past_the_end_is_rejected_without_growing_the_file() {
        let path = temp_file("past-end", 4 * 8);
        let mut dev = FileDevice::open(path.clone()).unwrap();
        assert!(dev.write_at(30, &[0u8; 4]).is_err());
        assert_eq!(fs::metadata(&path).unwrap().len(), 32);
    }

    #[test]
    fn flush_propagates_sync_failure() {
        let path = temp_file("sync-fail", 4 * 8);
        let mut dev = FileDevice::open_with_sync(path, failing_sync).unwrap();
        dev.write_at(0, &[1u8; 8]).unwrap();
        assert!(dev.flush().is_err());
    }

    #[test]
    fn flush_syncs_the_file_after_writes() {
        let path = temp_file("sync-ok", 4 * 8);
        let before = SYNCS.load(Ordering::SeqCst);
        let mut dev = FileDevice::open_with_sync(path, counting_sync).unwrap();
        dev.write_at(0, &[1u8; 8]).unwrap();
        dev.flush().unwrap();
        assert_eq!(SYNCS.load(Ordering::SeqCst), before + 1);
    }

    fn namespace(name: &str) -> (std::path::PathBuf, LockDir) {
        let dir =
            std::env::temp_dir().join(format!("ota-filedevice-ns-{}-{}", std::process::id(), name));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        fs::set_permissions(&dir, std::os::unix::fs::PermissionsExt::from_mode(0o700)).unwrap();
        let ns = LockDir::open(&dir).unwrap();
        return (dir, ns);
    }

    #[test]
    fn a_regular_file_is_locked_through_a_hard_link_and_released_on_drop() {
        let (_dir, ns) = namespace("link");
        let path = temp_file("lock-link", 4 * 8);
        let link = format!("{}-link", path);
        fs::hard_link(&path, &link).unwrap();
        let held = FileDevice::open_for_update(&path, &ns).unwrap();
        let refused = FileDevice::open_for_update(&link, &ns).err().unwrap();
        assert!(refused.contains("another update holds"), "{}", refused);
        drop(held);
        assert!(FileDevice::open_for_update(&link, &ns).is_ok());
        fs::remove_file(&link).unwrap();
    }

    #[test]
    fn a_different_regular_file_is_not_blocked_by_the_lock_on_another() {
        let (_dir, ns) = namespace("other");
        let first = temp_file("lock-first", 4 * 8);
        let second = temp_file("lock-second", 4 * 8);
        let _held = FileDevice::open_for_update(&first, &ns).unwrap();
        assert!(FileDevice::open_for_update(&second, &ns).is_ok());
    }

    #[test]
    fn a_regular_file_is_locked_on_its_own_file_and_creates_no_lock_for_its_identity() {
        let (dir, ns) = namespace("no-lock-file");
        let path = temp_file("lock-own", 4 * 8);
        let _held = FileDevice::open_for_update(&path, &ns).unwrap();
        let names: Vec<String> = fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().to_string())
            .collect();
        assert_eq!(names.len(), 1, "{:?}", names);
        assert!(names[0].starts_with("entry-"), "{:?}", names);
    }

    #[test]
    fn a_device_is_identified_without_a_lock_and_a_second_handle_to_it_is_held_by_the_first() {
        let (_dir, ns) = namespace("unlocked-identity");
        let path = temp_file("unlocked", 4 * 8);
        let link = format!("{}-link", path);
        fs::hard_link(&path, &link).unwrap();
        let unlocked = UnlockedDevice::open(&path).unwrap();
        let other = UnlockedDevice::open(&link).unwrap();
        assert_eq!(unlocked.identity().unwrap(), other.identity().unwrap());
        let held = unlocked.lock_exclusive(&ns).unwrap();
        assert!(
            other
                .lock_exclusive(&ns)
                .err()
                .unwrap()
                .contains("another update holds")
        );
        drop(held);
    }

    #[test]
    fn a_device_that_is_neither_a_regular_file_nor_a_block_device_is_refused_before_any_lock() {
        let (dir, _ns) = namespace("not-lockable");
        let fifo = dir.join("pipe");
        let name = std::ffi::CString::new(fifo.to_str().unwrap()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
        let refused = UnlockedDevice::open(fifo.to_str().unwrap()).err().unwrap();
        assert!(
            refused.contains("neither a regular file nor a block device"),
            "{}",
            refused
        );
    }
}
