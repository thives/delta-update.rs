//! Advisory locks on lock files and on role files, taken without waiting, and the lock namespace that holds them.
//!
//! once per run into a [`LockDir`], and the open handle is validated. Every lock file of the run is then opened
//! relative to that handle, so a later rename or replacement of the path cannot redirect a lock. A lock file is
//! opened without following a link. It must be a regular file with one name, owned by the caller, and accessible
//! to no one else. A new lock file is made private before its name exists, so no run can open one that is not
//! (`LockDir::take_in`). A lock file is never removed: a removal would let a second run lock a new file while the
//! first run still held the old one. The kernel releases a lock when its holder exits, so a crash leaves nothing held.
//!
//! the exclusive lock. A reader takes the shared one, so two readers do not contend, and a reader and a writer of one
//! identity do. A regular file is locked on its own open file, and a block device through its lock file in the
//! namespace ([`lock_role`]). The locks are advisory, and they stop only processes that take them.
//!
//! An identity lock follows an inode, or a device number, so it cannot say anything about a name that has no entry,
//! or about an entry another run is creating, renaming over or removing ([`Entry`], `docs/locking-and-recovery.md`
//! directory's identity and the entry's name, so one run's device, old image or streamed payload and another run's
//! checkpoint record contend for it whichever of them exists first, and neither can have the entry replaced under it.
//! The two locks together cover the two ways one file can be named twice: an entry lock covers one name in one directory
//! (including its absence), and an identity lock covers one inode reached through other paths, links and device nodes.

use std::ffi::CString;
use std::fs;
use std::io;
use std::os::unix::fs::MetadataExt;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::io::AsRawFd;
use std::os::unix::io::FromRawFd;
use std::path::Path;
use std::path::PathBuf;

use ota_payload::sha256;

/// Why a lock could not be taken.
#[derive(Debug)]
pub enum LockError {
    /// Another process holds the lock. Nothing waits for it.
    Held,
    /// The lock cannot be taken safely. The text says why, for the operator.
    Refused(String),
}

impl LockError {
    /// The message an operator sees for a lock that was not taken. `what` names the thing it protects.
    pub fn message(&self, what: &str) -> String {
        return match self {
            LockError::Held => format!("another update holds {}", what),
            LockError::Refused(reason) => format!("cannot lock {}: {}", what, reason),
        };
    }
}

/// The kind of lock a role takes. A writer takes the exclusive lock, which excludes every other lock on its identity. A
/// reader takes the shared lock, which excludes an exclusive one but not another reader.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LockMode {
    Shared,
    Exclusive,
}

/// What a role's lock is keyed by. It is chosen from the open handle, not from its path.
#[derive(Debug, PartialEq)]
pub enum LockKey {
    /// A regular file. Its own open file carries the lock, so every path and hard link to the file shares it.
    Regular,
    /// A block device. Every device node for one device carries the same device number, so the lock is a file named for
    /// that number, and nodes with other numbers do not contend.
    Block { major: u32, minor: u32 },
}

/// The lock a file with this `st_mode` and `st_rdev` needs. Any other file type has no identity this tool can lock, so
/// it is refused.
pub fn lock_key(mode: u32, rdev: u64) -> Result<LockKey, &'static str> {
    return match mode & libc::S_IFMT {
        libc::S_IFREG => Ok(LockKey::Regular),
        libc::S_IFBLK => Ok(LockKey::Block {
            major: libc::major(rdev),
            minor: libc::minor(rdev),
        }),
        _ => Err(
            "is neither a regular file nor a block device, which are the only files that can be locked",
        ),
    };
}

/// The name of a block device's lock file in the lock namespace.
pub fn block_lock_file_name(major: u32, minor: u32) -> String {
    return format!("block-{}-{}.lock", major, minor);
}

/// A directory entry a role names: its name, and the identity of the directory it is in.
///
/// Unlike an [`Identity`](identity.rs), which names a storage object, this names a *place in a directory*: it says
/// nothing about whether the entry exists, and it is the same lock whether the entry is absent, being created, being
/// renamed over or being removed. That is what lets a checkpoint record and another run's role refuse each other before
pub struct Entry {
    /// The device and inode of the directory that holds the entry, read from an open handle of it.
    pub dir: (u64, u64),
    /// The entry's name in that directory: one plain file name.
    pub name: String,
}

impl Entry {
    /// Resolve `path` to the entry its last component names. The directory is opened the way the checkpoint store opens
    /// its record's directory (`filecheckpoint.rs`): following symbolic links in the path, refusing anything that is not
    /// a directory, and read from the open handle, so the identity names the directory the entry is in whatever the
    /// path becomes later.
    ///
    /// A path whose last component is not one plain file name is refused, since such a component names an entry of
    /// no directory. A directory that cannot be opened is refused too: without it the run cannot say which entry it
    pub fn of(path: &str) -> Result<Entry, String> {
        return Ok(PinnedEntry::of(path)?.entry);
    }
}

/// An entry and the open directory from which its identity was derived. Opening through this handle keeps
/// a parent-path rename or symlink retarget from separating the entry lock from the file it protects.
pub struct PinnedEntry {
    pub entry: Entry,
    pub dir: fs::File,
}

