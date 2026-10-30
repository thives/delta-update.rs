//! A regular host file used as the read-only old image of `SOURCE_COPY`. It is
//! the read side of [`FileDevice`](filedevice.rs) without the write side: every
//! access is a positional read, and the file is never opened for writing, so a
//! copy cannot change the image it reads.
//!
//! The image is read for the whole run, so it is locked for the whole run, shared. A writer that names the same identity, as
//! in two steps: [`UnlockedImage::open`], then [`UnlockedImage::lock_shared`] once the run has compared identities.

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
use ota_client::image::OldImage;
use ota_client::sink::check_range;
use ota_client::sink::io_error;
use ota_payload::error::PayloadError;

use std::os::unix::fs::FileExt;

pub struct FileImage {
    file: fs::File,
    capacity: u64,
    _lock: Option<FileLock>,
}

/// An old image opened read-only, before its shared lock is taken. Its identity is read from this handle, so a run compares it
/// with its other roles before it attempts any lock (`roles.rs`).
pub struct UnlockedImage {
    file: fs::File,
    path: String,
}

impl UnlockedImage {
    /// Open an existing file read-only. It must be a regular file or a block device, since those are the only files with a lock.
    /// `O_NONBLOCK` stops the open from waiting for a writer when the name is a named pipe, which is then refused, as the lock
    /// namespace refuses one (`filelock.rs`).
    pub fn open(path: String) -> Result<UnlockedImage, String> {
        let entry = role_entry(&path)?;
        return Self::open_entry(path, &entry);
    }

    /// Open the image through the same pinned directory used for its entry lock.
    pub fn open_entry(path: String, entry: &PinnedEntry) -> Result<UnlockedImage, String> {
        let file = match entry.open(libc::O_RDONLY) {
            Err(e) => return Err(format!("{}: {}", path, e)),
            Ok(f) => f,
        };
        key_of(&file, &path)?;
        return Ok(UnlockedImage {
            file: file,
            path: path,
        });
    }

    /// The identity of the open image, read from its handle, for the alias check before any lock or write.
    pub fn identity(&self) -> Result<Identity, String> {
        return Identity::of(&self.file).map_err(|e| format!("old image: {}", e));
    }

    /// Take the image's shared lock, without waiting, and return the image ready for reading. The lock is on the handle that
    /// is read, so a writer that names the same identity contends with it for as long as the run reads.
    pub fn lock_shared(self, lock_dir: &LockDir) -> Result<FileImage, String> {
        let lock = lock_role(&self.file, &self.path, LockMode::Shared, lock_dir)?;
        let mut file = self.file;
        let capacity = match file.seek(io::SeekFrom::End(0)) {
            Err(e) => return Err(format!("{}: {}", self.path, e)),
            Ok(s) => s,
        };
        return Ok(FileImage {
            file: file,
            capacity: capacity,
            _lock: lock,
        });
    }
}

impl OldImage for FileImage {
    fn capacity(self: &FileImage) -> u64 {
        return self.capacity;
    }

    fn read_at(self: &FileImage, offset: u64, out: &mut [u8]) -> Result<(), PayloadError> {
        check_range(offset, out.len(), self.capacity)?;
        return match self.file.read_exact_at(out, offset) {
            Err(_e) => Err(io_error()),
            Ok(()) => Ok(()),
        };
    }
}
