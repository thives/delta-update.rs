//! A checkpoint record kept in a host file. The record is replaced by writing a
//! fresh temporary file in a private internal directory, syncing it, renaming it over the record, and
//! syncing both directories, so an interruption leaves either the old record or the
//! new one.
//!
//! The file is trusted execution state: the host must keep it writable only by the
//! updater. `docs/checkpoint.md` states that requirement. The temporary file is
//! also created defensively: `O_EXCL` never follows a file or symlink already at its
//! name, and the file is private (`0600`). Its name is a function of the record it holds, so
//! the file that produced the current record is found by name when a crash leaves it as a
//! second link. Bounded cleanup of staging debris runs only inside the validated internal
//! directory under the directory lock. Neither property makes a record directory that others
//! can write to safe.
//!
//! A run opens the record's directory once, in [`FileCheckpoint::open`], and keeps that
//! handle for the life of the store. Every operation on the record is relative to the handle:
//! the open, the temporary file, the rename, the unlink, the existence check and the sync. So a
//! path replaced during the run cannot redirect any of them, and the lock the store holds is the
//!
//! The record's own name is checked when it is read, through the open file. A symbolic
//! link is refused, since a replacement renames over the name and would leave the link's
//! target stale. So is a file with a second hard link, for the same reason. Both checks
//!
//! The record's lock lives in the host's lock namespace, not beside the record, and there is one lock per
//! directory. Its name derives from the directory's identity alone, so every record in the directory, under any
//! spelling and through any path, contends for the same lock. Two spellings of one entry therefore never operate at
//! once, whatever the filesystem's naming rules are. The record's name must be printable ASCII, and it must not begin
//! with the reserved prefix, which the store's own staging files and case probes use. The directory must not fold the
//! policy.
//!
//! Above the directory lock, the store also takes the lock of the *entry* its record is in (`filelock.rs::Entry`,
//! name. That lock covers the entry across its absence, creation, rename and removal, which an identity lock cannot,
//! since an absent entry has no inode to lock. It is what makes another run's device, old image or streamed payload and
//! this record refuse each other whichever of them exists first. While the entry holds a regular file, the store also
//! holds that file's own inode, exclusively: this is the second half of the pair, and it catches a role that reaches
//!
//! The reserved prefix prevents records and cooperating roles from colliding with internal names; it does not prove
//! ownership of arbitrary files. Staging lives in a private, owner-validated directory with an ownership marker. Only
//! matching, single-link regular files inside it are eligible for cleanup. Legacy sibling files are never swept.
//! The parent directory and the updater's user must be trusted against non-cooperating changes. The canonical staging
//! directory is published only after its marker is complete and synced, so an interrupted initialization leaves a
//! `.ota-checkpoint.staging.init` directory, which the next open classifies: a directory of the expected shape is
//! completed or rebuilt, and anything else is refused. A refused directory keeps its mode, unless its owner could not read
//! or search it, and the owner's permissions had to be restored to list its entries or check its marker. Legacy sibling
//! debris is never removed automatically.
//!
//! A replacement is durable only after both directories it changes are synced, and a resume confirms that again before it
//! trusts the record (`CheckpointStore::confirm_stored`):
//!
//! ```text
//! private staging directory                 record directory
//! create -> lock -> write -> fsync  --rename--> record -> fsync record directory
//!               |                                  |          -> fsync staging directory
//!               +---------- same inode lock -------+
//! resume: load -> confirm_stored (fsync both directories) -> trust the record
//! open: stale owned staging files -> bounded best-effort sweep
//!       second link to the record -> at most TEMPORARY_ATTEMPTS exact look-ups, removed
//!       fsync the staging directory on every open -> fail closed
//! replacement, every name taken -> exact look-up of each name, stale single-link files removed -> try once more
//! ```
//!
//! Each sweep inspects at most `SWEEP_LIMIT` entries via `/proc/self/fd`; it can leave debris when enumeration fails,
//! or when non-removable entries occupy the scan window. The record's own second link is not left to the sweep: its
//! name is derived from the record's bytes, so it is found by name whatever else the directory holds. A replacement's
//! stale files are found the same way, at its own names. Neither cleanup ever scans the shared record directory.

use std::ffi::CStr;
use std::ffi::CString;
use std::fs;
use std::io;
use std::io::Read;
use std::io::Write;
use std::os::unix::fs::MetadataExt;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::io::AsRawFd;
use std::os::unix::io::FromRawFd;
use std::path::Path;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use crate::filelock::Entry;
use crate::filelock::FileLock;
use crate::filelock::LockDir;
use crate::filelock::LockMode;
use crate::filelock::hold;
use crate::filelock::try_lock_open;
use crate::identity::Identity;
use delta_update_client::checkpoint::CheckpointStore;
use delta_update_client::checkpoint::RECORD_SIZE;
use delta_update_client::sink::io_error;
use delta_update_payload::error::Code;
use delta_update_payload::error::PayloadError;
use delta_update_payload::error::err;
use delta_update_payload::sha256;

const TEMPORARY_ATTEMPTS: u32 = 16;
const SWEEP_LIMIT: usize = 256;
const STAGING_DIR: &str = ".ota-checkpoint.staging";
const STAGING_INIT: &str = ".ota-checkpoint.staging.init";
const STAGING_MARKER: &str = "owner";
const MARKER_CONTENT: &[u8] = b"ota-checkpoint staging directory v1\n";
const INIT_ENTRIES: usize = 1;

/// The prefix of the store's internal directory and case probes. A record
/// may not begin with it, compared without regard to ASCII case, so no record can name one of these entries under any
/// spelling the directory accepts. A rename or a removal of a store's own entry then never touches a record. A role
/// of a run may not begin with it either, which is what makes the separation an enforced fact rather than a naming
/// convention (`roles.rs`).
pub const RESERVED_PREFIX: &str = ".ota-checkpoint.";

/// The directory sync that makes a rename or a removal durable. It takes the open directory the record is in,
/// so the sync is of that directory and not of a path. A plain function pointer, so a test can inject a
/// failure that the filesystem would not produce, as `FileDevice` does.
pub type DirSyncFn = fn(&fs::File) -> io::Result<()>;

type CaseProbeFn = fn(&fs::File) -> Result<(), String>;

type StatFn = fn(&fs::File, &CStr) -> io::Result<Option<libc::stat>>;

type ListFn = fn(&fs::File) -> io::Result<Vec<std::ffi::OsString>>;

type UnlinkFn = fn(&fs::File, &CStr) -> io::Result<()>;

fn sync_dir(dir: &fs::File) -> io::Result<()> {
    return dir.sync_all();
}

/// Whether `name` begins with the reserved prefix. The comparison ignores ASCII case, so a directory that folds case
/// cannot make a record name one of the store's entries under another spelling. A role of a run is checked the same way
/// (`roles.rs::role_entry`), so a device, an old image or a streamed payload can never be one of the store's entries
/// either.
pub fn is_reserved(name: &str) -> bool {
    return name
        .as_bytes()
        .get(..RESERVED_PREFIX.len())
        .is_some_and(|head| head.eq_ignore_ascii_case(RESERVED_PREFIX.as_bytes()));
}

const STAGING_PREFIX: &str = ".ota-checkpoint.stage.";

fn staging_name(record: &[u8; RECORD_SIZE], n: u64) -> String {
    let digest = sha256::hash_bytes(record);
    let mut name = String::from(STAGING_PREFIX);
    for byte in digest[..8].iter() {
        name.push_str(&format!("{:02x}", byte));
    }
    name.push_str(&format!(".{}", n));
    return name;
}

fn is_staging_name(name: &str) -> bool {
    let Some(rest) = name.strip_prefix(STAGING_PREFIX) else {
        return false;
    };
    let is_decimal = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    return match rest.split('.').collect::<Vec<&str>>()[..] {
        [digest, n] => {
            digest.len() == 16 && digest.bytes().all(|b| b.is_ascii_hexdigit()) && is_decimal(n)
        }
        _ => false,
    };
}

fn effective_uid() -> u32 {
    return unsafe { libc::geteuid() };
}

fn probe_name(pid: u32, stamp: u128, n: u64) -> String {
    return format!("{}probe.{}.{}.{}", RESERVED_PREFIX, pid, stamp, n);
}

fn checkpoint_lock_name(dir: (u64, u64)) -> String {
    let mut input: Vec<u8> = b"checkpoint\0".to_vec();
    input.extend_from_slice(&dir.0.to_be_bytes());
    input.extend_from_slice(&dir.1.to_be_bytes());
    let digest = sha256::hash_bytes(&input);
    let mut lock = String::from("checkpoint-");
    for byte in digest.iter() {
        lock.push_str(&format!("{:02x}", byte));
    }
    lock.push_str(".lock");
    return lock;
}

fn c_string(name: &str) -> io::Result<CString> {
    return CString::new(name).map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e));
}

fn open_at(
    dir: &fs::File,
    name: &CStr,
    flags: libc::c_int,
    mode: libc::mode_t,
) -> io::Result<fs::File> {
    let fd = unsafe {
        libc::openat(
            dir.as_raw_fd(),
            name.as_ptr(),
            flags | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            mode,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    return Ok(unsafe { fs::File::from_raw_fd(fd) });
}

fn rename_at(from_dir: &fs::File, from: &CStr, to_dir: &fs::File, to: &CStr) -> io::Result<()> {
    let rc = unsafe {
        libc::renameat(
            from_dir.as_raw_fd(),
            from.as_ptr(),
            to_dir.as_raw_fd(),
            to.as_ptr(),
        )
    };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    return Ok(());
}

fn unlink_at(dir: &fs::File, name: &CStr) -> io::Result<()> {
    let rc = unsafe { libc::unlinkat(dir.as_raw_fd(), name.as_ptr(), 0) };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    return Ok(());
}

fn entry_stat(dir: &fs::File, name: &CStr) -> io::Result<Option<libc::stat>> {
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    let rc = unsafe {
        libc::fstatat(
            dir.as_raw_fd(),
            name.as_ptr(),
            &mut st,
            libc::AT_SYMLINK_NOFOLLOW,
        )
    };
    if rc == 0 {
        return Ok(Some(st));
    }
    let e = io::Error::last_os_error();
    return match e.kind() {
        io::ErrorKind::NotFound => Ok(None),
        _ => Err(e),
    };
}

fn entry_exists(dir: &fs::File, name: &CStr) -> io::Result<bool> {
    return Ok(entry_stat(dir, name)?.is_some());
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum InitStep {
    Created,
    MarkerCreated,
    MarkerWritten,
    MarkerSynced,
    InitSynced,
    Published,
    PublishSynced,
}

fn open_staging_dir(parent: &fs::File) -> io::Result<fs::File> {
    return open_staging_dir_with(parent, effective_uid(), |_| Ok(()));
}

fn open_staging_dir_with(
    parent: &fs::File,
    owner: u32,
    step: impl Fn(InitStep) -> io::Result<()>,
) -> io::Result<fs::File> {
    let staging = c_string(STAGING_DIR)?;
    if entry_exists(parent, &staging)? {
        return open_published_staging(parent, owner, &staging);
    }
    let init = c_string(STAGING_INIT)?;
    if unsafe { libc::mkdirat(parent.as_raw_fd(), init.as_ptr(), 0o700) } == 0 {
        step(InitStep::Created)?;
    } else {
        let e = io::Error::last_os_error();
        if e.kind() != io::ErrorKind::AlreadyExists {
            return Err(e);
        }
    }
    let dir = open_init_dir(parent, &init, owner)?;
    complete_marker(&dir, owner, &step)?;
    dir.sync_all()?;
    step(InitStep::InitSynced)?;
    rename_at(parent, &init, parent, &staging)?;
    step(InitStep::Published)?;
    parent.sync_all()?;
    step(InitStep::PublishSynced)?;
    return open_published_staging(parent, owner, &staging);
}

fn owned_privately(meta: &fs::Metadata, owner: u32) -> bool {
    return meta.uid() == owner && meta.mode() & 0o077 == 0;
}

fn reopen(path: &fs::File) -> io::Result<fs::File> {
    return fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(format!("/proc/self/fd/{}", path.as_raw_fd()));
}

fn restore_owner_bits(path: &fs::File, want: u32) -> io::Result<()> {
    if path.metadata()?.mode() & 0o777 == want {
        return Ok(());
    }
    return fs::set_permissions(
        format!("/proc/self/fd/{}", path.as_raw_fd()),
        fs::Permissions::from_mode(want),
    );
}

fn init_entries(dir: &fs::File) -> io::Result<Vec<std::ffi::OsString>> {
    let names = list_entries(dir, INIT_ENTRIES + 1)?;
    if names.len() > INIT_ENTRIES || names.iter().any(|name| name != STAGING_MARKER) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "staging initialization directory holds entries other than its ownership marker, so it is not used: stop any updater that may be using this checkpoint, inspect the entries, and move them aside if they are not the updater's, then run again. If the owner cannot search or write it, chmod u+wx it first, or move the whole directory aside unchanged. If the owner could not read it before this run, its owner permissions were restored and stay restored. This message does not establish whether that happened, since it depends on the mode before this run",
        ));
    }
    return Ok(names);
}

fn open_init_dir(parent: &fs::File, init: &CStr, owner: u32) -> io::Result<fs::File> {
    let path = open_at(parent, init, libc::O_PATH | libc::O_DIRECTORY, 0)?;
    let meta = path.metadata()?;
    if !meta.is_dir() || !owned_privately(&meta, owner) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "staging initialization directory must be owned by the updater and private",
        ));
    }
    if meta.mode() & 0o400 != 0 {
        let names = init_entries(&path)?;
        if meta.mode() & 0o100 != 0 && !names.is_empty() {
            check_marker(&path, &c_string(STAGING_MARKER)?, owner)?;
        }
    }
    restore_owner_bits(&path, 0o700)?;
    return reopen(&path);
}

fn create_marker(dir: &fs::File, step: &impl Fn(InitStep) -> io::Result<()>) -> io::Result<()> {
    let mut file = open_at(
        dir,
        &c_string(STAGING_MARKER)?,
        libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL,
        0o600,
    )?;
    step(InitStep::MarkerCreated)?;
    file.set_permissions(fs::Permissions::from_mode(0o600))?;
    file.write_all(MARKER_CONTENT)?;
    return step(InitStep::MarkerWritten);
}