impl PinnedEntry {
    pub fn of(path: &str) -> Result<PinnedEntry, String> {
        let record = Path::new(path);
        let raw = match record.file_name() {
            None => return Err(format!("{} names no entry: it has no last component", path)),
            Some(n) => n,
        };
        let name = match raw.to_str() {
            None => return Err(format!("{}: its entry name is not a text name", path)),
            Some(n) => n.to_string(),
        };
        if !is_plain_name(&name) {
            return Err(format!(
                "invalid entry name {:?} in {}: it must be one file name, not empty, `.` or `..`, with no separator",
                name, path
            ));
        }
        let parent = match record.parent() {
            None => Path::new("."),
            Some(d) if d.as_os_str().is_empty() => Path::new("."),
            Some(d) => d,
        };
        let dir = match fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NONBLOCK)
            .open(parent)
        {
            Err(e) => {
                return Err(format!(
                    "cannot open the directory that holds {}: {}",
                    path, e
                ));
            }
            Ok(d) => d,
        };
        let meta = match dir.metadata() {
            Err(e) => {
                return Err(format!(
                    "cannot read the directory that holds {}: {}",
                    path, e
                ));
            }
            Ok(m) => m,
        };
        return Ok(PinnedEntry {
            entry: Entry {
                dir: (meta.dev(), meta.ino()),
                name: name,
            },
            dir: dir,
        });
    }

    /// Open the locked entry itself, never following a final symbolic link. `O_NONBLOCK` prevents a FIFO from
    /// stalling before its type is checked; the caller supplies only the access mode.
    pub fn open(&self, access: libc::c_int) -> io::Result<fs::File> {
        let name = CString::new(self.entry.name.as_bytes())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "entry name contains NUL"))?;
        let fd = unsafe {
            libc::openat(
                self.dir.as_raw_fd(),
                name.as_ptr(),
                access | libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK,
            )
        };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        return Ok(unsafe { fs::File::from_raw_fd(fd) });
    }
}

fn entry_lock_file_name(entry: &Entry) -> String {
    let mut input: Vec<u8> = b"entry\0".to_vec();
    input.extend_from_slice(&entry.dir.0.to_be_bytes());
    input.extend_from_slice(&entry.dir.1.to_be_bytes());
    input.push(0);
    input.extend_from_slice(entry.name.as_bytes());
    let digest = sha256::hash_bytes(&input);
    let mut lock = String::from("entry-");
    for byte in digest.iter() {
        lock.push_str(&format!("{:02x}", byte));
    }
    lock.push_str(".lock");
    return lock;
}

/// The lock key of an open file, read from its handle. A file that is neither a regular file nor a block device is
/// refused, and `path` names it in the message.
pub fn key_of(file: &fs::File, path: &str) -> Result<LockKey, String> {
    let meta = match file.metadata() {
        Err(e) => return Err(format!("{}: {}", path, e)),
        Ok(m) => m,
    };
    return match lock_key(meta.mode(), meta.rdev()) {
        Err(msg) => Err(format!("{}: {}", path, msg)),
        Ok(key) => Ok(key),
    };
}

/// Take a role's lock on its open file, in `mode`, without waiting. A regular file is locked on its own open file, so
/// every path and hard link to it contends, and nothing is created in the namespace. A block device is locked through
/// its lock file in `ns`, so every device node for it contends. `path` names the role in the messages.
///
/// The result is `None` when the lock is on `file` itself. The caller must then keep `file` open for as long as the lock
/// must hold, since closing every descriptor of the open file releases its lock.
pub fn lock_role(
    file: &fs::File,
    path: &str,
    mode: LockMode,
    ns: &LockDir,
) -> Result<Option<FileLock>, String> {
    return match key_of(file, path)? {
        LockKey::Regular => match try_lock_open(file, mode) {
            Err(e) => Err(e.message(path)),
            Ok(()) => Ok(None),
        },
        LockKey::Block { major, minor } => {
            match ns.take_mode(&block_lock_file_name(major, minor), mode) {
                Err(e) => Err(e.message(path)),
                Ok(lock) => Ok(Some(lock)),
            }
        }
    };
}

/// An exclusive or shared lock, held until this value is dropped.
pub struct FileLock {
    _file: fs::File,
}

/// A lock already taken on `file`, held until this value is dropped. Used where the lock belongs to a file the caller
/// opened itself, rather than to a lock file of the namespace (`filecheckpoint.rs` holds the record's own file this way).
pub fn hold(file: fs::File) -> FileLock {
    return FileLock { _file: file };
}

/// The lock namespace of this host, opened and validated once for a run. Every lock file of the run is opened
/// relative to this handle, so the directory that was validated is the one the locks are taken in, whatever the
/// path has become since.
pub struct LockDir {
    path: PathBuf,
    dir: fs::File,
    identity: (u64, u64),
}

impl LockDir {
    /// Open the lock namespace at `path` and check the open handle. It must be a directory, not a symbolic link,
    /// owned by this user, and accessible to no one else: it has no group or other permission bits. On Linux those
    /// bits also show an extended ACL entry that grants access, because the group bits hold the ACL mask. A named
    /// user or group that can reach the namespace is therefore refused as well.
    pub fn open(path: &Path) -> Result<LockDir, String> {
        let dir = match fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(path)
        {
            Err(e) if e.raw_os_error() == Some(libc::ELOOP) => {
                return Err(format!(
                    "lock directory {} is a symbolic link; name the directory itself",
                    path.display()
                ));
            }
            Err(e) => {
                return Err(format!(
                    "cannot use lock directory {}: {}",
                    path.display(),
                    e
                ));
            }
            Ok(f) => f,
        };
        let meta = match dir.metadata() {
            Err(e) => {
                return Err(format!(
                    "cannot use lock directory {}: {}",
                    path.display(),
                    e
                ));
            }
            Ok(m) => m,
        };
        if !meta.is_dir() {
            return Err(format!(
                "lock directory {} is not a directory",
                path.display()
            ));
        }
        if meta.uid() != unsafe { libc::geteuid() } {
            return Err(format!(
                "lock directory {} is not owned by this user",
                path.display()
            ));
        }
        if meta.mode() & 0o077 != 0 {
            return Err(format!(
                "lock directory {} is accessible to group or others; it must be private to the updater (mode 0700)",
                path.display()
            ));
        }
        return Ok(LockDir {
            path: path.to_path_buf(),
            dir: dir,
            identity: (meta.dev(), meta.ino()),
        });
    }