fn check_marker(dir: &fs::File, marker: &CStr, owner: u32) -> io::Result<(fs::File, fs::Metadata)> {
    let path = open_at(dir, marker, libc::O_PATH, 0)?;
    let meta = path.metadata()?;
    if !meta.is_file()
        || meta.nlink() != 1
        || !owned_privately(&meta, owner)
        || meta.len() > MARKER_CONTENT.len() as u64
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "staging initialization marker must be a private, single-link file owned by the updater, and no longer than its contents. If the owner could not read or search the directory before this run, its owner permissions were restored to check the marker, and they stay restored. This message does not establish whether that happened, since it depends on the mode before this run",
        ));
    }
    return Ok((path, meta));
}

fn read_marker(dir: &fs::File, marker: &CStr, owner: u32) -> io::Result<Vec<u8>> {
    let (path, meta) = check_marker(dir, marker, owner)?;
    restore_owner_bits(&path, 0o600)?;
    let mut content = Vec::new();
    reopen(&path)?.take(meta.len()).read_to_end(&mut content)?;
    return Ok(content);
}

fn complete_marker(
    dir: &fs::File,
    owner: u32,
    step: &impl Fn(InitStep) -> io::Result<()>,
) -> io::Result<()> {
    let marker = c_string(STAGING_MARKER)?;
    if init_entries(dir)?.is_empty() {
        create_marker(dir, step)?;
    } else if read_marker(dir, &marker, owner)? != MARKER_CONTENT {
        unlink_at(dir, &marker)?;
        create_marker(dir, step)?;
    }
    open_at(dir, &marker, libc::O_RDONLY | libc::O_NONBLOCK, 0)?.sync_all()?;
    return step(InitStep::MarkerSynced);
}

fn list_entries(dir: &fs::File, limit: usize) -> io::Result<Vec<std::ffi::OsString>> {
    return list_dir(dir)?
        .take(limit)
        .map(|entry| entry.map(|e| e.file_name()))
        .collect();
}

fn list_dir(dir: &fs::File) -> io::Result<fs::ReadDir> {
    return fs::read_dir(format!("/proc/self/fd/{}", dir.as_raw_fd()));
}

fn open_published_staging(parent: &fs::File, owner: u32, staging: &CStr) -> io::Result<fs::File> {
    let dir = open_at(
        parent,
        staging,
        libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NONBLOCK,
        0,
    )?;
    let meta = dir.metadata()?;
    if !meta.is_dir() || !owned_privately(&meta, owner) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "staging directory must be owned by the updater and private",
        ));
    }
    let mut marker = open_at(
        &dir,
        &c_string(STAGING_MARKER)?,
        libc::O_RDONLY | libc::O_NONBLOCK,
        0,
    )?;
    let meta = marker.metadata()?;
    if !meta.is_file()
        || !owned_privately(&meta, owner)
        || meta.nlink() != 1
        || meta.len() != MARKER_CONTENT.len() as u64
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "invalid staging directory ownership marker",
        ));
    }
    let mut content = vec![0; MARKER_CONTENT.len()];
    marker.read_exact(&mut content)?;
    if content != MARKER_CONTENT {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid staging directory ownership marker",
        ));
    }
    return Ok(dir);
}

fn sweep_window(dir: &fs::File) -> io::Result<Vec<std::ffi::OsString>> {
    return Ok(list_dir(dir)?
        .take(SWEEP_LIMIT)
        .flatten()
        .map(|entry| entry.file_name())
        .collect());
}

fn sweep_stale_staging(dir: &fs::File, list: ListFn) {
    let Ok(names) = list(dir) else {
        return;
    };
    for name in names {
        let Some(name) = name.to_str() else {
            continue;
        };
        if !is_staging_name(name) {
            continue;
        }
        let Ok(c_name) = c_string(name) else {
            continue;
        };
        if let Ok(Some(st)) = entry_stat(dir, &c_name) {
            let regular = st.st_mode as u32 & libc::S_IFMT == libc::S_IFREG;
            if regular && st.st_nlink == 1 {
                let _ = unlink_at(dir, &c_name);
            }
        }
    }
}

fn remove_stale_candidates(
    staging: &fs::File,
    record: &[u8; RECORD_SIZE],
    lookup: StatFn,
    unlink: UnlinkFn,
) -> io::Result<()> {
    for n in 0..TEMPORARY_ATTEMPTS as u64 {
        let candidate = c_string(&staging_name(record, n))?;
        let Some(st) = lookup(staging, &candidate)? else {
            continue;
        };
        if st.st_mode as u32 & libc::S_IFMT == libc::S_IFREG && st.st_nlink == 1 {
            unlink(staging, &candidate)?;
        }
    }
    return Ok(());
}

fn remove_record_staging_link(
    staging: &fs::File,
    dir: &fs::File,
    name: &CStr,
    lookup: StatFn,
) -> io::Result<()> {
    let Some(record) = lookup(dir, name)? else {
        return Ok(());
    };
    if record.st_mode as u32 & libc::S_IFMT != libc::S_IFREG || record.st_nlink != 2 {
        return Ok(());
    }
    let Some(bytes) = read_record_bytes(dir, name)? else {
        return Ok(());
    };
    for n in 0..TEMPORARY_ATTEMPTS as u64 {
        let candidate = c_string(&staging_name(&bytes, n))?;
        let Some(st) = lookup(staging, &candidate)? else {
            continue;
        };
        if st.st_dev == record.st_dev && st.st_ino == record.st_ino {
            unlink_at(staging, &candidate)?;
            return Ok(());
        }
    }
    return Ok(());
}

fn unlink_record_staging_link(
    staging: &fs::File,
    dir: &fs::File,
    name: &CStr,
    lookup: StatFn,
    dir_sync: DirSyncFn,
) -> io::Result<()> {
    remove_record_staging_link(staging, dir, name, lookup)?;
    return dir_sync(staging);
}

fn read_record_bytes(dir: &fs::File, name: &CStr) -> io::Result<Option<[u8; RECORD_SIZE]>> {
    let mut file = open_at(dir, name, libc::O_RDONLY | libc::O_NONBLOCK, 0)?;
    if file.metadata()?.len() != RECORD_SIZE as u64 {
        return Ok(None);
    }
    let mut bytes = [0u8; RECORD_SIZE];
    file.read_exact(&mut bytes)?;
    return Ok(Some(bytes));
}

fn hold_record_lock(dir: &fs::File, name: &CStr) -> Result<Option<FileLock>, String> {
    let stat = match entry_stat(dir, name) {
        Err(e) => return Err(format!("cannot look up the checkpoint: {}", e)),
        Ok(st) => st,
    };
    return match stat {
        None => Ok(None),
        Some(st) if st.st_mode as u32 & libc::S_IFMT != libc::S_IFREG => Ok(None),
        Some(_) => match open_at(dir, name, libc::O_RDONLY | libc::O_NONBLOCK, 0) {
            Err(e) => Err(format!("cannot open the checkpoint to lock it: {}", e)),
            Ok(file) => match try_lock_open(&file, LockMode::Exclusive) {
                Err(e) => Err(e.message("the checkpoint record")),
                Ok(()) => Ok(Some(hold(file))),
            },
        },
    };
}

type EntryFn = fn(&fs::File, &CStr) -> io::Result<bool>;

fn ensure_case_sensitive(dir: &fs::File) -> Result<(), String> {
    static PROBES: AtomicU64 = AtomicU64::new(0);
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let probe = probe_name(
        std::process::id(),
        stamp,
        PROBES.fetch_add(1, Ordering::Relaxed),
    );
    let upper = probe.to_ascii_uppercase();
    let unable = |e: io::Error| {
        format!(
            "cannot establish that the checkpoint directory is case-sensitive: {}",
            e
        )
    };
    let probe_name = c_string(&probe).map_err(unable)?;
    let upper_name = c_string(&upper).map_err(unable)?;
    if let Err(e) = open_at(
        dir,
        &probe_name,
        libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL,
        0o600,
    ) {
        return Err(unable(e));
    }
    let verdict = match open_at(dir, &upper_name, libc::O_RDONLY, 0) {
        Ok(_) => Err(format!(
            "the checkpoint directory folds the case of ASCII names, so {} and {} would be one record; use a case-sensitive directory",
            probe, upper
        )),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(unable(e)),
    };
    return match (verdict, unlink_at(dir, &probe_name)) {
        (Err(msg), _) => Err(msg),
        (Ok(()), Err(e)) => Err(unable(e)),
        (Ok(()), Ok(())) => Ok(()),
    };
}

pub struct FileCheckpoint {
    dir: fs::File,
    staging_dir: fs::File,
    c_name: CString,
    dir_sync: DirSyncFn,
    entry: Entry,
    _entry: FileLock,
    _lock: FileLock,
    record_lock: Option<FileLock>,
}

impl FileCheckpoint {
    /// Lock the directory of the record at `path` in the lock namespace `namespace`, and open its store. This is the only
    /// way to get a store, and the store holds the lock, so the lock and the directory it was taken in cannot come apart.
    ///
    /// The record's directory is opened once, following any symbolic links in its path, and its handle is kept. The lock's
    /// name derives from that handle's identity (device and inode) alone. So two paths that reach one directory name one
    /// lock, whether or not the record exists yet, and every record in the directory shares that lock. A record whose
    /// name is reserved is refused before any lock is taken or any probe is made. A record that would live in the
    /// namespace is refused, because its name could then clash with a lock file.
    pub fn open(namespace: &LockDir, path: &str) -> Result<FileCheckpoint, String> {
        return FileCheckpoint::open_with(
            namespace,
            path,
            sync_dir,
            ensure_case_sensitive,
            sweep_window,
        );
    }

    /// As [`open`](FileCheckpoint::open), with the directory sync supplied by the caller.
    pub fn open_with_dir_sync(
        namespace: &LockDir,
        path: &str,
        dir_sync: DirSyncFn,
    ) -> Result<FileCheckpoint, String> {
        return FileCheckpoint::open_with(
            namespace,
            path,
            dir_sync,
            ensure_case_sensitive,
            sweep_window,
        );
    }

    fn open_with(
        namespace: &LockDir,
        path: &str,
        dir_sync: DirSyncFn,
        case_probe: CaseProbeFn,
        list: ListFn,
    ) -> Result<FileCheckpoint, String> {
        let record = Path::new(path);
        let raw = match record.file_name() {
            None => return Err(format!("checkpoint {} does not name a file", path)),
            Some(n) => n,
        };
        let name = match raw.to_str() {
            Some(n) if n.bytes().all(|b| b.is_ascii_graphic()) => n,
            _ => {
                return Err(format!(
                    "checkpoint name {:?} must be printable ASCII with no space, so that its spelling is its only identity",
                    raw
                ));
            }
        };
        if is_reserved(name) {
            return Err(format!(
                "checkpoint name {:?} begins with {:?}, which is reserved for the updater's own files; choose another name",
                name, RESERVED_PREFIX
            ));
        }
        let c_name = match c_string(name) {
            Err(e) => return Err(format!("checkpoint name {:?}: {}", name, e)),
            Ok(c) => c,
        };
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
            Err(e) => return Err(format!("checkpoint directory {}: {}", parent.display(), e)),
            Ok(d) => d,
        };
        if crate::roles::inside_staging(&dir)
            .map_err(|e| format!("cannot inspect checkpoint directory ancestry: {}", e))?
        {
            return Err(format!(
                "checkpoint {} is inside the updater's staging namespace",
                path
            ));
        }
        let meta = match dir.metadata() {
            Err(e) => return Err(format!("checkpoint directory {}: {}", parent.display(), e)),
            Ok(m) => m,
        };
        let identity = (meta.dev(), meta.ino());
        if identity == namespace.identity() {
            return Err(format!(
                "checkpoint {} names a file in the lock directory",
                path
            ));
        }
        let entry = Entry {
            dir: identity,
            name: name.to_string(),
        };
        let entry_lock = match namespace.take_entry(&entry, LockMode::Exclusive) {
            Err(e) => return Err(e.message(&format!("the checkpoint {}", path))),
            Ok(lock) => lock,
        };
        let lock = match namespace.take(&checkpoint_lock_name(identity)) {
            Err(e) => return Err(e.message(&format!("the checkpoint {}", path))),
            Ok(lock) => lock,
        };
        case_probe(&dir)?;
        let staging_dir = open_staging_dir(&dir)
            .map_err(|e| format!("cannot use checkpoint staging directory: {}", e))?;
        sweep_stale_staging(&staging_dir, list);
        let record_lock = match hold_record_lock(&dir, &c_name) {
            Err(msg) => return Err(msg),
            Ok(record_lock) => record_lock,
        };
        if let Err(e) =
            unlink_record_staging_link(&staging_dir, &dir, &c_name, entry_stat, dir_sync)
        {
            return Err(format!(
                "cannot settle the checkpoint's staging links, so the run stops before any write: {}",
                e
            ));
        }
        return Ok(FileCheckpoint {
            dir: dir,
            staging_dir: staging_dir,
            c_name: c_name,
            entry: entry,
            dir_sync: dir_sync,
            _entry: entry_lock,
            _lock: lock,
            record_lock: record_lock,
        });
    }

    /// The entry this store's record is in: the record's name, and the identity of the directory the store pinned. A
    /// run uses it to find the name of the lock the store holds, so it can avoid taking that lock twice against itself
    /// (`roles.rs`).
    pub fn entry(self: &FileCheckpoint) -> Entry {
        return Entry {
            dir: self.entry.dir,
            name: self.entry.name.clone(),
        };
    }

    fn create_temporary(&self, record: &[u8; RECORD_SIZE]) -> io::Result<(CString, fs::File)> {
        return match self.claim_temporary(record) {
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                remove_stale_candidates(&self.staging_dir, record, entry_stat, unlink_at)?;
                self.claim_temporary(record)
            }
            other => other,
        };
    }

    fn claim_temporary(&self, record: &[u8; RECORD_SIZE]) -> io::Result<(CString, fs::File)> {
        for n in 0..TEMPORARY_ATTEMPTS as u64 {
            let tmp = c_string(&staging_name(record, n))?;
            match open_at(
                &self.staging_dir,
                &tmp,
                libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL,
                0o600,
            ) {
                Ok(file) => {
                    if let Err(e) = file.set_permissions(fs::Permissions::from_mode(0o600)) {
                        let _ = unlink_at(&self.staging_dir, &tmp);
                        return Err(e);
                    }
                    return Ok((tmp, file));
                }
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
                Err(e) => return Err(e),
            };
        }
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "no free temporary name",
        ));
    }

    fn write_record(&mut self, record: &[u8; RECORD_SIZE]) -> io::Result<()> {
        self.write_record_with(record, |_, _| Ok(()))
    }

    fn write_record_with(
        &mut self,
        record: &[u8; RECORD_SIZE],
        before_rename: impl FnOnce(&fs::File, &CStr) -> io::Result<()>,
    ) -> io::Result<()> {
        let (tmp, mut file) = self.create_temporary(record)?;
        let staged = try_lock_open(&file, LockMode::Exclusive)
            .map_err(|e| io::Error::other(e.message("the checkpoint staging file")))
            .and_then(|()| file.write_all(record))
            .and_then(|()| file.sync_all())
            .and_then(|()| before_rename(&self.staging_dir, &tmp));
        match staged.and_then(|()| rename_at(&self.staging_dir, &tmp, &self.dir, &self.c_name)) {
            Err(e) => {
                let _ = unlink_at(&self.staging_dir, &tmp);
                return Err(e);
            }
            Ok(()) => {}
        };
        self.record_lock = Some(hold(file));
        return self.sync_published();
    }

    fn sync_directory(&self) -> io::Result<()> {
        return (self.dir_sync)(&self.dir);
    }

    fn sync_published(&self) -> io::Result<()> {
        self.sync_directory()?;
        return (self.dir_sync)(&self.staging_dir);
    }

    /// The identity of the entry at the record's name, read through the pinned directory without following a link, or `None`
    /// when there is no entry. A run compares it with its other roles before it writes. A symbolic link at the name has its
    /// own identity, which no other role can share, and `load` refuses the link itself.
    pub fn entry_identity(&self) -> io::Result<Option<Identity>> {
        return Ok(entry_stat(&self.dir, &self.c_name)?.map(|st| {
            Identity::from_stat(
                st.st_mode as u32,
                st.st_dev as u64,
                st.st_ino as u64,
                st.st_rdev as u64,
            )
        }));
    }

    fn confirm_absent_with(&mut self, lookup: EntryFn) -> Result<(), PayloadError> {
        match lookup(&self.dir, &self.c_name) {
            Err(_e) => {
                return Err(err(
                    Code::checkpoint_removal_uncertain,
                    "checkpoint absence cannot be checked, so it is not confirmed",
                ));
            }
            Ok(true) => {
                return Err(err(
                    Code::checkpoint_present,
                    "checkpoint is present, so the reset that expected none removes nothing",
                ));
            }
            Ok(false) => {}
        };
        return match self.sync_directory() {
            Err(_e) => Err(err(
                Code::checkpoint_removal_uncertain,
                "checkpoint is absent, but its absence is not confirmed durable",
            )),
            Ok(()) => Ok(()),
        };
    }
}