    /// The device and inode that name this namespace. A checkpoint record kept in the namespace is refused by it.
    pub fn identity(&self) -> (u64, u64) {
        return self.identity;
    }

    /// Take the exclusive lock on the file `name` in this namespace, creating the file if it is absent. `name` must
    /// be one plain file name, so it cannot reach outside the namespace. The file is opened relative to the validated
    /// handle, without following a symbolic link at its name.
    ///
    /// A new file is made private before its name exists (see [`take_with`](LockDir::take_with)). An existing file keeps
    /// its mode, and is refused if it is not private.
    pub fn take(&self, name: &str) -> Result<FileLock, LockError> {
        return self.take_mode(name, LockMode::Exclusive);
    }

    /// As [`take`](LockDir::take), but the shared lock: it excludes an exclusive lock on `name` and not another shared one.
    pub fn take_shared(&self, name: &str) -> Result<FileLock, LockError> {
        return self.take_mode(name, LockMode::Shared);
    }

    /// As [`take`](LockDir::take), in `mode`.
    pub fn take_mode(&self, name: &str, mode: LockMode) -> Result<FileLock, LockError> {
        return self.take_in(name, mode, set_private_mode, &|| {});
    }

    /// Take the lock of the directory entry `entry`, in `mode`, without waiting.
    ///
    /// This lock is about the *name*, not the storage behind it: two runs that name one entry of one directory contend
    /// for it whether the entry is absent, a file, a symbolic link, mid-creation by a rename, or just removed. So a run
    /// that writes an entry and a run whose checkpoint record is that entry cannot both be at work, and neither can
    /// a run that only reads the entry the shared one, as for an identity lock.
    pub fn take_entry(&self, entry: &Entry, mode: LockMode) -> Result<FileLock, LockError> {
        return self.take_mode(&entry_lock_file_name(entry), mode);
    }

    /// The name of the lock file [`take_entry`](LockDir::take_entry) uses for `entry`. A run that already holds an
    /// entry lock hands this name to the next step, which then knows not to take the same lock against itself
    /// (`roles.rs`).
    pub fn entry_lock_name(&self, entry: &Entry) -> String {
        return entry_lock_file_name(entry);
    }

    /// As [`take`](LockDir::take), with the mode of a new file set by `set_mode`. The new file is created with no name
    /// (`O_TMPFILE`), its mode is set through its open handle, and only then is it linked to `name`. No run can open it
    /// before it has its final mode, so an interruption or a failed mode change leaves no name behind. The link fails if
    /// the name is taken, so an existing lock file is never replaced: a run that loses the race opens the winner's file.
    /// `set_mode` is a plain function pointer, so a test can inject the failure a mode change can meet.
    pub fn take_with(&self, name: &str, set_mode: ModeFn) -> Result<FileLock, LockError> {
        return self.take_in(name, LockMode::Exclusive, set_mode, &|| {});
    }

    fn take_in(
        &self,
        name: &str,
        mode: LockMode,
        set_mode: ModeFn,
        before_publish: &dyn Fn(),
    ) -> Result<FileLock, LockError> {
        let shown = self.path.join(name);
        if !is_plain_name(name) {
            return Err(LockError::Refused(format!(
                "invalid lock file name {:?}: it must be one file name, not empty, `.` or `..`, with no separator",
                name
            )));
        }
        let Ok(c_name) = CString::new(name) else {
            return Err(LockError::Refused(format!(
                "invalid lock file name {:?}",
                name
            )));
        };
        let open_failed = |e: io::Error| {
            let hint = if e.kind() == io::ErrorKind::PermissionDenied {
                "; if an interrupted earlier release left the file without owner access, restore it (docs/locking-and-recovery.md §1.4)"
            } else {
                ""
            };
            LockError::Refused(format!(
                "cannot open lock file {}: {}{}",
                shown.display(),
                e,
                hint
            ))
        };
        let file = match self.open_entry(&c_name, libc::O_RDWR) {
            Ok(file) => file,
            Err(e) if e.raw_os_error() == Some(libc::ENOENT) => {
                before_publish();
                match self.publish(&c_name, set_mode, &shown)? {
                    Some(file) => file,
                    None => self
                        .open_entry(&c_name, libc::O_RDWR)
                        .map_err(open_failed)?,
                }
            }
            Err(e) => return Err(open_failed(e)),
        };
        check_lock_file(&file, &shown)?;
        try_lock_open(&file, mode)?;
        return Ok(FileLock { _file: file });
    }