impl CheckpointStore for FileCheckpoint {
    /// Read the record into a fixed-size buffer. The file is opened once, without following
    /// a symbolic link, and every check is made on that open file: it must be a regular file
    /// with one name, and its length is the length read. Any other size is refused before a
    /// record is read, and a file that grows while it is read is refused after it. A
    /// replacement renames over the name, so a record with another name would be left stale.
    fn load(&self, out: &mut [u8; RECORD_SIZE]) -> Result<bool, PayloadError> {
        let mut file = match open_at(
            &self.dir,
            &self.c_name,
            libc::O_RDONLY | libc::O_NONBLOCK,
            0,
        ) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(false),
            Err(e) if e.raw_os_error() == Some(libc::ELOOP) => {
                return Err(err(
                    Code::checkpoint_invalid,
                    "checkpoint is a symbolic link; name the record file itself",
                ));
            }
            Err(_e) => return Err(io_error()),
            Ok(f) => f,
        };
        let meta = match file.metadata() {
            Err(_e) => return Err(io_error()),
            Ok(m) => m,
        };
        if !meta.is_file() {
            return Err(err(
                Code::checkpoint_invalid,
                "checkpoint is not a regular file",
            ));
        }
        if meta.nlink() != 1 {
            return Err(err(
                Code::checkpoint_invalid,
                "checkpoint file has more than one name; a replacement would leave the others stale",
            ));
        }
        let len = meta.len();
        if len != RECORD_SIZE as u64 {
            return Err(err(
                Code::checkpoint_invalid,
                "checkpoint file has the wrong length",
            ));
        }
        let mut record = [0u8; RECORD_SIZE];
        match file.read_exact(&mut record) {
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => {
                return Err(err(
                    Code::checkpoint_invalid,
                    "checkpoint file is truncated",
                ));
            }
            Err(_e) => return Err(io_error()),
            Ok(()) => {}
        };
        let mut probe = [0u8; 1];
        match file.read(&mut probe) {
            Err(_e) => return Err(io_error()),
            Ok(0) => {}
            Ok(_) => {
                return Err(err(
                    Code::checkpoint_invalid,
                    "checkpoint file changed while it was read",
                ));
            }
        };
        out.copy_from_slice(&record);
        return Ok(true);
    }

    fn replace(&mut self, record: &[u8; RECORD_SIZE]) -> Result<(), PayloadError> {
        return match self.write_record(record) {
            Err(_e) => Err(io_error()),
            Ok(()) => Ok(()),
        };
    }

    /// Sync both directories again. A replacement that failed after its rename left the new record visible, and a
    /// later process cannot tell whether that sync ever succeeded, so this always syncs. A sync that fails is
    /// `checkpoint_unconfirmed`: the record is not used, and it is kept. The sync is of the pinned handles, under the
    /// record's lock, so it is the sync of the directories the store locked.
    fn confirm_stored(&mut self) -> Result<(), PayloadError> {
        return match self.sync_published() {
            Err(_e) => Err(err(
                Code::checkpoint_unconfirmed,
                "checkpoint is stored, but its replacement is not confirmed durable; it is not used",
            )),
            Ok(()) => Ok(()),
        };
    }

    /// Remove the record, then sync the directory so the removal is durable. A failure to remove
    /// leaves the record as it was, and is a plain storage error. A failure to sync after the
    /// removal is `checkpoint_removal_uncertain`: the record is gone now, but its removal is not on
    /// disk, so it may come back after a crash. Once the name is free the store lets go of the lock it held on the
    /// record's inode: nothing is left at the entry to protect, and a run that names the entry again may now take it.
    fn clear(&mut self) -> Result<(), PayloadError> {
        match unlink_at(&self.dir, &self.c_name) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(_e) => return Err(io_error()),
            Ok(()) => {
                self.record_lock = None;
            }
        };
        return match self.sync_directory() {
            Err(_e) => Err(err(
                Code::checkpoint_removal_uncertain,
                "checkpoint removed, but the removal is not confirmed durable",
            )),
            Ok(()) => Ok(()),
        };
    }

    /// Confirm that no record is stored, and that its absence is durable. The name is looked up in the pinned
    /// directory without following a link. Any entry there, a symbolic link included, is a stored record: it is
    /// refused as `checkpoint_present`, and nothing is removed. Only a free name is synced, and the sync is of the
    /// pinned directory, under the record's lock. A failed look-up or a failed sync is `checkpoint_removal_uncertain`:
    /// the name's state, or an earlier removal's durability, is then unknown, so the record may come back after a crash.
    fn confirm_absent(&mut self) -> Result<(), PayloadError> {
        return self.confirm_absent_with(entry_exists);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::PermissionsExt;
    use std::os::unix::fs::symlink;
    use std::path::PathBuf;

    const RECORD_NAME: &str = "ckpt";

    fn scratch(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("ota-checkpoint-{}-{}", std::process::id(), name));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        return dir;
    }

    fn namespace(dir: &Path) -> LockDir {
        let ns = dir.join("lock-dir");
        fs::create_dir_all(&ns).unwrap();
        fs::set_permissions(&ns, fs::Permissions::from_mode(0o700)).unwrap();
        return LockDir::open(&ns).unwrap();
    }

    fn record_path(dir: &Path) -> String {
        return dir.join(RECORD_NAME).to_str().unwrap().to_string();
    }

    fn store_in(dir: &Path) -> FileCheckpoint {
        return FileCheckpoint::open(&namespace(dir), &record_path(dir)).unwrap();
    }

    fn store_with_sync(dir: &Path, sync: DirSyncFn) -> FileCheckpoint {
        return FileCheckpoint::open_with_dir_sync(&namespace(dir), &record_path(dir), sync)
            .unwrap();
    }

    fn loaded(store: &FileCheckpoint) -> Option<[u8; RECORD_SIZE]> {
        let mut out = [0u8; RECORD_SIZE];
        return if store.load(&mut out).unwrap() {
            Some(out)
        } else {
            None
        };
    }

    #[test]
    fn checkpoint_records_cannot_live_inside_the_internal_staging_directory_even_through_an_alias()
    {
        let dir = scratch("nested-staging-record");
        let ns = namespace(&dir);
        drop(FileCheckpoint::open(&ns, &record_path(&dir)).unwrap());
        let alias = dir.join("alias");
        symlink(dir.join(STAGING_DIR), &alias).unwrap();
        let nested = alias.join("record");
        let error = FileCheckpoint::open(&ns, nested.to_str().unwrap())
            .err()
            .unwrap();
        assert!(
            error.contains("inside the updater's staging namespace"),
            "{}",
            error
        );
        assert!(!nested.exists());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn legacy_staging_names_in_the_record_directory_are_never_swept() {
        let dir = scratch("legacy-prefix-preserved");
        let legacy = dir.join(staging_name(&[9; RECORD_SIZE], 0));
        fs::write(&legacy, b"unrelated data").unwrap();
        let mut store = store_in(&dir);
        store.replace(&[1; RECORD_SIZE]).unwrap();
        sweep_stale_staging(&store.staging_dir, sweep_window);
        drop(store);
        drop(store_in(&dir));
        assert_eq!(fs::read(&legacy).unwrap(), b"unrelated data");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn untrusted_preexisting_staging_namespaces_are_refused_without_changes() {
        for kind in [
            "symlink",
            "file",
            "unmarked",
            "public",
            "bad-marker",
            "linked-marker",
            "symlink-marker",
        ] {
            let dir = scratch(&format!("staging-refuse-{kind}"));
            let ns = namespace(&dir);
            let path = dir.join(STAGING_DIR);
            let victim = dir.join("victim");
            fs::write(&victim, MARKER_CONTENT).unwrap();
            match kind {
                "symlink" => symlink(&dir, &path).unwrap(),
                "file" => fs::write(&path, b"keep").unwrap(),
                _ => {
                    fs::create_dir(&path).unwrap();
                    fs::set_permissions(
                        &path,
                        fs::Permissions::from_mode(if kind == "public" { 0o755 } else { 0o700 }),
                    )
                    .unwrap();
                    if kind == "bad-marker" {
                        fs::write(path.join(STAGING_MARKER), b"not ours").unwrap();
                    }
                    if kind == "linked-marker" {
                        fs::set_permissions(&victim, fs::Permissions::from_mode(0o600)).unwrap();
                        fs::hard_link(&victim, path.join(STAGING_MARKER)).unwrap();
                    }
                    if kind == "symlink-marker" {
                        symlink(&victim, path.join(STAGING_MARKER)).unwrap();
                    }
                    fs::write(path.join(staging_name(&[3; RECORD_SIZE], 3)), b"keep").unwrap();
                }
            }
            assert!(
                FileCheckpoint::open(&ns, &record_path(&dir)).is_err(),
                "{kind}"
            );
            assert_eq!(fs::read(&victim).unwrap(), MARKER_CONTENT);
            if path.is_dir() && kind != "symlink" {
                assert_eq!(
                    fs::read(path.join(staging_name(&[3; RECORD_SIZE], 3))).unwrap(),
                    b"keep"
                );
            }
            fs::remove_dir_all(&dir).unwrap();
        }
    }

    #[test]
    fn cleanup_is_bounded_and_can_continue_on_later_opens() {
        let dir = scratch("bounded-sweep");
        drop(store_in(&dir));
        let staging = dir.join(STAGING_DIR);
        assert_eq!(fs::metadata(&staging).unwrap().mode() & 0o777, 0o700);
        assert_eq!(
            fs::metadata(staging.join(STAGING_MARKER)).unwrap().mode() & 0o777,
            0o600
        );
        for n in 0..(SWEEP_LIMIT + 50) {
            fs::write(
                staging.join(staging_name(&[2; RECORD_SIZE], n as u64)),
                b"debris",
            )
            .unwrap();
        }
        drop(store_in(&dir));
        let left = fs::read_dir(&staging).unwrap().count() - 1;
        assert!(left >= 50 && left < SWEEP_LIMIT + 50, "{left}");
        drop(store_in(&dir));
        assert_eq!(fs::read_dir(&staging).unwrap().count(), 1);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn staging_inode_is_locked_before_rename_and_remains_locked_after_publication() {
        let dir = scratch("staging-publication-lock");
        let mut store = store_in(&dir);
        store.replace(&[1; RECORD_SIZE]).unwrap();
        let mut contender = None;
        store
            .write_record_with(&[2; RECORD_SIZE], |staging, name| {
                let file = open_at(staging, name, libc::O_RDONLY, 0)?;
                assert!(try_lock_open(&file, LockMode::Exclusive).is_err());
                assert_eq!(
                    fs::read(dir.join(RECORD_NAME)).unwrap(),
                    vec![1; RECORD_SIZE]
                );
                contender = Some(file);
                Ok(())
            })
            .unwrap();
        let file = contender.unwrap();
        assert_eq!(
            file.metadata().unwrap().ino(),
            fs::metadata(dir.join(RECORD_NAME)).unwrap().ino()
        );
        assert!(try_lock_open(&file, LockMode::Exclusive).is_err());
        drop(store);
        assert!(try_lock_open(&file, LockMode::Exclusive).is_ok());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn failure_before_publication_preserves_old_record_and_removes_staging_file() {
        let dir = scratch("staging-failed-publication");
        let mut store = store_in(&dir);
        store.replace(&[1; RECORD_SIZE]).unwrap();
        assert!(
            store
                .write_record_with(&[2; RECORD_SIZE], |_, _| Err(io::Error::other("injected")))
                .is_err()
        );
        assert_eq!(loaded(&store), Some([1; RECORD_SIZE]));
        assert_eq!(fs::read_dir(dir.join(STAGING_DIR)).unwrap().count(), 1);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_missing_record_is_no_record() {
        let dir = scratch("missing");
        assert_eq!(loaded(&store_in(&dir)), None);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_stored_record_loads_back_and_a_replacement_supersedes_it() {
        let dir = scratch("round-trip");
        let mut store = store_in(&dir);
        store.replace(&[1u8; RECORD_SIZE]).unwrap();
        assert_eq!(loaded(&store), Some([1u8; RECORD_SIZE]));
        store.replace(&[2u8; RECORD_SIZE]).unwrap();
        assert_eq!(loaded(&store), Some([2u8; RECORD_SIZE]));
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_file_of_exactly_one_record_is_read_whole() {
        let dir = scratch("exact");
        fs::write(dir.join(RECORD_NAME), [7u8; RECORD_SIZE]).unwrap();
        assert_eq!(loaded(&store_in(&dir)), Some([7u8; RECORD_SIZE]));
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_file_of_any_other_size_is_refused_as_invalid() {
        let dir = scratch("sizes");
        let path = dir.join(RECORD_NAME);
        let store = store_in(&dir);
        let mut out = [0u8; RECORD_SIZE];
        for len in [0, RECORD_SIZE - 1, RECORD_SIZE + 1] {
            fs::write(&path, vec![0u8; len]).unwrap();
            assert_eq!(
                store.load(&mut out).err().map(|e| e.code),
                Some(Code::checkpoint_invalid),
                "a {len}-byte file was accepted"
            );
        }
        fs::File::create(&path).unwrap().set_len(1 << 32).unwrap();
        assert_eq!(
            store.load(&mut out).err().map(|e| e.code),
            Some(Code::checkpoint_invalid)
        );
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_new_record_has_no_group_or_other_permissions() {
        let dir = scratch("mode");
        let mut store = store_in(&dir);
        store.replace(&[3u8; RECORD_SIZE]).unwrap();
        let mode = fs::metadata(dir.join(RECORD_NAME))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600, "record is not 0600: {:o}", mode);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_leftover_temporary_file_does_not_block_a_replacement() {
        let dir = scratch("leftover");
        let mut store = store_in(&dir);
        store.replace(&[1u8; RECORD_SIZE]).unwrap();
        let leftover = dir
            .join(STAGING_DIR)
            .join(staging_name(&[2u8; RECORD_SIZE], 0));
        fs::write(&leftover, b"crash debris").unwrap();
        store.replace(&[2u8; RECORD_SIZE]).unwrap();
        assert_eq!(loaded(&store), Some([2u8; RECORD_SIZE]));
        assert_eq!(fs::read(&leftover).unwrap(), b"crash debris");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_legacy_temporary_symlink_cannot_redirect_a_write() {
        let dir = scratch("legacy-symlink");
        let victim = dir.join("victim");
        fs::write(&victim, b"do not touch").unwrap();
        let mut store = store_in(&dir);
        symlink(&victim, dir.join(format!("{}.tmp", RECORD_NAME))).unwrap();
        store.replace(&[4u8; RECORD_SIZE]).unwrap();
        assert_eq!(fs::read(&victim).unwrap(), b"do not touch");
        assert_eq!(loaded(&store), Some([4u8; RECORD_SIZE]));
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_symbolic_link_to_a_record_is_refused_and_its_target_is_untouched() {
        let dir = scratch("record-symlink");
        let victim = dir.join("victim");
        fs::write(&victim, [9u8; RECORD_SIZE]).unwrap();
        symlink(&victim, dir.join(RECORD_NAME)).unwrap();
        let store = store_in(&dir);
        let mut out = [0u8; RECORD_SIZE];
        assert_eq!(
            store.load(&mut out).err().map(|e| e.code),
            Some(Code::checkpoint_invalid)
        );
        assert_eq!(fs::read(&victim).unwrap(), vec![9u8; RECORD_SIZE]);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_named_pipe_is_refused_without_waiting_for_a_writer() {
        let dir = scratch("record-fifo");
        let path = dir.join(RECORD_NAME);
        let name = std::ffi::CString::new(path.to_str().unwrap()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
        let store = store_in(&dir);
        let mut out = [0u8; RECORD_SIZE];
        assert_eq!(
            store.load(&mut out).err().map(|e| e.code),
            Some(Code::checkpoint_invalid)
        );
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_dangling_symbolic_link_is_refused_rather_than_read_as_no_record() {
        let dir = scratch("record-dangling");
        symlink(dir.join("missing"), dir.join(RECORD_NAME)).unwrap();
        let store = store_in(&dir);
        let mut out = [0u8; RECORD_SIZE];
        assert_eq!(
            store.load(&mut out).err().map(|e| e.code),
            Some(Code::checkpoint_invalid)
        );
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_record_with_a_second_name_is_refused_before_it_is_read() {
        let dir = scratch("record-linked");
        fs::write(dir.join(RECORD_NAME), [5u8; RECORD_SIZE]).unwrap();
        fs::hard_link(dir.join(RECORD_NAME), dir.join("other-name")).unwrap();
        let store = store_in(&dir);
        let mut out = [0u8; RECORD_SIZE];
        assert_eq!(
            store.load(&mut out).err().map(|e| e.code),
            Some(Code::checkpoint_invalid)
        );
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_role_reaching_the_record_through_a_second_name_meets_the_records_own_inode_lock() {
        use crate::filedevice::FileDevice;
        let dir = scratch("record-inode-lock");
        let mut store = store_in(&dir);
        store.replace(&[1u8; RECORD_SIZE]).unwrap();
        let link = dir.join("second-name").to_str().unwrap().to_string();
        fs::hard_link(dir.join(RECORD_NAME), dir.join("second-name")).unwrap();
        let ns2 = namespace(&dir);
        let refused = FileDevice::open_for_update(&link, &ns2).err().unwrap();
        assert!(refused.contains("another update holds"), "{}", refused);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_store_refuses_a_record_that_is_another_runs_held_file_and_releases_its_locks() {
        use crate::filedevice::FileDevice;
        let dir = scratch("record-held-elsewhere");
        fs::write(dir.join("victim"), [0u8; 64]).unwrap();
        fs::hard_link(dir.join("victim"), dir.join(RECORD_NAME)).unwrap();
        let ns = namespace(&dir);
        let held =
            FileDevice::open_for_update(&dir.join("victim").to_str().unwrap().to_string(), &ns)
                .unwrap();
        let refused = FileCheckpoint::open(&ns, &record_path(&dir)).err().unwrap();
        assert!(
            refused.contains("another update holds the checkpoint record"),
            "{}",
            refused
        );
        drop(held);
        let mut store = FileCheckpoint::open(&ns, &record_path(&dir)).unwrap();
        store.replace(&[1u8; RECORD_SIZE]).unwrap();
        assert_eq!(loaded(&store), Some([1u8; RECORD_SIZE]));
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_store_holds_its_entry_lock_across_the_absence_the_creation_and_the_removal_of_the_record()
     {
        use crate::filedevice::FileDevice;
        let dir = scratch("entry-held-across-record");
        let ns = namespace(&dir);
        let record = record_path(&dir);
        let mut store = FileCheckpoint::open(&ns, &record).unwrap();
        let refused = FileDevice::open_for_update(&record, &ns).err().unwrap();
        assert!(refused.contains("another update holds"), "{}", refused);
        store.replace(&[1u8; RECORD_SIZE]).unwrap();
        let refused = FileDevice::open_for_update(&record, &ns).err().unwrap();
        assert!(refused.contains("another update holds"), "{}", refused);
        store.clear().unwrap();
        assert!(!dir.join(RECORD_NAME).exists());
        let refused = FileDevice::open_for_update(&record, &ns).err().unwrap();
        assert!(refused.contains("another update holds"), "{}", refused);
        drop(store);
        fs::write(dir.join(RECORD_NAME), [1u8; RECORD_SIZE]).unwrap();
        assert!(FileDevice::open_for_update(&record, &ns).is_ok());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn debris_of_an_earlier_replacement_is_removed_when_the_next_store_opens_the_directory() {
        let dir = scratch("names-earlier-debris");
        let mut first = store_in(&dir);
        first.replace(&[1u8; RECORD_SIZE]).unwrap();
        let foreign: Vec<String> = (0..3)
            .map(|n| staging_name(&[7u8; RECORD_SIZE], n))
            .collect();
        for name in &foreign {
            fs::write(dir.join(STAGING_DIR).join(name), b"earlier run").unwrap();
        }
        drop(first);
        let mut resumed = store_in(&dir);
        for name in &foreign {
            assert!(
                !dir.join(STAGING_DIR).join(name).exists(),
                "{} was left behind",
                name
            );
        }
        assert_eq!(loaded(&resumed), Some([1u8; RECORD_SIZE]));
        resumed.replace(&[2u8; RECORD_SIZE]).unwrap();
        assert_eq!(loaded(&resumed), Some([2u8; RECORD_SIZE]));
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_stale_staging_form_entry_the_sweep_may_not_remove_is_skipped_and_the_replacement_proceeds()
    {
        let dir = scratch("names-sweep-skips");
        let mut store = store_in(&dir);
        fs::create_dir(
            dir.join(STAGING_DIR)
                .join(staging_name(&[7u8; RECORD_SIZE], 0)),
        )
        .unwrap();
        let victim = dir.join("victim");
        fs::write(&victim, b"keep").unwrap();
        fs::hard_link(
            &victim,
            dir.join(STAGING_DIR)
                .join(staging_name(&[8u8; RECORD_SIZE], 0)),
        )
        .unwrap();
        sweep_stale_staging(&store.staging_dir, sweep_window);
        store.replace(&[1u8; RECORD_SIZE]).unwrap();
        assert_eq!(loaded(&store), Some([1u8; RECORD_SIZE]));
        assert!(
            fs::symlink_metadata(
                dir.join(STAGING_DIR)
                    .join(staging_name(&[7u8; RECORD_SIZE], 0))
            )
            .is_ok()
        );
        assert_eq!(fs::read(victim).unwrap(), b"keep");
        let linked = dir
            .join(STAGING_DIR)
            .join(staging_name(&[8u8; RECORD_SIZE], 0));
        assert_eq!(fs::metadata(&linked).unwrap().nlink(), 2);
        fs::remove_dir_all(&dir).unwrap();
    }

    fn next_names(record: &[u8; RECORD_SIZE]) -> Vec<String> {
        return (0..TEMPORARY_ATTEMPTS as u64)
            .map(|n| staging_name(record, n))
            .collect();
    }

    fn decoy_names() -> Vec<String> {
        return (0..SWEEP_LIMIT as u64)
            .map(|n| staging_name(&[0xee; RECORD_SIZE], n))
            .collect();
    }

    fn decoy_window(_dir: &fs::File) -> io::Result<Vec<std::ffi::OsString>> {
        return Ok(decoy_names()
            .into_iter()
            .map(std::ffi::OsString::from)
            .collect());
    }

    fn make_decoy_window(staging: &Path) {
        for name in decoy_names() {
            fs::create_dir(staging.join(name)).unwrap();
        }
    }

    fn store_listed(dir: &Path, list: ListFn) -> FileCheckpoint {
        return FileCheckpoint::open_with(
            &namespace(dir),
            &record_path(dir),
            sync_dir,
            ensure_case_sensitive,
            list,
        )
        .unwrap();
    }

    #[test]
    fn stale_staging_files_that_take_every_name_a_replacement_tries_are_removed_and_the_replacement_proceeds()
     {
        let dir = scratch("names-stale");
        let mut store = store_in(&dir);
        store.replace(&[1u8; RECORD_SIZE]).unwrap();
        let stale = next_names(&[2u8; RECORD_SIZE]);
        for name in &stale {
            fs::write(dir.join(STAGING_DIR).join(name), b"stale").unwrap();
        }
        store.replace(&[2u8; RECORD_SIZE]).unwrap();
        assert_eq!(loaded(&store), Some([2u8; RECORD_SIZE]));
        for name in &stale {
            assert!(
                !dir.join(STAGING_DIR).join(name).exists(),
                "{} was not removed",
                name
            );
        }
        let leftovers: Vec<String> = fs::read_dir(dir.join(STAGING_DIR))
            .unwrap()
            .map(|e| e.unwrap().file_name().to_str().unwrap().to_string())
            .filter(|name| is_staging_name(name))
            .collect();
        assert!(leftovers.is_empty(), "{:?}", leftovers);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn entries_of_another_form_that_take_every_name_are_not_removed_and_the_replacement_fails_keeping_the_record()
     {
        let dir = scratch("names-foreign");
        let mut store = store_in(&dir);
        store.replace(&[1u8; RECORD_SIZE]).unwrap();
        let victim = dir.join("victim");
        fs::write(&victim, b"keep").unwrap();
        let names = next_names(&[2u8; RECORD_SIZE]);
        symlink(&victim, dir.join(STAGING_DIR).join(&names[0])).unwrap();
        fs::create_dir(dir.join(STAGING_DIR).join(&names[1])).unwrap();
        for name in &names[2..] {
            fs::hard_link(&victim, dir.join(STAGING_DIR).join(name)).unwrap();
        }
        assert_eq!(
            store.replace(&[2u8; RECORD_SIZE]).err().map(|e| e.code),
            Some(Code::device_io)
        );
        assert_eq!(loaded(&store), Some([1u8; RECORD_SIZE]));
        assert_eq!(fs::read(&victim).unwrap(), b"keep");
        for name in &names {
            assert!(
                fs::symlink_metadata(dir.join(STAGING_DIR).join(name)).is_ok(),
                "{} was removed",
                name
            );
        }
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn stale_files_at_every_name_of_a_replacement_past_a_full_sweep_window_are_removed_and_the_entries_that_cannot_be_removed_stay()
     {
        let dir = scratch("names-past-window");
        let mut store = store_in(&dir);
        store.replace(&[1u8; RECORD_SIZE]).unwrap();
        drop(store);
        let staging = dir.join(STAGING_DIR);
        make_decoy_window(&staging);
        let stale = next_names(&[2u8; RECORD_SIZE]);
        for name in &stale {
            fs::write(staging.join(name), b"stale").unwrap();
        }
        let mut store = store_listed(&dir, decoy_window);
        for name in &stale {
            assert!(staging.join(name).exists(), "{} was swept", name);
        }
        store.replace(&[2u8; RECORD_SIZE]).unwrap();
        assert_eq!(loaded(&store), Some([2u8; RECORD_SIZE]));
        for name in &stale {
            assert!(!staging.join(name).exists(), "{} was not removed", name);
        }
        assert_eq!(fs::read_dir(&staging).unwrap().count(), SWEEP_LIMIT + 1);
        for name in decoy_names() {
            assert!(staging.join(&name).is_dir(), "{} was removed", name);
        }
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn remove_stale_candidates_removes_only_single_link_files_at_its_own_names() {
        let dir = scratch("candidates-kinds");
        drop(store_in(&dir));
        let staging = dir.join(STAGING_DIR);
        let record = [5u8; RECORD_SIZE];
        let names = next_names(&record);
        let victim = dir.join("victim");
        fs::write(&victim, b"keep").unwrap();
        fs::write(staging.join(&names[0]), b"stale").unwrap();
        fs::hard_link(&victim, staging.join(&names[1])).unwrap();
        symlink(&victim, staging.join(&names[2])).unwrap();
        fs::create_dir(staging.join(&names[3])).unwrap();
        for name in &names[4..] {
            fs::write(staging.join(name), b"stale").unwrap();
        }
        let other = staging_name(&[6u8; RECORD_SIZE], 0);
        fs::write(staging.join(&other), b"other").unwrap();
        let staging_dir = fs::File::open(&staging).unwrap();
        remove_stale_candidates(&staging_dir, &record, entry_stat, unlink_at).unwrap();
        assert!(fs::symlink_metadata(staging.join(&names[0])).is_err());
        for name in &names[4..] {
            assert!(
                fs::symlink_metadata(staging.join(name)).is_err(),
                "{name} was not removed"
            );
        }
        assert_eq!(fs::metadata(staging.join(&names[1])).unwrap().nlink(), 2);
        assert!(
            fs::symlink_metadata(staging.join(&names[2]))
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert!(staging.join(&names[3]).is_dir());
        assert_eq!(fs::read(staging.join(&other)).unwrap(), b"other");
        assert_eq!(fs::read(&victim).unwrap(), b"keep");
        fs::remove_dir_all(&dir).unwrap();
    }

    fn failing_unlink(_dir: &fs::File, _name: &CStr) -> io::Result<()> {
        return Err(io::Error::other("injected unlink failure"));
    }

    #[test]
    fn a_failed_look_up_or_removal_of_a_stale_candidate_is_an_error_and_keeps_the_candidates() {
        let dir = scratch("candidates-fail");
        drop(store_in(&dir));
        let staging = dir.join(STAGING_DIR);
        let record = [5u8; RECORD_SIZE];
        let names = next_names(&record);
        for name in &names {
            fs::write(staging.join(name), b"stale").unwrap();
        }
        let staging_dir = fs::File::open(&staging).unwrap();
        assert!(
            remove_stale_candidates(&staging_dir, &record, failing_staging_lookup, unlink_at)
                .is_err()
        );
        assert!(
            remove_stale_candidates(&staging_dir, &record, entry_stat, failing_unlink).is_err()
        );
        for name in &names {
            assert_eq!(fs::read(staging.join(name)).unwrap(), b"stale", "{name}");
        }
        remove_stale_candidates(&staging_dir, &record, entry_stat, unlink_at).unwrap();
        for name in &names {
            assert!(fs::symlink_metadata(staging.join(name)).is_err(), "{name}");
        }
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_second_link_under_another_records_name_is_left_alone_and_the_record_stays_refused() {
        let dir = scratch("names-unrelated-link");
        let mut store = store_in(&dir);
        store.replace(&[1u8; RECORD_SIZE]).unwrap();
        let unrelated = dir
            .join(STAGING_DIR)
            .join(staging_name(&[9u8; RECORD_SIZE], 0));
        fs::hard_link(dir.join(RECORD_NAME), &unrelated).unwrap();
        drop(store);
        let refused = store_in(&dir);
        let mut out = [0u8; RECORD_SIZE];
        assert_eq!(
            refused.load(&mut out).err().map(|e| e.code),
            Some(Code::checkpoint_invalid)
        );
        assert_eq!(fs::metadata(&unrelated).unwrap().nlink(), 2);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_later_store_resumes_its_record_past_the_staging_file_of_an_interrupted_replacement() {
        let dir = scratch("names-resume");
        {
            let mut store = store_in(&dir);
            store.replace(&[1u8; RECORD_SIZE]).unwrap();
            let debris = staging_name(&[2u8; RECORD_SIZE], 0);
            fs::write(dir.join(STAGING_DIR).join(debris), [2u8; RECORD_SIZE]).unwrap();
        }
        let mut resumed = store_in(&dir);
        assert_eq!(loaded(&resumed), Some([1u8; RECORD_SIZE]));
        resumed.replace(&[3u8; RECORD_SIZE]).unwrap();
        assert_eq!(loaded(&resumed), Some([3u8; RECORD_SIZE]));
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn only_the_form_this_store_makes_is_a_staging_name() {
        let made = staging_name(&[0x5au8; RECORD_SIZE], 7);
        let digest = sha256::hash_bytes(&[0x5au8; RECORD_SIZE]);
        let hex: String = digest[..8].iter().map(|b| format!("{:02x}", b)).collect();
        assert_eq!(made, format!("{}{}.7", STAGING_PREFIX, hex));
        assert!(is_staging_name(&made));
        for name in [
            ".ota-checkpoint.stage.0123456789abcdef",
            ".ota-checkpoint.stage.0123456789abcdef.",
            ".ota-checkpoint.stage.0123456789abcde.7",
            ".ota-checkpoint.stage.0123456789abcdeg.7",
            ".ota-checkpoint.stage.0123456789abcdef.7.tmp",
            ".ota-checkpoint.stage..7",
            ".ota-checkpoint.stage.1.0123456789abcdef.7",
            ".ota-checkpoint.probe.1.2.3",
            "ckpt",
        ] {
            assert!(
                !is_staging_name(name),
                "{} is taken for a staging file",
                name
            );
        }
    }

    thread_local! {
        static SYNCS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
        static SYNCS_FAIL: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    }

    fn counting_dir_sync(_dir: &fs::File) -> io::Result<()> {
        SYNCS.set(SYNCS.get() + 1);
        if SYNCS_FAIL.get() {
            return Err(io::Error::other("injected directory sync failure"));
        }
        return Ok(());
    }

    #[test]
    fn a_failed_directory_sync_after_the_removal_is_uncertain_and_the_record_is_gone() {
        let dir = scratch("clear-sync-fails");
        let mut writer = store_in(&dir);
        writer.replace(&[6u8; RECORD_SIZE]).unwrap();
        drop(writer);
        let mut store = store_with_sync(&dir, counting_dir_sync);
        SYNCS_FAIL.set(true);
        assert_eq!(
            store.clear().err().map(|e| e.code),
            Some(Code::checkpoint_removal_uncertain)
        );
        assert_eq!(loaded(&store), None);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_removal_that_fails_before_the_unlink_is_a_storage_error_and_keeps_the_entry() {
        let dir = scratch("clear-unlink-fails");
        fs::create_dir(dir.join(RECORD_NAME)).unwrap();
        let mut store = store_with_sync(&dir, counting_dir_sync);
        SYNCS_FAIL.set(true);
        assert_eq!(store.clear().err().map(|e| e.code), Some(Code::device_io));
        assert!(dir.join(RECORD_NAME).is_dir());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_checkpoint_clear_with_a_working_sync_removes_the_record_and_reports_nothing() {
        let dir = scratch("clear-ok");
        let mut store = store_in(&dir);
        store.replace(&[7u8; RECORD_SIZE]).unwrap();
        store.clear().unwrap();
        assert_eq!(loaded(&store), None);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_checkpoint_lock_is_shared_through_a_symbolic_link_to_the_directory_before_the_record_exists()
     {
        let dir = scratch("lock-alias");
        let ns = namespace(&dir);
        let alias = scratch("lock-alias-link");
        fs::remove_dir(&alias).unwrap();
        symlink(&dir, &alias).unwrap();
        let direct = dir.join(RECORD_NAME).to_str().unwrap().to_string();
        let through_link = alias.join(RECORD_NAME).to_str().unwrap().to_string();
        assert!(!dir.join(RECORD_NAME).exists());
        let held = FileCheckpoint::open(&ns, &direct).unwrap();
        let refused = FileCheckpoint::open(&ns, &through_link).err().unwrap();
        assert_eq!(
            refused,
            "another update holds the checkpoint ".to_string() + &through_link
        );
        drop(held);
        assert!(FileCheckpoint::open(&ns, &through_link).is_ok());
        fs::remove_file(&alias).unwrap();
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn two_checkpoint_names_in_one_directory_share_one_lock() {
        let dir = scratch("lock-names");
        let ns = namespace(&dir);
        let one = dir.join("one").to_str().unwrap().to_string();
        let two = dir.join("two").to_str().unwrap().to_string();
        let first = FileCheckpoint::open(&ns, &one).unwrap();
        assert_eq!(
            FileCheckpoint::open(&ns, &two).err().unwrap(),
            "another update holds the checkpoint ".to_string() + &two
        );
        drop(first);
        assert!(FileCheckpoint::open(&ns, &two).is_ok());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn case_variants_on_a_case_sensitive_directory_are_separate_records_that_one_lock_serialises() {
        let dir = scratch("case-variants");
        let ns = namespace(&dir);
        let lower_path = dir.join("ckpt").to_str().unwrap().to_string();
        let upper_path = dir.join("CKPT").to_str().unwrap().to_string();
        let lower = FileCheckpoint::open(&ns, &lower_path).unwrap();
        assert!(
            FileCheckpoint::open(&ns, &upper_path)
                .err()
                .unwrap()
                .contains("another update holds")
        );
        drop(lower);
        let mut upper = FileCheckpoint::open(&ns, &upper_path).unwrap();
        upper.replace(&[2u8; RECORD_SIZE]).unwrap();
        assert_eq!(loaded(&upper), Some([2u8; RECORD_SIZE]));
        assert!(dir.join("CKPT").exists());
        assert!(!dir.join("ckpt").exists());
        drop(upper);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_record_named_like_a_lock_file_contends_for_the_directory_lock_and_writes_nothing_while_it_is_held()
     {
        let dir = scratch("lock-collision");
        let ns = namespace(&dir);
        let state = dir.join("state").to_str().unwrap().to_string();
        let state_lock_named = dir.join("state.lock").to_str().unwrap().to_string();
        let held = FileCheckpoint::open(&ns, &state).unwrap();
        assert!(
            FileCheckpoint::open(&ns, &state_lock_named)
                .err()
                .unwrap()
                .contains("another update holds")
        );
        assert!(!dir.join("state.lock").exists());
        drop(held);
        let mut other = FileCheckpoint::open(&ns, &state_lock_named).unwrap();
        other.replace(&[1u8; RECORD_SIZE]).unwrap();
        assert_eq!(loaded(&other), Some([1u8; RECORD_SIZE]));
        drop(other);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_checkpoint_lock_lives_in_the_namespace_and_never_beside_the_record() {
        let dir = scratch("lock-placed");
        let ns = namespace(&dir);
        let record = dir.join(RECORD_NAME).to_str().unwrap().to_string();
        drop(FileCheckpoint::open(&ns, &record).unwrap());
        assert!(!dir.join(format!("{}.lock", RECORD_NAME)).exists());
        let names: Vec<String> = fs::read_dir(dir.join("lock-dir"))
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().to_string())
            .collect();
        assert_eq!(names.len(), 2, "{:?}", names);
        assert!(
            names
                .iter()
                .any(|n| n.starts_with("checkpoint-") && n.ends_with(".lock")),
            "{:?}",
            names
        );
        assert!(
            names
                .iter()
                .any(|n| n.starts_with("entry-") && n.ends_with(".lock")),
            "{:?}",
            names
        );
        let beside: Vec<String> = fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().to_string())
            .collect();
        assert_eq!(beside.len(), 2, "{:?}", beside);
        assert!(beside.contains(&"lock-dir".to_string()));
        assert!(beside.contains(&STAGING_DIR.to_string()));
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_checkpoint_record_in_the_lock_directory_is_refused() {
        let dir = scratch("lock-in-namespace");
        let ns = namespace(&dir);
        let inside = dir
            .join("lock-dir")
            .join("ck")
            .to_str()
            .unwrap()
            .to_string();
        assert!(
            FileCheckpoint::open(&ns, &inside)
                .err()
                .unwrap()
                .contains("names a file in the lock directory")
        );
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_checkpoint_path_without_a_file_name_or_a_missing_directory_is_refused() {
        let dir = scratch("lock-refused");
        let ns = namespace(&dir);
        assert!(FileCheckpoint::open(&ns, dir.join("..").to_str().unwrap()).is_err());
        assert!(
            FileCheckpoint::open(&ns, dir.join("missing").join("ck").to_str().unwrap()).is_err()
        );
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_checkpoint_name_that_is_not_plain_ascii_is_refused_before_any_lock_is_taken() {
        let dir = scratch("name-ascii");
        let ns = namespace(&dir);
        for name in ["caf\u{e9}", "ck pt", "ck\tpt"] {
            let path = dir.join(name).to_str().unwrap().to_string();
            assert!(
                FileCheckpoint::open(&ns, &path)
                    .err()
                    .unwrap()
                    .contains("must be printable ASCII"),
                "{:?}",
                name
            );
        }
        assert!(FileCheckpoint::open(&ns, &record_path(&dir)).is_ok());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_record_named_as_a_staging_file_is_refused_before_the_lock_and_no_staging_file_is_taken_over()
     {
        let dir = scratch("reserved-staging");
        let ns = namespace(&dir);
        let mut held = FileCheckpoint::open(&ns, &record_path(&dir)).unwrap();
        let planted = dir.join(staging_name(&[1u8; RECORD_SIZE], 0));
        fs::write(&planted, b"another store's staging file").unwrap();
        let named = planted.to_str().unwrap().to_string();
        let refused = FileCheckpoint::open(&ns, &named).err().unwrap();
        assert!(refused.contains("reserved"), "{}", refused);
        assert!(!refused.contains("another update holds"), "{}", refused);
        held.replace(&[1u8; RECORD_SIZE]).unwrap();
        assert_eq!(loaded(&held), Some([1u8; RECORD_SIZE]));
        assert_eq!(fs::read(&planted).unwrap(), b"another store's staging file");
        drop(held);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_record_named_as_a_case_probe_is_refused_and_a_probe_in_flight_is_left_alone() {
        let dir = scratch("reserved-probe");
        let ns = namespace(&dir);
        let planted = dir.join(probe_name(std::process::id(), 7, 0));
        fs::write(&planted, b"probe in flight").unwrap();
        let named = planted.to_str().unwrap().to_string();
        assert!(
            FileCheckpoint::open(&ns, &named)
                .err()
                .unwrap()
                .contains("reserved")
        );
        let mut store = FileCheckpoint::open(&ns, &record_path(&dir)).unwrap();
        store.replace(&[2u8; RECORD_SIZE]).unwrap();
        assert_eq!(loaded(&store), Some([2u8; RECORD_SIZE]));
        assert_eq!(fs::read(&planted).unwrap(), b"probe in flight");
        drop(store);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_reserved_prefix_is_refused_in_any_ascii_case_and_only_as_a_whole_prefix() {
        let dir = scratch("reserved-case");
        let ns = namespace(&dir);
        for name in [
            ".ota-checkpoint.stage.1.0",
            ".OTA-CHECKPOINT.x",
            ".Ota-Checkpoint.probe.1.2.3",
        ] {
            let path = dir.join(name).to_str().unwrap().to_string();
            assert!(
                FileCheckpoint::open(&ns, &path)
                    .err()
                    .unwrap()
                    .contains("reserved"),
                "{}",
                name
            );
        }
        for name in ["ota-checkpoint.x", ".ota-checkpoints", ".ota-checkpoin"] {
            assert!(
                FileCheckpoint::open(&ns, dir.join(name).to_str().unwrap()).is_ok(),
                "{}",
                name
            );
        }
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_probe_that_cannot_run_refuses_the_store_and_releases_its_lock() {
        fn unable(_dir: &fs::File) -> Result<(), String> {
            return Err(
                "cannot establish that the checkpoint directory is case-sensitive: injected"
                    .to_string(),
            );
        }
        let dir = scratch("probe-unable");
        let ns = namespace(&dir);
        let path = record_path(&dir);
        assert!(
            FileCheckpoint::open_with(&ns, &path, sync_dir, unable, sweep_window)
                .err()
                .unwrap()
                .contains("cannot establish")
        );
        assert!(FileCheckpoint::open(&ns, &path).is_ok());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_failed_look_up_after_an_absent_load_is_uncertain_and_confirms_nothing() {
        fn failing_lookup(_dir: &fs::File, _name: &CStr) -> io::Result<bool> {
            return Err(io::Error::other("injected look-up failure"));
        }
        let dir = scratch("confirm-lookup");
        let mut store = store_in(&dir);
        assert_eq!(loaded(&store), None);
        let err = store.confirm_absent_with(failing_lookup).err().unwrap();
        assert_eq!(err.code, Code::checkpoint_removal_uncertain);
        assert!(err.msg.contains("not confirmed"), "{}", err.msg);
        assert!(!err.msg.contains("present"), "{}", err.msg);
        assert_eq!(loaded(&store), None);
        store.confirm_absent().unwrap();
        assert_eq!(loaded(&store), None);
        assert!(!dir.join(RECORD_NAME).exists());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_entry_identity_is_the_records_or_none_and_a_link_at_the_name_is_its_own_entry() {
        let dir = scratch("entry-identity");
        let mut store = store_in(&dir);
        assert_eq!(store.entry_identity().unwrap(), None);
        store.replace(&[1u8; RECORD_SIZE]).unwrap();
        let meta = fs::metadata(dir.join(RECORD_NAME)).unwrap();
        assert_eq!(
            store.entry_identity().unwrap(),
            Some(Identity::File {
                dev: meta.dev(),
                ino: meta.ino()
            })
        );
        fs::remove_file(dir.join(RECORD_NAME)).unwrap();
        symlink(dir.join("missing"), dir.join(RECORD_NAME)).unwrap();
        let link = fs::symlink_metadata(dir.join(RECORD_NAME)).unwrap();
        assert_eq!(
            store.entry_identity().unwrap(),
            Some(Identity::File {
                dev: link.dev(),
                ino: link.ino()
            })
        );
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_folding_directory_is_refused_whether_or_not_the_record_exists_and_releases_its_lock() {
        fn folds(_dir: &fs::File) -> Result<(), String> {
            return Err("the checkpoint directory folds the case of ASCII names".to_string());
        }
        let dir = scratch("folding");
        let ns = namespace(&dir);
        let path = record_path(&dir);
        assert!(
            FileCheckpoint::open_with(&ns, &path, sync_dir, folds, sweep_window)
                .err()
                .unwrap()
                .contains("folds")
        );
        assert!(FileCheckpoint::open(&ns, &path).is_ok());
        fs::write(dir.join(RECORD_NAME), [8u8; RECORD_SIZE]).unwrap();
        assert!(
            FileCheckpoint::open_with(&ns, &path, sync_dir, folds, sweep_window)
                .err()
                .unwrap()
                .contains("folds")
        );
        assert_eq!(
            fs::read(dir.join(RECORD_NAME)).unwrap(),
            vec![8u8; RECORD_SIZE]
        );
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_case_probe_of_a_case_sensitive_directory_passes_and_leaves_no_probe_behind() {
        let dir = scratch("probe-ok");
        let handle = fs::File::open(&dir).unwrap();
        assert!(ensure_case_sensitive(&handle).is_ok());
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 0);
        assert!(FileCheckpoint::open(&namespace(&dir), &record_path(&dir)).is_ok());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_parent_symlink_retargeted_after_the_lock_leaves_the_run_on_its_own_directory() {
        let root = scratch("retarget");
        let real = root.join("real");
        let other = root.join("other");
        fs::create_dir(&real).unwrap();
        fs::create_dir(&other).unwrap();
        let link = root.join("link");
        symlink(&real, &link).unwrap();
        let ns = namespace(&root);
        let through_link = link.join(RECORD_NAME).to_str().unwrap().to_string();
        let mut store = FileCheckpoint::open(&ns, &through_link).unwrap();
        fs::remove_file(&link).unwrap();
        symlink(&other, &link).unwrap();
        store.replace(&[1u8; RECORD_SIZE]).unwrap();
        assert_eq!(
            fs::read(real.join(RECORD_NAME)).unwrap(),
            [1u8; RECORD_SIZE].to_vec()
        );
        assert!(!other.join(RECORD_NAME).exists());
        assert_eq!(loaded(&store), Some([1u8; RECORD_SIZE]));
        let mut elsewhere = FileCheckpoint::open(&ns, &through_link).unwrap();
        assert_eq!(loaded(&elsewhere), None);
        elsewhere.replace(&[2u8; RECORD_SIZE]).unwrap();
        assert_eq!(
            fs::read(other.join(RECORD_NAME)).unwrap(),
            [2u8; RECORD_SIZE].to_vec()
        );
        assert_eq!(
            fs::read(real.join(RECORD_NAME)).unwrap(),
            [1u8; RECORD_SIZE].to_vec()
        );
        let direct = real.join(RECORD_NAME).to_str().unwrap().to_string();
        assert_eq!(
            FileCheckpoint::open(&ns, &direct).err().unwrap(),
            "another update holds the checkpoint ".to_string() + &direct
        );
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_failed_sync_when_no_record_remains_is_uncertain_until_a_later_sync_confirms_the_absence() {
        let dir = scratch("confirm-absent");
        let mut writer = store_in(&dir);
        writer.replace(&[8u8; RECORD_SIZE]).unwrap();
        drop(writer);
        let mut store = store_with_sync(&dir, counting_dir_sync);
        SYNCS_FAIL.set(true);
        assert_eq!(
            store.clear().err().map(|e| e.code),
            Some(Code::checkpoint_removal_uncertain)
        );
        assert_eq!(loaded(&store), None);
        assert_eq!(
            store.confirm_absent().err().map(|e| e.code),
            Some(Code::checkpoint_removal_uncertain)
        );
        assert_eq!(loaded(&store), None);
        drop(store);
        let mut retry = store_in(&dir);
        assert!(retry.confirm_absent().is_ok());
        assert_eq!(loaded(&retry), None);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_case_folded_directory_refuses_the_record_before_and_after_it_exists() {
        let Ok(folded) = std::env::var("OTA_TEST_CASEFOLD_DIR") else {
            eprintln!(
                "skipped: OTA_TEST_CASEFOLD_DIR is not set; the case-folded check is platform validation"
            );
            return;
        };
        let folded = PathBuf::from(folded);
        let work = scratch("casefold-env");
        let ns = namespace(&work);
        let record = folded.join(RECORD_NAME);
        let upper = folded.join("CKPT");
        let refused = |p: &Path| {
            FileCheckpoint::open(&ns, p.to_str().unwrap())
                .err()
                .unwrap()
        };
        assert!(refused(&record).contains("folds"));
        assert!(refused(&upper).contains("folds"));
        fs::write(&record, [4u8; RECORD_SIZE]).unwrap();
        assert!(refused(&record).contains("folds"));
        assert!(refused(&upper).contains("folds"));
        fs::remove_file(&record).unwrap();
        fs::remove_dir_all(&work).unwrap();
    }

    #[test]
    fn confirm_absent_refuses_a_stored_record_and_removes_nothing() {
        let dir = scratch("confirm-present");
        let mut store = store_in(&dir);
        store.replace(&[9u8; RECORD_SIZE]).unwrap();
        assert_eq!(
            store.confirm_absent().err().map(|e| e.code),
            Some(Code::checkpoint_present)
        );
        assert_eq!(loaded(&store), Some([9u8; RECORD_SIZE]));
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn confirm_absent_refuses_a_dangling_link_at_the_record_name_and_removes_nothing() {
        let dir = scratch("confirm-link");
        symlink(dir.join("missing"), dir.join(RECORD_NAME)).unwrap();
        let mut store = store_in(&dir);
        assert_eq!(
            store.confirm_absent().err().map(|e| e.code),
            Some(Code::checkpoint_present)
        );
        assert!(fs::symlink_metadata(dir.join(RECORD_NAME)).is_ok());
        fs::remove_dir_all(&dir).unwrap();
    }

    thread_local! {
        static FAILING_INODE: std::cell::Cell<Option<u64>> = const { std::cell::Cell::new(None) };
    }

    fn sync_failing_for_inode(dir: &fs::File) -> io::Result<()> {
        if FAILING_INODE.get() == Some(dir.metadata()?.ino()) {
            return Err(io::Error::other("injected directory sync failure"));
        }
        return Ok(());
    }

    fn record_bytes(dir: &Path) -> Vec<u8> {
        return fs::read(dir.join(RECORD_NAME)).unwrap();
    }

    #[test]
    fn a_renamed_record_whose_directory_sync_failed_is_visible_but_refused_until_a_confirmation_succeeds()
     {
        let dir = scratch("confirm-first-record");
        let mut store = FileCheckpoint::open_with_dir_sync(
            &namespace(&dir),
            &record_path(&dir),
            counting_dir_sync,
        )
        .unwrap();
        SYNCS_FAIL.set(true);
        assert_eq!(
            store.replace(&[1; RECORD_SIZE]).err().map(|e| e.code),
            Some(Code::device_io)
        );
        assert_eq!(loaded(&store), Some([1; RECORD_SIZE]));
        for _ in 0..2 {
            assert_eq!(
                store.confirm_stored().err().map(|e| e.code),
                Some(Code::checkpoint_unconfirmed)
            );
        }
        assert_eq!(record_bytes(&dir), vec![1; RECORD_SIZE]);
        drop(store);
        let mut retry = store_in(&dir);
        retry.confirm_stored().unwrap();
        assert_eq!(loaded(&retry), Some([1; RECORD_SIZE]));
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_later_record_whose_directory_sync_failed_is_refused_by_the_confirmation_until_a_sync_works()
     {
        let dir = scratch("confirm-later-record");
        let mut store = store_in(&dir);
        store.replace(&[1; RECORD_SIZE]).unwrap();
        drop(store);
        let mut failing = FileCheckpoint::open_with_dir_sync(
            &namespace(&dir),
            &record_path(&dir),
            counting_dir_sync,
        )
        .unwrap();
        SYNCS_FAIL.set(true);
        assert!(failing.replace(&[2; RECORD_SIZE]).is_err());
        assert_eq!(loaded(&failing), Some([2; RECORD_SIZE]));
        assert_eq!(
            failing.confirm_stored().err().map(|e| e.code),
            Some(Code::checkpoint_unconfirmed)
        );
        drop(failing);
        let mut retry = store_in(&dir);
        retry.confirm_stored().unwrap();
        assert_eq!(loaded(&retry), Some([2; RECORD_SIZE]));
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_failed_staging_directory_sync_leaves_the_record_unconfirmed_as_well() {
        let dir = scratch("confirm-staging-sync");
        let ns = namespace(&dir);
        let mut store =
            FileCheckpoint::open_with_dir_sync(&ns, &record_path(&dir), sync_failing_for_inode)
                .unwrap();
        FAILING_INODE.set(Some(fs::metadata(dir.join(STAGING_DIR)).unwrap().ino()));
        assert!(store.replace(&[3; RECORD_SIZE]).is_err());
        assert_eq!(loaded(&store), Some([3; RECORD_SIZE]));
        assert_eq!(
            store.confirm_stored().err().map(|e| e.code),
            Some(Code::checkpoint_unconfirmed)
        );
        FAILING_INODE.set(None);
        store.confirm_stored().unwrap();
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_crash_that_keeps_an_unconfirmed_rename_resumes_the_new_record_once_it_is_confirmed() {
        let dir = scratch("crash-rename-kept");
        let mut store = store_in(&dir);
        store.replace(&[1; RECORD_SIZE]).unwrap();
        drop(store);
        let mut failing = FileCheckpoint::open_with_dir_sync(
            &namespace(&dir),
            &record_path(&dir),
            counting_dir_sync,
        )
        .unwrap();
        SYNCS_FAIL.set(true);
        assert!(failing.replace(&[2; RECORD_SIZE]).is_err());
        drop(failing);
        let mut resumed = store_in(&dir);
        resumed.confirm_stored().unwrap();
        assert_eq!(loaded(&resumed), Some([2; RECORD_SIZE]));
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_crash_that_loses_an_unconfirmed_rename_leaves_the_previous_record_which_confirms_and_resumes()
     {
        let dir = scratch("crash-rename-lost");
        let mut store = store_in(&dir);
        store.replace(&[1; RECORD_SIZE]).unwrap();
        drop(store);
        let mut failing = FileCheckpoint::open_with_dir_sync(
            &namespace(&dir),
            &record_path(&dir),
            counting_dir_sync,
        )
        .unwrap();
        SYNCS_FAIL.set(true);
        assert!(failing.replace(&[2; RECORD_SIZE]).is_err());
        drop(failing);
        fs::write(dir.join(RECORD_NAME), [1; RECORD_SIZE]).unwrap();
        let mut resumed = store_in(&dir);
        resumed.confirm_stored().unwrap();
        assert_eq!(loaded(&resumed), Some([1; RECORD_SIZE]));
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_second_link_to_the_record_left_by_an_unfinished_publication_is_removed_on_open() {
        let dir = scratch("second-link-swept");
        let mut store = store_in(&dir);
        store.replace(&[1; RECORD_SIZE]).unwrap();
        let staged = dir
            .join(STAGING_DIR)
            .join(staging_name(&[1; RECORD_SIZE], 0));
        fs::hard_link(dir.join(RECORD_NAME), &staged).unwrap();
        drop(store);
        let resumed = store_in(&dir);
        assert!(!staged.exists());
        assert_eq!(fs::metadata(dir.join(RECORD_NAME)).unwrap().nlink(), 1);
        assert_eq!(loaded(&resumed), Some([1; RECORD_SIZE]));
        fs::remove_dir_all(&dir).unwrap();
    }

    const STEPS: [InitStep; 7] = [
        InitStep::Created,
        InitStep::MarkerCreated,
        InitStep::MarkerWritten,
        InitStep::MarkerSynced,
        InitStep::InitSynced,
        InitStep::Published,
        InitStep::PublishSynced,
    ];

    fn published_marker_is_complete(dir: &Path) -> bool {
        let staging = dir.join(STAGING_DIR);
        if !staging.exists() {
            return true;
        }
        return fs::read(staging.join(STAGING_MARKER)).unwrap() == MARKER_CONTENT;
    }

    #[test]
    fn an_initialization_interrupted_at_any_step_is_completed_by_the_next_open_and_never_published_without_its_marker()
     {
        for first in STEPS {
            for second in STEPS {
                let dir = scratch(&format!("init-{:?}-{:?}", first, second));
                let parent = fs::File::open(&dir).unwrap();
                let fail_at = |step: InitStep| {
                    if step == first {
                        Err(io::Error::other("injected failure"))
                    } else {
                        Ok(())
                    }
                };
                assert!(
                    open_staging_dir_with(&parent, effective_uid(), fail_at).is_err(),
                    "{:?}",
                    first
                );
                assert!(published_marker_is_complete(&dir), "{:?}", first);
                let fail_again = |step: InitStep| {
                    if step == second {
                        Err(io::Error::other("injected failure"))
                    } else {
                        Ok(())
                    }
                };
                let _ = open_staging_dir_with(&parent, effective_uid(), fail_again);
                assert!(
                    published_marker_is_complete(&dir),
                    "{:?} then {:?}",
                    first,
                    second
                );
                open_staging_dir(&parent).unwrap();
                assert!(
                    !dir.join(STAGING_INIT).exists(),
                    "{:?} then {:?}",
                    first,
                    second
                );
                assert_eq!(
                    fs::read(dir.join(STAGING_DIR).join(STAGING_MARKER)).unwrap(),
                    MARKER_CONTENT
                );
                assert_eq!(
                    fs::metadata(dir.join(STAGING_DIR)).unwrap().mode() & 0o777,
                    0o700
                );
                fs::remove_dir_all(&dir).unwrap();
            }
        }
    }

    #[test]
    fn a_torn_marker_in_an_abandoned_initialization_is_discarded_and_made_again() {
        let mut partly_persisted = MARKER_CONTENT[..10].to_vec();
        partly_persisted.resize(MARKER_CONTENT.len(), 0);
        let cases: [Vec<u8>; 6] = [
            Vec::new(),
            MARKER_CONTENT[..5].to_vec(),
            MARKER_CONTENT[..MARKER_CONTENT.len() - 1].to_vec(),
            vec![0; MARKER_CONTENT.len()],
            partly_persisted,
            b"stale bytes, not ours\n".to_vec(),
        ];
        for (i, content) in cases.iter().enumerate() {
            let dir = scratch(&format!("torn-marker-{i}"));
            let init = dir.join(STAGING_INIT);
            fs::create_dir(&init).unwrap();
            fs::set_permissions(&init, fs::Permissions::from_mode(0o700)).unwrap();
            fs::write(init.join(STAGING_MARKER), content).unwrap();
            fs::set_permissions(init.join(STAGING_MARKER), fs::Permissions::from_mode(0o600))
                .unwrap();
            drop(store_in(&dir));
            assert!(!init.exists(), "case {i}");
            let published = dir.join(STAGING_DIR).join(STAGING_MARKER);
            assert_eq!(fs::read(&published).unwrap(), MARKER_CONTENT, "case {i}");
            assert_eq!(
                fs::metadata(&published).unwrap().mode() & 0o777,
                0o600,
                "case {i}"
            );
            fs::remove_dir_all(&dir).unwrap();
        }
    }

    #[test]
    fn a_marker_no_longer_than_its_contents_is_made_again_but_a_longer_one_is_refused() {
        let dir = scratch("marker-longer");
        let init = dir.join(STAGING_INIT);
        fs::create_dir(&init).unwrap();
        fs::set_permissions(&init, fs::Permissions::from_mode(0o700)).unwrap();
        let marker = init.join(STAGING_MARKER);
        let mut longer = MARKER_CONTENT.to_vec();
        longer.push(b'x');
        fs::write(&marker, &longer).unwrap();
        fs::set_permissions(&marker, fs::Permissions::from_mode(0o600)).unwrap();
        let parent = fs::File::open(&dir).unwrap();
        assert!(open_staging_dir(&parent).is_err());
        assert_eq!(fs::read(&marker).unwrap(), longer);
        assert!(!dir.join(STAGING_DIR).exists());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn an_initialization_directory_that_is_not_its_own_shape_is_refused_and_untouched() {
        for kind in [
            "extra-entry",
            "public-mode",
            "marker-symlink",
            "marker-second-link",
            "marker-directory",
            "marker-fifo",
            "file-not-directory",
            "symlink-directory",
        ] {
            let dir = scratch(&format!("init-refuse-{kind}"));
            let victim = dir.join("victim");
            fs::write(&victim, MARKER_CONTENT).unwrap();
            fs::set_permissions(&victim, fs::Permissions::from_mode(0o400)).unwrap();
            let init = dir.join(STAGING_INIT);
            let marker = init.join(STAGING_MARKER);
            match kind {
                "file-not-directory" => fs::write(&init, b"keep").unwrap(),
                "symlink-directory" => {
                    let elsewhere = dir.join("elsewhere");
                    fs::create_dir(&elsewhere).unwrap();
                    symlink(&elsewhere, &init).unwrap();
                }
                _ => {
                    fs::create_dir(&init).unwrap();
                    fs::set_permissions(
                        &init,
                        fs::Permissions::from_mode(if kind == "public-mode" {
                            0o755
                        } else {
                            0o700
                        }),
                    )
                    .unwrap();
                    match kind {
                        "extra-entry" => {
                            fs::write(init.join("stray"), b"keep").unwrap();
                            fs::write(&marker, MARKER_CONTENT).unwrap();
                            fs::set_permissions(&marker, fs::Permissions::from_mode(0o600))
                                .unwrap();
                        }
                        "marker-symlink" => symlink(&victim, &marker).unwrap(),
                        "marker-second-link" => fs::hard_link(&victim, &marker).unwrap(),
                        "marker-directory" => fs::create_dir(&marker).unwrap(),
                        "marker-fifo" => {
                            let path =
                                std::ffi::CString::new(marker.as_os_str().as_bytes()).unwrap();
                            assert_eq!(unsafe { libc::mkfifo(path.as_ptr(), 0o600) }, 0);
                        }
                        _ => {}
                    }
                }
            }
            let parent = fs::File::open(&dir).unwrap();
            assert!(open_staging_dir(&parent).is_err(), "{kind}");
            assert!(!dir.join(STAGING_DIR).exists(), "{kind}");
            assert_eq!(fs::read(&victim).unwrap(), MARKER_CONTENT, "{kind}");
            assert_eq!(mode_of(&victim), 0o400, "{kind}");
            if kind == "public-mode" {
                assert_eq!(mode_of(&init), 0o755, "{kind}");
            }
            if kind == "extra-entry" {
                assert_eq!(fs::read(init.join("stray")).unwrap(), b"keep", "{kind}");
            }
            fs::remove_dir_all(&dir).unwrap();
        }
    }

    fn mode_of(path: &Path) -> u32 {
        return fs::symlink_metadata(path).unwrap().mode() & 0o777;
    }

    #[test]
    fn a_published_staging_directory_wins_and_a_leftover_initialization_directory_beside_it_is_left_alone()
     {
        let dir = scratch("init-beside-published");
        drop(store_in(&dir));
        let init = dir.join(STAGING_INIT);
        fs::create_dir(&init).unwrap();
        fs::write(init.join("stray"), b"left alone").unwrap();
        let mut store = store_in(&dir);
        store.replace(&[5; RECORD_SIZE]).unwrap();
        assert_eq!(loaded(&store), Some([5; RECORD_SIZE]));
        assert_eq!(fs::read(init.join("stray")).unwrap(), b"left alone");
        fs::remove_dir_all(&dir).unwrap();
    }

    const DIR_MODES: [u32; 3] = [0o500, 0o200, 0o000];
    const MARKER_MODES: [u32; 3] = [0o400, 0o200, 0o000];

    #[test]
    fn an_initialization_left_with_umask_modes_is_completed_by_the_next_open_at_every_step() {
        for dir_mode in DIR_MODES {
            for marker_mode in MARKER_MODES {
                for step in STEPS {
                    let dir = scratch(&format!("umask-{dir_mode:o}-{marker_mode:o}-{step:?}"));
                    let parent = fs::File::open(&dir).unwrap();
                    let fail_at = |s: InitStep| {
                        if s == step {
                            Err(io::Error::other("injected failure"))
                        } else {
                            Ok(())
                        }
                    };
                    assert!(
                        open_staging_dir_with(&parent, effective_uid(), fail_at).is_err(),
                        "{:?}",
                        step
                    );
                    let init = dir.join(STAGING_INIT);
                    if init.exists() {
                        let marker = init.join(STAGING_MARKER);
                        if marker.exists() {
                            fs::set_permissions(&marker, fs::Permissions::from_mode(marker_mode))
                                .unwrap();
                        }
                        fs::set_permissions(&init, fs::Permissions::from_mode(dir_mode)).unwrap();
                    }
                    open_staging_dir(&parent)
                        .unwrap_or_else(|e| panic!("{dir_mode:o} {marker_mode:o} {step:?}: {e}"));
                    let staging = dir.join(STAGING_DIR);
                    assert_eq!(
                        mode_of(&staging),
                        0o700,
                        "{dir_mode:o} {marker_mode:o} {step:?}"
                    );
                    assert_eq!(
                        mode_of(&staging.join(STAGING_MARKER)),
                        0o600,
                        "{dir_mode:o} {marker_mode:o} {step:?}"
                    );
                    assert!(!init.exists(), "{dir_mode:o} {marker_mode:o} {step:?}");
                    fs::remove_dir_all(&dir).unwrap();
                }
            }
        }
    }

    #[test]
    fn a_marker_or_directory_of_another_owner_is_refused_and_its_modes_are_left_as_they_are() {
        let dir = scratch("init-foreign-owner");
        let init = dir.join(STAGING_INIT);
        fs::create_dir(&init).unwrap();
        fs::set_permissions(&init, fs::Permissions::from_mode(0o500)).unwrap();
        let parent = fs::File::open(&dir).unwrap();
        let foreign = effective_uid().wrapping_add(1);
        assert!(open_init_dir(&parent, &c_string(STAGING_INIT).unwrap(), foreign).is_err());
        assert_eq!(mode_of(&init), 0o500);
        fs::set_permissions(&init, fs::Permissions::from_mode(0o700)).unwrap();
        let marker = init.join(STAGING_MARKER);
        fs::write(&marker, b"ota-checkpoint staging").unwrap();
        fs::set_permissions(&marker, fs::Permissions::from_mode(0o400)).unwrap();
        let init_dir = fs::File::open(&init).unwrap();
        assert!(complete_marker(&init_dir, foreign, &|_| Ok(())).is_err());
        assert_eq!(mode_of(&marker), 0o400);
        assert_eq!(fs::read(&marker).unwrap(), b"ota-checkpoint staging");
        fs::remove_dir_all(&dir).unwrap();
    }

    fn init_with_unrelated_entry(dir: &Path, mode: u32) -> PathBuf {
        let init = dir.join(STAGING_INIT);
        fs::create_dir(&init).unwrap();
        fs::write(init.join("stray"), b"keep").unwrap();
        fs::set_permissions(&init, fs::Permissions::from_mode(mode)).unwrap();
        return init;
    }

    #[test]
    fn an_initialization_directory_the_owner_can_read_is_refused_with_its_mode_and_entries_kept() {
        for mode in [0o700, 0o500, 0o600, 0o400] {
            let dir = scratch(&format!("init-readable-{mode:o}"));
            let init = init_with_unrelated_entry(&dir, mode);
            let parent = fs::File::open(&dir).unwrap();
            let err = open_staging_dir(&parent).err().unwrap();
            assert!(
                err.to_string().contains("other than its ownership marker"),
                "{mode:o}: {err}"
            );
            assert!(
                err.to_string().contains("move them aside"),
                "{mode:o}: {err}"
            );
            assert!(err.to_string().contains("chmod u+wx"), "{mode:o}: {err}");
            assert!(!err.to_string().contains("remove"), "{mode:o}: {err}");
            assert_eq!(mode_of(&init), mode, "{mode:o}");
            assert!(!dir.join(STAGING_DIR).exists(), "{mode:o}");
            fs::set_permissions(&init, fs::Permissions::from_mode(0o700)).unwrap();
            assert_eq!(fs::read(init.join("stray")).unwrap(), b"keep", "{mode:o}");
            fs::remove_dir_all(&dir).unwrap();
        }
    }

    #[test]
    fn an_initialization_directory_the_owner_cannot_read_has_its_owner_bits_restored_before_the_refusal()
     {
        for mode in [0o300, 0o200, 0o100, 0o000] {
            let dir = scratch(&format!("init-unreadable-{mode:o}"));
            let init = init_with_unrelated_entry(&dir, mode);
            let parent = fs::File::open(&dir).unwrap();
            let err = open_staging_dir(&parent).err().unwrap();
            assert!(
                err.to_string()
                    .contains("its owner permissions were restored"),
                "{mode:o}: {err}"
            );
            assert!(
                err.to_string().contains("does not establish whether"),
                "{mode:o}: {err}"
            );
            assert_eq!(mode_of(&init), 0o700, "{mode:o}");
            assert!(!dir.join(STAGING_DIR).exists(), "{mode:o}");
            assert_eq!(fs::read(init.join("stray")).unwrap(), b"keep", "{mode:o}");
            fs::remove_dir_all(&dir).unwrap();
        }
    }

    #[test]
    fn a_marker_that_an_interrupted_umask_left_unreadable_is_restored_and_completed() {
        let dir = scratch("marker-unreadable");
        let init = dir.join(STAGING_INIT);
        fs::create_dir(&init).unwrap();
        fs::set_permissions(&init, fs::Permissions::from_mode(0o700)).unwrap();
        let marker = init.join(STAGING_MARKER);
        fs::write(&marker, MARKER_CONTENT).unwrap();
        fs::set_permissions(&marker, fs::Permissions::from_mode(0o000)).unwrap();
        drop(store_in(&dir));
        assert_eq!(
            fs::read(dir.join(STAGING_DIR).join(STAGING_MARKER)).unwrap(),
            MARKER_CONTENT
        );
        assert!(!init.exists());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn an_initialization_directory_the_owner_can_search_keeps_its_mode_when_its_sole_marker_is_refused()
     {
        for kind in ["longer", "second-link", "symlink", "public"] {
            let dir = scratch(&format!("init-searchable-{kind}"));
            let victim = dir.join("victim");
            fs::write(&victim, MARKER_CONTENT).unwrap();
            fs::set_permissions(&victim, fs::Permissions::from_mode(0o400)).unwrap();
            let init = dir.join(STAGING_INIT);
            fs::create_dir(&init).unwrap();
            let marker = init.join(STAGING_MARKER);
            match kind {
                "longer" => {
                    let mut longer = MARKER_CONTENT.to_vec();
                    longer.push(b'x');
                    fs::write(&marker, &longer).unwrap();
                    fs::set_permissions(&marker, fs::Permissions::from_mode(0o600)).unwrap();
                }
                "second-link" => fs::hard_link(&victim, &marker).unwrap(),
                "symlink" => symlink(&victim, &marker).unwrap(),
                _ => {
                    fs::write(&marker, MARKER_CONTENT).unwrap();
                    fs::set_permissions(&marker, fs::Permissions::from_mode(0o644)).unwrap();
                }
            }
            fs::set_permissions(&init, fs::Permissions::from_mode(0o500)).unwrap();
            let parent = fs::File::open(&dir).unwrap();
            let err = open_staging_dir(&parent).err().unwrap();
            assert!(
                err.to_string()
                    .contains("staging initialization marker must be"),
                "{kind}: {err}"
            );
            assert_eq!(mode_of(&init), 0o500, "{kind}");
            assert!(!dir.join(STAGING_DIR).exists(), "{kind}");
            assert_eq!(mode_of(&victim), 0o400, "{kind}");
            assert_eq!(fs::read(&victim).unwrap(), MARKER_CONTENT, "{kind}");
            fs::set_permissions(&init, fs::Permissions::from_mode(0o700)).unwrap();
            fs::remove_dir_all(&dir).unwrap();
        }
    }

    #[test]
    fn a_torn_marker_in_an_initialization_directory_the_owner_can_search_is_still_repaired() {
        let dir = scratch("init-searchable-torn");
        let init = dir.join(STAGING_INIT);
        fs::create_dir(&init).unwrap();
        fs::write(init.join(STAGING_MARKER), &MARKER_CONTENT[..5]).unwrap();
        fs::set_permissions(init.join(STAGING_MARKER), fs::Permissions::from_mode(0o600)).unwrap();
        fs::set_permissions(&init, fs::Permissions::from_mode(0o500)).unwrap();
        open_staging_dir(&fs::File::open(&dir).unwrap()).unwrap();
        assert!(!init.exists());
        assert_eq!(
            fs::read(dir.join(STAGING_DIR).join(STAGING_MARKER)).unwrap(),
            MARKER_CONTENT
        );
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_directory_the_owner_can_read_but_not_search_has_its_owner_bits_restored_before_its_marker_is_checked()
     {
        for mode in [0o400, 0o600] {
            let dir = scratch(&format!("init-readable-unsearchable-{mode:o}"));
            let init = dir.join(STAGING_INIT);
            fs::create_dir(&init).unwrap();
            let marker = init.join(STAGING_MARKER);
            let mut longer = MARKER_CONTENT.to_vec();
            longer.push(b'x');
            fs::write(&marker, &longer).unwrap();
            fs::set_permissions(&marker, fs::Permissions::from_mode(0o600)).unwrap();
            fs::set_permissions(&init, fs::Permissions::from_mode(mode)).unwrap();
            let parent = fs::File::open(&dir).unwrap();
            let err = open_staging_dir(&parent).err().unwrap();
            assert!(
                err.to_string()
                    .contains("its owner permissions were restored"),
                "{mode:o}: {err}"
            );
            assert!(
                err.to_string().contains("does not establish whether"),
                "{mode:o}: {err}"
            );
            assert_eq!(mode_of(&init), 0o700, "{mode:o}");
            assert_eq!(fs::read(&marker).unwrap(), longer, "{mode:o}");
            assert!(!dir.join(STAGING_DIR).exists(), "{mode:o}");
            fs::remove_dir_all(&dir).unwrap();
        }
    }

    #[test]
    fn a_refused_marker_in_a_directory_the_owner_cannot_read_restores_its_mode_and_the_refusal_names_read_and_search()
     {
        for mode in [0o300, 0o200, 0o100, 0o000] {
            let dir = scratch(&format!("init-unreadable-marker-{mode:o}"));
            let init = dir.join(STAGING_INIT);
            fs::create_dir(&init).unwrap();
            let marker = init.join(STAGING_MARKER);
            let mut longer = MARKER_CONTENT.to_vec();
            longer.push(b'x');
            fs::write(&marker, &longer).unwrap();
            fs::set_permissions(&marker, fs::Permissions::from_mode(0o600)).unwrap();
            fs::set_permissions(&init, fs::Permissions::from_mode(mode)).unwrap();
            let parent = fs::File::open(&dir).unwrap();
            let err = open_staging_dir(&parent).err().unwrap();
            assert!(
                err.to_string().contains("could not read or search"),
                "{mode:o}: {err}"
            );
            assert!(
                err.to_string().contains("does not establish whether"),
                "{mode:o}: {err}"
            );
            assert!(
                err.to_string()
                    .contains("staging initialization marker must be"),
                "{mode:o}: {err}"
            );
            assert_eq!(mode_of(&init), 0o700, "{mode:o}");
            assert_eq!(fs::read(&marker).unwrap(), longer, "{mode:o}");
            assert!(!dir.join(STAGING_DIR).exists(), "{mode:o}");
            fs::remove_dir_all(&dir).unwrap();
        }
    }

    #[test]
    fn an_initialization_directory_the_owner_cannot_search_is_moved_aside_whole_and_the_next_open_starts_afresh()
     {
        let dir = scratch("init-whole-move-aside");
        let init = init_with_unrelated_entry(&dir, 0o400);
        let parent = fs::File::open(&dir).unwrap();
        assert!(open_staging_dir(&parent).is_err());
        let aside = dir.join("init-moved-aside");
        fs::rename(&init, &aside).unwrap();
        open_staging_dir(&parent).unwrap();
        assert_eq!(mode_of(&dir.join(STAGING_DIR)), 0o700);
        assert_eq!(mode_of(&dir.join(STAGING_DIR).join(STAGING_MARKER)), 0o600);
        assert_eq!(mode_of(&aside), 0o400);
        fs::set_permissions(&aside, fs::Permissions::from_mode(0o700)).unwrap();
        assert_eq!(fs::read(aside.join("stray")).unwrap(), b"keep");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_record_staging_link_is_removed_on_open_even_past_a_full_sweep_window_of_entries_that_cannot_be_removed()
     {
        let dir = scratch("link-past-window");
        let mut store = store_in(&dir);
        store.replace(&[1; RECORD_SIZE]).unwrap();
        drop(store);
        let staging = dir.join(STAGING_DIR);
        make_decoy_window(&staging);
        let link = staging.join(staging_name(&[1; RECORD_SIZE], 0));
        fs::hard_link(dir.join(RECORD_NAME), &link).unwrap();
        let store = store_listed(&dir, decoy_window);
        assert!(!link.exists());
        assert_eq!(fs::metadata(dir.join(RECORD_NAME)).unwrap().nlink(), 1);
        assert_eq!(loaded(&store), Some([1; RECORD_SIZE]));
        assert_eq!(fs::read_dir(&staging).unwrap().count(), SWEEP_LIMIT + 1);
        drop(store);
        let mut store = store_in(&dir);
        store.clear().unwrap();
        assert_eq!(loaded(&store), None);
        fs::remove_dir_all(&dir).unwrap();
    }

    fn failing_staging_lookup(dir: &fs::File, name: &CStr) -> io::Result<Option<libc::stat>> {
        if name.to_bytes().starts_with(STAGING_PREFIX.as_bytes()) {
            return Err(io::Error::other("injected look-up failure"));
        }
        return entry_stat(dir, name);
    }

    fn pinned(dir: &Path) -> (fs::File, fs::File, CString) {
        return (
            fs::File::open(dir).unwrap(),
            fs::File::open(dir.join(STAGING_DIR)).unwrap(),
            c_string(RECORD_NAME).unwrap(),
        );
    }

    fn record_with_second_link(dir: &Path) {
        let mut store = store_in(dir);
        store.replace(&[1; RECORD_SIZE]).unwrap();
        drop(store);
        fs::hard_link(
            dir.join(RECORD_NAME),
            dir.join(STAGING_DIR)
                .join(staging_name(&[1; RECORD_SIZE], 0)),
        )
        .unwrap();
    }

    #[test]
    fn a_failed_look_up_while_removing_the_link_is_an_error_and_the_link_is_kept() {
        let dir = scratch("link-lookup-fails");
        record_with_second_link(&dir);
        let (parent, staging, name) = pinned(&dir);
        let linked = dir
            .join(STAGING_DIR)
            .join(staging_name(&[1; RECORD_SIZE], 0));
        assert!(
            unlink_record_staging_link(&staging, &parent, &name, failing_staging_lookup, sync_dir)
                .is_err()
        );
        assert_eq!(fs::metadata(&linked).unwrap().nlink(), 2);
        unlink_record_staging_link(&staging, &parent, &name, entry_stat, sync_dir).unwrap();
        assert!(!linked.exists());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_link_that_cannot_be_removed_refuses_the_open_and_the_next_open_removes_it() {
        let dir = scratch("link-unremovable");
        record_with_second_link(&dir);
        let staging = dir.join(STAGING_DIR);
        fs::set_permissions(&staging, fs::Permissions::from_mode(0o500)).unwrap();
        assert!(FileCheckpoint::open(&namespace(&dir), &record_path(&dir)).is_err());
        assert_eq!(fs::metadata(dir.join(RECORD_NAME)).unwrap().nlink(), 2);
        fs::set_permissions(&staging, fs::Permissions::from_mode(0o700)).unwrap();
        let store = store_in(&dir);
        assert_eq!(fs::metadata(dir.join(RECORD_NAME)).unwrap().nlink(), 1);
        assert_eq!(loaded(&store), Some([1; RECORD_SIZE]));
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_removal_that_is_not_confirmed_durable_refuses_every_open_until_a_sync_confirms_it() {
        let dir = scratch("link-sync-fails");
        record_with_second_link(&dir);
        let ns = namespace(&dir);
        let record = record_path(&dir);
        let stored = record_bytes(&dir);
        let ino = fs::metadata(dir.join(RECORD_NAME)).unwrap().ino();
        let names = |dir: &Path| -> Vec<String> {
            let mut names: Vec<String> = fs::read_dir(dir)
                .unwrap()
                .map(|e| e.unwrap().file_name().into_string().unwrap())
                .collect();
            names.sort();
            return names;
        };
        let entries = names(&dir);
        SYNCS.set(0);
        SYNCS_FAIL.set(true);
        assert!(
            FileCheckpoint::open_with_dir_sync(&ns, &record, counting_dir_sync)
                .err()
                .unwrap()
                .contains("cannot settle the checkpoint's staging links")
        );
        assert_eq!(SYNCS.get(), 1);
        assert_eq!(fs::metadata(dir.join(RECORD_NAME)).unwrap().nlink(), 1);
        assert!(FileCheckpoint::open_with_dir_sync(&ns, &record, counting_dir_sync).is_err());
        assert_eq!(SYNCS.get(), 2);
        assert_eq!(record_bytes(&dir), stored);
        assert_eq!(fs::metadata(dir.join(RECORD_NAME)).unwrap().ino(), ino);
        assert_eq!(names(&dir), entries);
        SYNCS_FAIL.set(false);
        let mut store =
            FileCheckpoint::open_with_dir_sync(&ns, &record, counting_dir_sync).unwrap();
        assert_eq!(SYNCS.get(), 3);
        assert_eq!(loaded(&store), Some([1; RECORD_SIZE]));
        store.replace(&[2; RECORD_SIZE]).unwrap();
        assert_eq!(loaded(&store), Some([2; RECORD_SIZE]));
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn every_open_syncs_the_staging_directory_even_when_no_second_link_is_found() {
        let dir = scratch("open-syncs");
        let ns = namespace(&dir);
        drop(store_in(&dir));
        SYNCS.set(0);
        drop(
            FileCheckpoint::open_with_dir_sync(&ns, &record_path(&dir), counting_dir_sync).unwrap(),
        );
        assert_eq!(SYNCS.get(), 1);
        drop(
            FileCheckpoint::open_with_dir_sync(&ns, &record_path(&dir), counting_dir_sync).unwrap(),
        );
        assert_eq!(SYNCS.get(), 2);
        fs::remove_dir_all(&dir).unwrap();
    }

    fn failing_list(_dir: &fs::File) -> io::Result<Vec<std::ffi::OsString>> {
        return Err(io::Error::other("injected listing failure"));
    }

    #[test]
    fn a_failed_listing_leaves_the_debris_and_the_record_link_is_still_found_by_name() {
        let dir = scratch("listing-fails");
        record_with_second_link(&dir);
        let staging = dir.join(STAGING_DIR);
        let debris = staging.join(staging_name(&[6u8; RECORD_SIZE], 0));
        fs::write(&debris, b"debris").unwrap();
        sweep_stale_staging(&fs::File::open(&staging).unwrap(), failing_list);
        assert_eq!(fs::read(&debris).unwrap(), b"debris");
        let (parent, staging_dir, name) = pinned(&dir);
        unlink_record_staging_link(&staging_dir, &parent, &name, entry_stat, sync_dir).unwrap();
        assert_eq!(fs::metadata(dir.join(RECORD_NAME)).unwrap().nlink(), 1);
        sweep_stale_staging(&staging_dir, sweep_window);
        assert!(!debris.exists());
        fs::remove_dir_all(&dir).unwrap();
    }
}