    fn publish(
        &self,
        name: &CString,
        set_mode: ModeFn,
        shown: &Path,
    ) -> Result<Option<fs::File>, LockError> {
        let file = match open_unnamed(&self.dir) {
            Err(e)
                if matches!(
                    e.raw_os_error(),
                    Some(libc::EOPNOTSUPP) | Some(libc::EISDIR)
                ) =>
            {
                return Err(LockError::Refused(format!(
                    "cannot create lock file {}: the lock namespace's filesystem does not support unnamed files \
                     (O_TMPFILE), which a lock file needs (docs/platform.md §2.1)",
                    shown.display()
                )));
            }
            Err(e) => {
                return Err(LockError::Refused(format!(
                    "cannot create lock file {}: {}",
                    shown.display(),
                    e
                )));
            }
            Ok(file) => file,
        };
        if let Err(e) = set_mode(&file) {
            return Err(LockError::Refused(format!(
                "cannot set the mode of new lock file {}: {}",
                shown.display(),
                e
            )));
        }
        let through = CString::new(format!("/proc/self/fd/{}", file.as_raw_fd()))
            .expect("a descriptor path has no NUL byte");
        let rc = unsafe {
            libc::linkat(
                libc::AT_FDCWD,
                through.as_ptr(),
                self.dir.as_raw_fd(),
                name.as_ptr(),
                libc::AT_SYMLINK_FOLLOW,
            )
        };
        if rc == 0 {
            return Ok(Some(file));
        }
        let e = io::Error::last_os_error();
        return match e.raw_os_error() {
            Some(libc::EEXIST) => Ok(None),
            _ => Err(LockError::Refused(format!(
                "cannot publish lock file {}: {}",
                shown.display(),
                e
            ))),
        };
    }

    fn open_entry(&self, name: &CString, flags: libc::c_int) -> io::Result<fs::File> {
        let fd = unsafe {
            libc::openat(
                self.dir.as_raw_fd(),
                name.as_ptr(),
                flags | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC,
                0o600 as libc::mode_t,
            )
        };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        return Ok(unsafe { fs::File::from_raw_fd(fd) });
    }
}

fn is_plain_name(name: &str) -> bool {
    return !name.is_empty()
        && name != "."
        && name != ".."
        && !name.contains('/')
        && !name.contains('\0');
}

/// Sets the mode of a new lock file through its open handle. A plain function pointer, so a test can inject the failure.
pub type ModeFn = fn(&fs::File) -> io::Result<()>;

fn set_private_mode(file: &fs::File) -> io::Result<()> {
    return file.set_permissions(fs::Permissions::from_mode(0o600));
}

fn open_unnamed(dir: &fs::File) -> io::Result<fs::File> {
    let fd = unsafe {
        libc::openat(
            dir.as_raw_fd(),
            c".".as_ptr(),
            libc::O_TMPFILE | libc::O_RDWR | libc::O_CLOEXEC,
            0o600 as libc::mode_t,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    return Ok(unsafe { fs::File::from_raw_fd(fd) });
}

fn check_lock_file(file: &fs::File, shown: &Path) -> Result<(), LockError> {
    let meta = match file.metadata() {
        Err(e) => {
            return Err(LockError::Refused(format!(
                "cannot read lock file {}: {}",
                shown.display(),
                e
            )));
        }
        Ok(m) => m,
    };
    if !meta.is_file() {
        return Err(LockError::Refused(format!(
            "lock file {} is not a regular file",
            shown.display()
        )));
    }
    if meta.nlink() != 1 {
        return Err(LockError::Refused(format!(
            "lock file {} has more than one name",
            shown.display()
        )));
    }
    if meta.uid() != unsafe { libc::geteuid() } {
        return Err(LockError::Refused(format!(
            "lock file {} is not owned by this user",
            shown.display()
        )));
    }
    if meta.mode() & 0o077 != 0 {
        return Err(LockError::Refused(format!(
            "lock file {} is accessible to group or others (mode {:o}); remove that access. The updater does not change it",
            shown.display(),
            meta.mode() & 0o777
        )));
    }
    return Ok(());
}

/// Take a lock on an open file, in `mode`, without waiting. The lock belongs to the open file, so every path or hard
/// link that opens the same file contends for it. Each `open` makes its own open file, so two opens of one file contend
/// even within one process.
pub fn try_lock_open(file: &fs::File, mode: LockMode) -> Result<(), LockError> {
    let attempt = match mode {
        LockMode::Exclusive => file.try_lock(),
        LockMode::Shared => file.try_lock_shared(),
    };
    return match attempt {
        Err(fs::TryLockError::WouldBlock) => Err(LockError::Held),
        Err(fs::TryLockError::Error(e)) => Err(LockError::Refused(e.to_string())),
        Ok(()) => Ok(()),
    };
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::PermissionsExt;
    use std::os::unix::fs::symlink;
    use std::sync::Barrier;
    use std::sync::atomic::AtomicUsize;
    use std::sync::atomic::Ordering;

    fn scratch(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("ota-filelock-{}-{}", std::process::id(), name));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).unwrap();
        return dir;
    }

    fn open(dir: &Path) -> LockDir {
        return LockDir::open(dir).unwrap();
    }

    const XATTR_ACCESS_ACL: &[u8] = b"system.posix_acl_access\0";
    const XATTR_DEFAULT_ACL: &[u8] = b"system.posix_acl_default\0";
    const ACL_UNDEFINED_ID: u32 = u32::MAX;
    const ACL_USER_OBJ: u16 = 0x01;
    const ACL_GROUP_OBJ: u16 = 0x04;
    const ACL_GROUP: u16 = 0x08;
    const ACL_MASK: u16 = 0x10;
    const ACL_OTHER: u16 = 0x20;

    fn acl_blob(entries: &[(u16, u16, u32)]) -> Vec<u8> {
        let mut blob = 2u32.to_le_bytes().to_vec();
        for (tag, perm, id) in entries {
            blob.extend_from_slice(&tag.to_le_bytes());
            blob.extend_from_slice(&perm.to_le_bytes());
            blob.extend_from_slice(&id.to_le_bytes());
        }
        return blob;
    }

    fn set_acl(path: &Path, xattr: &[u8], entries: &[(u16, u16, u32)]) -> io::Result<()> {
        let c_path = CString::new(path.as_os_str().as_bytes()).unwrap();
        let blob = acl_blob(entries);
        let rc = unsafe {
            libc::setxattr(
                c_path.as_ptr(),
                xattr.as_ptr() as *const libc::c_char,
                blob.as_ptr() as *const libc::c_void,
                blob.len(),
                0,
            )
        };
        if rc != 0 {
            return Err(io::Error::last_os_error());
        }
        return Ok(());
    }

    fn acl_unsupported(e: &io::Error) -> bool {
        return e.raw_os_error() == Some(libc::ENOTSUP)
            || e.raw_os_error() == Some(libc::EOPNOTSUPP);
    }

    #[test]
    fn a_new_lock_file_is_created_private_and_a_second_attempt_is_held() {
        let dir = scratch("held");
        let ns = open(&dir);
        let first = ns.take("x.lock").unwrap();
        assert_eq!(
            fs::metadata(dir.join("x.lock"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        assert!(matches!(ns.take("x.lock"), Err(LockError::Held)));
        drop(first);
        assert!(ns.take("x.lock").is_ok());
        fs::remove_dir_all(&dir).unwrap();
    }

    fn refuse_mode(_file: &fs::File) -> io::Result<()> {
        return Err(io::Error::other("injected mode failure"));
    }

    #[test]
    fn an_injected_mode_failure_is_reached_and_publishes_no_name_and_a_later_take_succeeds() {
        static FAILURES: AtomicUsize = AtomicUsize::new(0);
        fn counted_refusal(file: &fs::File) -> io::Result<()> {
            FAILURES.fetch_add(1, Ordering::SeqCst);
            return refuse_mode(file);
        }
        let dir = scratch("mode-fails");
        let ns = open(&dir);
        let err = ns.take_with("x.lock", counted_refusal).err().unwrap();
        assert!(
            matches!(&err, LockError::Refused(m) if m.contains("cannot set the mode")),
            "{:?}",
            err
        );
        assert_eq!(FAILURES.load(Ordering::SeqCst), 1);
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 0);
        let held = ns.take("x.lock").unwrap();
        assert_eq!(
            fs::metadata(dir.join("x.lock"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        drop(held);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_publication_seam_runs_only_when_the_name_is_absent() {
        let dir = scratch("seam-absent");
        let ns = open(&dir);
        let calls = AtomicUsize::new(0);
        let count = || {
            calls.fetch_add(1, Ordering::SeqCst);
        };
        drop(
            ns.take_in("x.lock", LockMode::Exclusive, set_private_mode, &count)
                .unwrap(),
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        drop(
            ns.take_in("x.lock", LockMode::Exclusive, set_private_mode, &count)
                .unwrap(),
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn contenders_that_all_see_a_new_name_absent_publish_one_file_and_the_others_are_held_by_it() {
        let dir = scratch("race-publish");
        let ns = open(&dir);
        const CONTENDERS: usize = 3;
        let barrier = Barrier::new(CONTENDERS);
        let arrivals = AtomicUsize::new(0);
        let outcomes: Vec<Result<FileLock, LockError>> = std::thread::scope(|s| {
            let handles: Vec<_> = (0..CONTENDERS)
                .map(|_| {
                    let (ns, barrier, arrivals) = (&ns, &barrier, &arrivals);
                    s.spawn(move || {
                        ns.take_in("x.lock", LockMode::Exclusive, set_private_mode, &|| {
                            arrivals.fetch_add(1, Ordering::SeqCst);
                            barrier.wait();
                        })
                    })
                })
                .collect();
            handles.into_iter().map(|h| h.join().unwrap()).collect()
        });
        assert_eq!(arrivals.load(Ordering::SeqCst), CONTENDERS);
        assert_eq!(outcomes.iter().filter(|o| o.is_ok()).count(), 1);
        assert!(
            outcomes
                .iter()
                .filter_map(|o| o.as_ref().err())
                .all(|e| matches!(e, LockError::Held))
        );
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 1);
        assert_eq!(
            fs::metadata(dir.join("x.lock"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        drop(outcomes);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_contender_whose_mode_change_fails_at_publication_publishes_no_name_while_its_rival_publishes_one()
     {
        let dir = scratch("race-mode");
        let ns = open(&dir);
        let barrier = Barrier::new(2);
        let outcomes: Vec<Result<FileLock, LockError>> = std::thread::scope(|s| {
            let handles: Vec<_> = (0..2)
                .map(|i| {
                    let (ns, barrier) = (&ns, &barrier);
                    s.spawn(move || {
                        let mode: ModeFn = if i == 0 {
                            refuse_mode
                        } else {
                            set_private_mode
                        };
                        ns.take_in("x.lock", LockMode::Exclusive, mode, &|| {
                            barrier.wait();
                        })
                    })
                })
                .collect();
            handles.into_iter().map(|h| h.join().unwrap()).collect()
        });
        assert!(
            matches!(&outcomes[0], Err(LockError::Refused(m)) if m.contains("cannot set the mode"))
        );
        assert!(outcomes[1].is_ok());
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 1);
        assert_eq!(
            fs::metadata(dir.join("x.lock"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        drop(outcomes);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_shared_lock_admits_another_reader_and_refuses_a_writer_of_the_same_file() {
        let dir = scratch("shared-regular");
        let path = dir.join("image.img");
        fs::write(&path, b"x").unwrap();
        let first = fs::File::open(&path).unwrap();
        let second = fs::File::open(&path).unwrap();
        let writer = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        assert!(try_lock_open(&first, LockMode::Shared).is_ok());
        assert!(try_lock_open(&second, LockMode::Shared).is_ok());
        assert!(matches!(
            try_lock_open(&writer, LockMode::Exclusive),
            Err(LockError::Held)
        ));
        drop(first);
        drop(second);
        assert!(try_lock_open(&writer, LockMode::Exclusive).is_ok());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_writer_is_refused_through_any_hard_link_while_a_reader_of_another_link_holds_the_file() {
        let dir = scratch("roles-hard-link");
        let ns = open(&dir);
        let path = dir.join("image.img");
        fs::write(&path, b"x").unwrap();
        let link = dir.join("image-link.img");
        fs::hard_link(&path, &link).unwrap();
        let reader = fs::File::open(&link).unwrap();
        let writer = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        assert!(
            lock_role(&reader, "old image", LockMode::Shared, &ns)
                .unwrap()
                .is_none()
        );
        let refused = lock_role(&writer, "device", LockMode::Exclusive, &ns)
            .err()
            .unwrap();
        assert!(refused.contains("another update holds"), "{}", refused);
        drop(reader);
        assert!(
            lock_role(&writer, "device", LockMode::Exclusive, &ns)
                .unwrap()
                .is_none()
        );
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_shared_block_lock_file_admits_readers_and_refuses_a_writer_of_the_same_device_number() {
        let dir = scratch("shared-block-name");
        let ns = open(&dir);
        let name = block_lock_file_name(8, 1);
        let first = ns.take_shared(&name).unwrap();
        let second = ns.take_shared(&name).unwrap();
        assert!(matches!(ns.take(&name), Err(LockError::Held)));
        drop(first);
        assert!(matches!(ns.take(&name), Err(LockError::Held)));
        drop(second);
        assert!(ns.take(&name).is_ok());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_lock_follows_the_device_number_and_other_file_types_are_refused() {
        let rdev = libc::makedev(8, 1);
        assert_eq!(lock_key(libc::S_IFREG | 0o600, 0), Ok(LockKey::Regular));
        assert_eq!(
            lock_key(libc::S_IFBLK | 0o660, rdev),
            Ok(LockKey::Block { major: 8, minor: 1 })
        );
        assert_eq!(
            lock_key(libc::S_IFBLK | 0o600, rdev),
            Ok(LockKey::Block { major: 8, minor: 1 })
        );
        assert_ne!(
            lock_key(libc::S_IFBLK, libc::makedev(8, 2)),
            lock_key(libc::S_IFBLK, rdev)
        );
        assert_eq!(block_lock_file_name(8, 1), "block-8-1.lock");
        for mode in [libc::S_IFCHR, libc::S_IFIFO, libc::S_IFDIR, libc::S_IFLNK] {
            assert!(
                lock_key(mode | 0o600, 0).is_err(),
                "mode {:o} has an update lock",
                mode
            );
        }
    }

    #[test]
    fn a_lock_file_an_interrupted_release_left_without_owner_access_is_refused_and_is_not_changed()
    {
        if unsafe { libc::geteuid() } == 0 {
            eprintln!("skipped: root is not restricted by the owner's mode bits");
            return;
        }
        let dir = scratch("legacy-mode");
        let path = dir.join("x.lock");
        fs::write(&path, b"").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o400)).unwrap();
        let err = open(&dir).take("x.lock").err().unwrap();
        let named_in_refusal = matches!(&err, LockError::Refused(m)
            if m.contains("cannot open lock file") && m.contains("docs/locking-and-recovery.md"));
        assert!(named_in_refusal, "{:?}", err);
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o400
        );
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        assert!(open(&dir).take("x.lock").is_ok());
        fs::remove_dir_all(&dir).unwrap();
    }

    fn namespace_in(root: &Path) -> (PathBuf, LockDir) {
        let ns_path = root.join("ns");
        fs::create_dir(&ns_path).unwrap();
        fs::set_permissions(&ns_path, fs::Permissions::from_mode(0o700)).unwrap();
        let ns = open(&ns_path);
        return (ns_path, ns);
    }

    #[test]
    fn a_lock_name_must_be_one_plain_file_name_and_every_other_form_is_refused_before_any_open() {
        let root = scratch("names");
        let (ns_path, ns) = namespace_in(&root);
        let absolute = root.join("absolute.lock");
        let forms = [
            String::new(),
            ".".to_string(),
            "..".to_string(),
            "x/y.lock".to_string(),
            "/x.lock".to_string(),
            absolute.to_str().unwrap().to_string(),
            "../escape.lock".to_string(),
            "x/../../escape.lock".to_string(),
            "./x.lock".to_string(),
            "x.lock/".to_string(),
            "x\0.lock".to_string(),
        ];
        for name in &forms {
            let err = ns.take(name).err().unwrap();
            assert!(
                matches!(err, LockError::Refused(_)),
                "{:?} was accepted: {:?}",
                name,
                err
            );
        }
        let beside: Vec<_> = fs::read_dir(&root)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(beside.len(), 1, "{:?}", beside);
        assert_eq!(fs::read_dir(&ns_path).unwrap().count(), 0);
        assert!(!absolute.exists());
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_rejected_name_neither_opens_nor_changes_a_file_outside_the_namespace() {
        let root = scratch("victim");
        let (_ns_path, ns) = namespace_in(&root);
        let victim = root.join("victim");
        fs::write(&victim, b"keep").unwrap();
        fs::set_permissions(&victim, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(matches!(ns.take("../victim"), Err(LockError::Refused(_))));
        assert_eq!(fs::read(&victim).unwrap(), b"keep");
        assert_eq!(
            fs::metadata(&victim).unwrap().permissions().mode() & 0o777,
            0o644
        );
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_symbolic_link_at_the_lock_name_is_refused_and_its_target_is_untouched() {
        let dir = scratch("symlink");
        let victim = dir.join("victim");
        fs::write(&victim, b"keep").unwrap();
        symlink(&victim, dir.join("x.lock")).unwrap();
        assert!(matches!(
            open(&dir).take("x.lock"),
            Err(LockError::Refused(_))
        ));
        assert_eq!(fs::read(&victim).unwrap(), b"keep");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_dangling_symbolic_link_at_the_lock_name_is_refused_and_creates_nothing() {
        let dir = scratch("dangling");
        symlink(dir.join("missing"), dir.join("x.lock")).unwrap();
        assert!(matches!(
            open(&dir).take("x.lock"),
            Err(LockError::Refused(_))
        ));
        assert!(!dir.join("missing").exists());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_named_pipe_at_the_lock_name_is_refused_without_waiting() {
        let dir = scratch("fifo");
        let path = dir.join("x.lock");
        let name = CString::new(path.to_str().unwrap()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
        assert!(matches!(
            open(&dir).take("x.lock"),
            Err(LockError::Refused(_))
        ));
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_lock_file_with_a_second_name_is_refused() {
        let dir = scratch("linked");
        let path = dir.join("x.lock");
        fs::write(&path, b"").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        fs::hard_link(&path, dir.join("other.lock")).unwrap();
        assert!(matches!(
            open(&dir).take("x.lock"),
            Err(LockError::Refused(_))
        ));
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_pre_existing_lock_file_accessible_to_others_is_refused_and_its_mode_is_left_alone() {
        let dir = scratch("permissive");
        let path = dir.join("x.lock");
        fs::write(&path, b"").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o640)).unwrap();
        let err = open(&dir).take("x.lock").err().unwrap();
        assert!(
            matches!(&err, LockError::Refused(m) if m.contains("accessible to group or others")),
            "{:?}",
            err
        );
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o640
        );
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        assert!(open(&dir).take("x.lock").is_ok());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_lock_file_that_an_access_acl_grants_a_named_group_is_refused() {
        let dir = scratch("acl-access");
        let path = dir.join("x.lock");
        fs::write(&path, b"").unwrap();
        let gid = unsafe { libc::getegid() };
        let entries = [
            (ACL_USER_OBJ, 6, ACL_UNDEFINED_ID),
            (ACL_GROUP_OBJ, 0, ACL_UNDEFINED_ID),
            (ACL_GROUP, 6, gid),
            (ACL_MASK, 6, ACL_UNDEFINED_ID),
            (ACL_OTHER, 0, ACL_UNDEFINED_ID),
        ];
        match set_acl(&path, XATTR_ACCESS_ACL, &entries) {
            Err(e) if acl_unsupported(&e) => {
                eprintln!("skipped: the filesystem has no ACLs: {}", e);
                fs::remove_dir_all(&dir).unwrap();
                return;
            }
            Err(e) => panic!("cannot set the access ACL on the lock file: {}", e),
            Ok(()) => {}
        }
        assert_ne!(fs::metadata(&path).unwrap().permissions().mode() & 0o070, 0);
        assert!(matches!(
            open(&dir).take("x.lock"),
            Err(LockError::Refused(_))
        ));
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_new_lock_file_in_a_namespace_with_a_default_acl_grants_the_named_group_nothing() {
        let dir = scratch("acl-default");
        let gid = unsafe { libc::getegid() };
        let entries = [
            (ACL_USER_OBJ, 7, ACL_UNDEFINED_ID),
            (ACL_GROUP_OBJ, 7, ACL_UNDEFINED_ID),
            (ACL_GROUP, 6, gid),
            (ACL_MASK, 7, ACL_UNDEFINED_ID),
            (ACL_OTHER, 0, ACL_UNDEFINED_ID),
        ];
        match set_acl(&dir, XATTR_DEFAULT_ACL, &entries) {
            Err(e) if acl_unsupported(&e) => {
                eprintln!("skipped: the filesystem has no ACLs: {}", e);
                fs::remove_dir_all(&dir).unwrap();
                return;
            }
            Err(e) => panic!("cannot set the default ACL on the namespace: {}", e),
            Ok(()) => {}
        }
        let ns = open(&dir);
        assert!(ns.take("x.lock").is_ok());
        assert_eq!(
            fs::metadata(dir.join("x.lock"))
                .unwrap()
                .permissions()
                .mode()
                & 0o077,
            0
        );
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_lock_directory_is_usable_only_when_no_one_but_its_owner_can_reach_it() {
        let dir = scratch("dir-mode");
        assert!(LockDir::open(&dir).is_ok());
        for mode in [0o755, 0o770, 0o701] {
            fs::set_permissions(&dir, fs::Permissions::from_mode(mode)).unwrap();
            let err = LockDir::open(&dir).err().unwrap();
            assert!(
                err.contains("accessible to group or others"),
                "mode {:o}: {}",
                mode,
                err
            );
        }
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).unwrap();
        assert!(LockDir::open(&dir).is_ok());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_lock_directory_that_is_missing_a_file_or_a_symbolic_link_is_refused() {
        let dir = scratch("dir-bad");
        let file = dir.join("plain");
        fs::write(&file, b"").unwrap();
        assert!(
            LockDir::open(&file)
                .err()
                .unwrap()
                .contains("is not a directory")
        );
        assert!(
            LockDir::open(&dir.join("missing"))
                .err()
                .unwrap()
                .contains("cannot use lock directory")
        );
        let link = dir.join("link");
        symlink(&dir, &link).unwrap();
        assert!(
            LockDir::open(&link)
                .err()
                .unwrap()
                .contains("is a symbolic link")
        );
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn locks_are_taken_in_the_directory_that_was_validated_even_after_its_path_is_replaced() {
        let root = scratch("swap");
        let ns_path = root.join("ns");
        fs::create_dir(&ns_path).unwrap();
        fs::set_permissions(&ns_path, fs::Permissions::from_mode(0o700)).unwrap();
        let ns = LockDir::open(&ns_path).unwrap();
        let moved = root.join("moved");
        fs::rename(&ns_path, &moved).unwrap();
        fs::create_dir(&ns_path).unwrap();
        let _held = ns.take("x.lock").unwrap();
        assert!(moved.join("x.lock").exists());
        assert!(!ns_path.join("x.lock").exists());
        let other = root.join("other");
        fs::create_dir(&other).unwrap();
        fs::remove_dir(&ns_path).unwrap();
        symlink(&other, &ns_path).unwrap();
        let _second = ns.take("y.lock").unwrap();
        assert!(moved.join("y.lock").exists());
        assert!(!other.join("y.lock").exists());
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn one_entry_names_one_lock_whether_it_is_absent_being_created_renamed_over_or_removed() {
        let dir = scratch("entry-same-lock");
        let ns = open(&scratch("entry-same-lock-ns"));
        let target = dir.join("ck").to_str().unwrap().to_string();
        let entry = Entry::of(&target).unwrap();
        let held = ns.take_entry(&entry, LockMode::Exclusive).unwrap();
        assert!(
            matches!(
                ns.take_entry(&entry, LockMode::Shared),
                Err(LockError::Held)
            ),
            "the entry's lock is held, so a reader of the same entry is refused too"
        );
        fs::write(&target, b"first").unwrap();
        assert_eq!(
            ns.entry_lock_name(&Entry::of(&target).unwrap()),
            ns.entry_lock_name(&entry)
        );
        assert!(matches!(
            ns.take_entry(&entry, LockMode::Exclusive),
            Err(LockError::Held)
        ));
        let replacement = dir.join("replacement");
        fs::write(&replacement, b"second").unwrap();
        fs::rename(&replacement, &target).unwrap();
        assert!(matches!(
            ns.take_entry(&entry, LockMode::Exclusive),
            Err(LockError::Held)
        ));
        fs::remove_file(&target).unwrap();
        assert!(matches!(
            ns.take_entry(&entry, LockMode::Exclusive),
            Err(LockError::Held)
        ));
        drop(held);
        let next = ns.take_entry(&entry, LockMode::Exclusive).unwrap();
        fs::write(&target, b"third").unwrap();
        assert_eq!(fs::read(&target).unwrap(), b"third");
        drop(next);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn two_spellings_of_one_directory_reach_one_entry_lock_and_another_directory_has_its_own() {
        let dir = scratch("entry-alias");
        let ns = open(&scratch("entry-alias-ns"));
        let other = scratch("entry-alias-other");
        symlink(&dir, &other.join("link")).unwrap();
        let direct = dir.join("ck").to_str().unwrap().to_string();
        let through_link = other.join("link").join("ck").to_str().unwrap().to_string();
        let _held = ns
            .take_entry(&Entry::of(&direct).unwrap(), LockMode::Exclusive)
            .unwrap();
        let refused = ns.take_entry(&Entry::of(&through_link).unwrap(), LockMode::Exclusive);
        assert!(
            matches!(refused, Err(LockError::Held)),
            "the retargeted spelling names one entry"
        );
        assert!(
            ns.take_entry(
                &Entry::of(other.join("ck").to_str().unwrap()).unwrap(),
                LockMode::Exclusive
            )
            .is_ok()
        );
        fs::remove_dir_all(&other).unwrap();
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_second_name_for_one_file_is_a_second_entry_and_only_the_lock_on_the_file_reaches_it() {
        let dir = scratch("entry-hard-link");
        let ns = open(&scratch("entry-hard-link-ns"));
        let first = dir.join("first");
        let second = dir.join("second");
        fs::write(&first, b"one file, two names").unwrap();
        fs::hard_link(&first, &second).unwrap();
        let first_name = first.to_str().unwrap().to_string();
        let second_name = second.to_str().unwrap().to_string();
        let one = ns
            .take_entry(&Entry::of(&first_name).unwrap(), LockMode::Exclusive)
            .unwrap();
        assert!(
            ns.take_entry(&Entry::of(&second_name).unwrap(), LockMode::Exclusive)
                .is_ok(),
            "the second entry's lock is a different name"
        );
        let held = fs::File::open(&first).unwrap();
        assert!(
            lock_role(&held, &first_name, LockMode::Exclusive, &ns)
                .unwrap()
                .is_none()
        );
        let by_the_other_name = lock_role(
            &fs::File::open(&second).unwrap(),
            &second_name,
            LockMode::Exclusive,
            &ns,
        )
        .err()
        .unwrap();
        assert!(
            by_the_other_name.contains("another update holds"),
            "{}",
            by_the_other_name
        );
        drop(one);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn an_entry_whose_directory_cannot_be_opened_or_whose_name_is_not_one_name_is_refused() {
        let dir = scratch("entry-refused");
        assert!(
            Entry::of(dir.join("gone").join("ck").to_str().unwrap())
                .err()
                .unwrap()
                .contains("cannot open the directory"),
            "missing directory"
        );
        assert!(
            Entry::of(dir.join("..").to_str().unwrap())
                .err()
                .unwrap()
                .contains("names no entry"),
            "dot-dot"
        );
        fs::remove_dir_all(&dir).unwrap();
    }
}
