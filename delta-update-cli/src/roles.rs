//! The files one run names, opened once and checked by the identity of their open handles.
//!
//! A run names four files: the payload, the device, the checkpoint and the optional old image. The caller opens the
//! payload, so the bytes it authenticates are the bytes it pins. [`open`] then opens the other three, each through a handle
//! the run keeps. Only after all of them are open are their identities compared (`identity.rs`), and only then are the
//! device and the old image locked. A comparison of paths is not a check of the files the run uses, since a link in one of
//! the paths can be retargeted between the comparison and the open. A handle cannot be retargeted, so the comparison is made
//! on the files the run writes.
//!
//! Before each protected role is opened or looked at, the run takes the lock of the *entry* it names
//! of the directory holding it, so it says something about a name that has no file yet: one run's device, old image or
//! streamed payload and another run's checkpoint record refuse each other whichever of them exists first, and neither
//! can have the entry created, renamed over or removed under the other. The identity locks that follow cover the other way
//! one file can be named twice: a second path, hard link or device node for the same storage.
//!
//! Two roles of one run that name one entry take that entry lock once (`EntryLocks`), and the run's own payload entry
//! lock is handed to [`open`] so it is not taken twice against itself. Without that, an alias inside one run would be
//!
//! Device and old-image identity locks follow the comparison: a reader's shared lock and a writer's exclusive lock on
//! one inode conflict even within a process. The payload and existing checkpoint record are locked earlier, so an alias
//! involving them can itself cause a refusal before the later comparison.
//!
//! streamed payload's entry lock and its shared identity lock come first, taken by the caller before the payload is read
//! (`filesource.rs::PayloadPin`). Then the checkpoint's entry lock and its directory's exclusive lock, taken by its store.
//! Then the device's entry lock, and the old image's. Then the identities are compared, and last the device's exclusive
//! identity lock and the old image's shared one.
//!
//! The comparison runs before any write to any role. A checkpoint that does not exist yet is covered as well: its
//! directory is pinned by its store, and the entry at its name is looked up again through that directory before the
//!
//! Roles and records cannot name entries in the store's private staging namespace. Cleanup operates only within the
//!
//! Role entry resolution keeps the directory open from identity lookup through the role open:
//!
//! ```text
//! path -> pinned parent -> (device, inode, name) -> advisory entry lock
//!                    \-> openat(name, O_NOFOLLOW) -> identity check -> identity lock
//! ```
//!
//! Parent symlinks may be followed while pinning, but later retargeting cannot redirect the open. Final symlinks are
//! refused. Reserved path components and pinned ancestors are inspected, including symlink aliases to the staging
//! directory. This is not a sandbox: the hierarchy must be trusted against bind mounts and concurrent ancestor renames,
//! and advisory locks do not prevent changes by nonparticipants. procfs, when available, adds checks for aliases to
//! other reserved names; without it, the ancestry identity check specifically covers `.ota-checkpoint.staging`.
//! Buffered payloads are validated and copied before apply but do not retain entry or identity locks.

use std::fs;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::os::unix::io::AsRawFd;
use std::os::unix::io::FromRawFd;
use std::path::Path;

use crate::filecheckpoint::FileCheckpoint;
use crate::filecheckpoint::RESERVED_PREFIX;
use crate::filecheckpoint::is_reserved;
use crate::filedevice::FileDevice;
use crate::filedevice::UnlockedDevice;
use crate::fileimage::FileImage;
use crate::fileimage::UnlockedImage;
use crate::filelock::Entry;
use crate::filelock::FileLock;
use crate::filelock::LockDir;
use crate::filelock::LockMode;
use crate::filelock::PinnedEntry;
use crate::identity::Identity;

/// The paths a run names apart from its payload. The namespace that locks them is opened by the caller.
#[derive(Clone, Copy)]
pub struct Names<'a> {
    pub checkpoint: &'a str,
    pub device: &'a str,
    pub source: Option<&'a str>,
}

/// The roles of one run, each held through the handle that the alias check was made on.
pub struct Roles {
    pub store: FileCheckpoint,
    pub device: FileDevice,
    pub source: Option<FileImage>,
    _entries: Vec<FileLock>,
}

/// The entry a role names, resolved for the run: the name, and the identity of the directory that holds it.
///
/// A role may not name an entry with the reserved prefix or inside the staging directory, including through a parent
/// symlink. The store can remove owned staging leftovers, so roles must not use them. A path whose directory cannot be
/// opened or whose ancestry cannot be inspected is refused: without a pinned parent the entry lock cannot protect the
/// file later opened through it.
pub fn role_entry(path: &str) -> Result<PinnedEntry, String> {
    fn reserved_path(path: &Path) -> bool {
        path.components().any(|component| {
            let bytes = component.as_os_str().as_bytes();
            bytes
                .get(..RESERVED_PREFIX.len())
                .is_some_and(|head| head.eq_ignore_ascii_case(RESERVED_PREFIX.as_bytes()))
        })
    }
    let refused = || {
        format!(
            "role {} is in the updater's own namespace (reserved prefix {:?}); choose another path",
            path, RESERVED_PREFIX
        )
    };
    if reserved_path(Path::new(path)) {
        return Err(refused());
    }
    let entry = PinnedEntry::of(path)?;
    if is_reserved(&entry.entry.name) {
        return Err(refused());
    }
    let fd_path = format!("/proc/self/fd/{}", entry.dir.as_raw_fd());
    if fs::read_link(fd_path).is_ok_and(|resolved| reserved_path(&resolved)) {
        return Err(refused());
    }
    if inside_staging(&entry.dir)
        .map_err(|e| format!("cannot inspect ancestors of {}: {}", path, e))?
    {
        return Err(refused());
    }
    return Ok(entry);
}

pub(crate) fn inside_staging(dir: &fs::File) -> io::Result<bool> {
    fn open_dir_at(dir: &fs::File, name: &std::ffi::CStr) -> io::Result<fs::File> {
        let fd = unsafe {
            libc::openat(
                dir.as_raw_fd(),
                name.as_ptr(),
                libc::O_PATH | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        return Ok(unsafe { fs::File::from_raw_fd(fd) });
    }
    fn id(dir: &fs::File) -> io::Result<(u64, u64)> {
        let meta = dir.metadata()?;
        return Ok((meta.dev(), meta.ino()));
    }
    let mut current = dir.try_clone()?;
    for _ in 0..1024 {
        let current_id = id(&current)?;
        let parent = open_dir_at(&current, c"..")?;
        let parent_id = id(&parent)?;
        if current_id == parent_id {
            return Ok(false);
        }
        match open_dir_at(&parent, c".ota-checkpoint.staging") {
            Ok(staging) if id(&staging)? == current_id => return Ok(true),
            Ok(_) => {}
            Err(e)
                if matches!(
                    e.raw_os_error(),
                    Some(libc::ENOENT | libc::ENOTDIR | libc::ELOOP)
                ) => {}
            Err(e) => return Err(e),
        }
        current = parent;
    }
    return Err(io::Error::other(
        "directory ancestry exceeds inspection limit",
    ));
}

struct EntryLocks {
    names: Vec<String>,
    held: Vec<FileLock>,
}

impl EntryLocks {
    fn remember(self: &mut EntryLocks, ns: &LockDir, entry: &Entry) {
        self.names.push(ns.entry_lock_name(entry));
    }

    fn take(
        self: &mut EntryLocks,
        ns: &LockDir,
        entry: &Entry,
        mode: LockMode,
        shown: &str,
    ) -> Result<(), String> {
        let name = ns.entry_lock_name(entry);
        if self.names.iter().any(|held| held.eq(&name)) {
            return Ok(());
        }
        self.names.push(name);
        match ns.take_entry(entry, mode) {
            Err(e) => return Err(e.message(shown)),
            Ok(lock) => {
                self.held.push(lock);
                return Ok(());
            }
        };
    }
}

/// Open and lock the roles of a run, then refuse any two of them that are one file.
///
/// its entry lock and the directory's exclusive lock. Then this run takes the entry lock of the device's name and of the
/// old image's, each without waiting. `payload_entry` is the entry lock the caller already holds on the payload; it is
/// registered so this run does not take that name again against itself. Only then are the device and the old image opened,
/// and the identities of the open handles compared; no roles are returned for a run that has an alias. Only after the
/// comparison is the device's exclusive identity lock taken, and the old image's shared lock after it. The only entry the
/// run creates before the comparison is the store's case probe, which it removes.
///
/// Pass `None` for `payload_entry` when the caller holds no entry lock on the payload. A role then names an entry no lock
/// guards, and an alias that way is still refused, by the comparison of the open handles.
pub fn open(
    ns: &LockDir,
    payload: &fs::File,
    names: &Names,
    payload_entry: Option<&Entry>,
) -> Result<Roles, String> {
    let store = match FileCheckpoint::open(ns, names.checkpoint) {
        Ok(store) => store,
        Err(error) => {
            if let (Ok(meta), Ok(payload_id)) = (
                fs::symlink_metadata(names.checkpoint),
                Identity::of(payload),
            ) {
                let id = Identity::from_stat(meta.mode(), meta.dev(), meta.ino(), meta.rdev());
                if id == payload_id {
                    return Err("the payload and the checkpoint are the same file".to_string());
                }
            }
            return Err(error);
        }
    };
    let mut entries = EntryLocks {
        names: vec![],
        held: vec![],
    };
    entries.remember(ns, &store.entry());
    if let Some(entry) = payload_entry {
        entries.remember(ns, entry);
    }
    let device_entry = role_entry(names.device)?;
    entries.take(ns, &device_entry.entry, LockMode::Exclusive, names.device)?;
    let source_entry = match names.source {
        None => None,
        Some(path) => {
            let entry = role_entry(path)?;
            entries.take(ns, &entry.entry, LockMode::Shared, path)?;
            Some(entry)
        }
    };
    let device = UnlockedDevice::open_entry(names.device, &device_entry)?;
    let source = match (names.source, source_entry.as_ref()) {
        (Some(path), Some(entry)) => Some(UnlockedImage::open_entry(path.to_string(), entry)?),
        _ => None,
    };
    let mut handles: Vec<(&str, Identity)> = vec![
        (
            "payload",
            Identity::of(payload).map_err(|e| format!("payload: {}", e))?,
        ),
        ("device", device.identity()?),
    ];
    if let Some(id) = store
        .entry_identity()
        .map_err(|e| format!("checkpoint {}: {}", names.checkpoint, e))?
    {
        handles.push(("checkpoint", id));
    }
    if let Some(img) = &source {
        handles.push(("old image", img.identity()?));
    }
    refuse_aliases(&handles)?;
    let device = device.lock_exclusive(ns)?;
    let source = match source {
        None => None,
        Some(img) => Some(img.lock_shared(ns)?),
    };
    return Ok(Roles {
        store: store,
        device: device,
        source: source,
        _entries: entries.held,
    });
}

/// Refuse two roles that are one storage object. Each entry is a role and the identity of its open handle, and two entries
/// with equal identities are one object (`identity.rs`). A regular file is one object for its device and inode, so a symbolic
/// link or a second hard link to it is caught too. A block device is one object for its device number, so two device nodes
/// for one device are caught, whatever their inodes. Two partitions that share one disk are not, and nothing here can see
pub fn refuse_aliases(handles: &[(&str, Identity)]) -> Result<(), String> {
    for (i, (role, id)) in handles.iter().enumerate() {
        for (other, other_id) in &handles[..i] {
            if other_id == id {
                let what = match id {
                    Identity::Block { .. } => "the same block device",
                    Identity::File { .. } => "the same file",
                };
                return Err(format!("the {} and the {} are {}", other, role, what));
            }
        }
    }
    return Ok(());
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::filelock::LockMode;
    use crate::filelock::lock_role;
    use std::os::unix::fs::PermissionsExt;
    use std::os::unix::fs::symlink;
    use std::path::Path;
    use std::path::PathBuf;

    fn setup(name: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("ota-roles-{}-{}", std::process::id(), name));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("lock-dir")).unwrap();
        fs::set_permissions(root.join("lock-dir"), fs::Permissions::from_mode(0o700)).unwrap();
        fs::create_dir(root.join("real")).unwrap();
        fs::create_dir(root.join("other")).unwrap();
        fs::write(root.join("payload.bin"), b"authenticated payload").unwrap();
        fs::write(root.join("device.img"), [0u8; 64]).unwrap();
        symlink(root.join("real"), root.join("link")).unwrap();
        return root;
    }

    fn path(root: &Path, name: &str) -> String {
        return root.join(name).to_str().unwrap().to_string();
    }

    fn namespace(root: &Path) -> LockDir {
        return LockDir::open(&root.join("lock-dir")).unwrap();
    }

    #[test]
    fn a_checkpoint_parent_retargeted_after_the_payload_is_opened_is_checked_in_the_directory_it_now_names()
     {
        let root = setup("retarget-device");
        let payload = fs::File::open(root.join("payload.bin")).unwrap();
        fs::hard_link(root.join("device.img"), root.join("other").join("ckpt")).unwrap();
        fs::remove_file(root.join("link")).unwrap();
        symlink(root.join("other"), root.join("link")).unwrap();
        let ns = namespace(&root);
        let device = path(&root, "device.img");
        let through_link = path(&root, "link/ckpt");
        let names = Names {
            checkpoint: &through_link,
            device: &device,
            source: None,
        };
        let refused = open(&ns, &payload, &names, None).err().unwrap();
        assert!(
            refused.contains("the device and the checkpoint are the same file"),
            "{}",
            refused
        );
        assert_eq!(fs::read(root.join("device.img")).unwrap(), vec![0u8; 64]);
        let direct = path(&root, "real/ckpt");
        assert!(
            open(
                &ns,
                &payload,
                &Names {
                    checkpoint: &direct,
                    ..names
                },
                None
            )
            .is_ok()
        );
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_checkpoint_parent_retargeted_to_a_directory_holding_the_payload_is_refused_as_its_alias() {
        let root = setup("retarget-payload");
        let payload = fs::File::open(root.join("payload.bin")).unwrap();
        fs::hard_link(root.join("payload.bin"), root.join("other").join("ckpt")).unwrap();
        fs::remove_file(root.join("link")).unwrap();
        symlink(root.join("other"), root.join("link")).unwrap();
        let ns = namespace(&root);
        let device = path(&root, "device.img");
        let through_link = path(&root, "link/ckpt");
        let names = Names {
            checkpoint: &through_link,
            device: &device,
            source: None,
        };
        let refused = open(&ns, &payload, &names, None).err().unwrap();
        assert!(
            refused.contains("the payload and the checkpoint are the same file"),
            "{}",
            refused
        );
        assert_eq!(
            fs::read(root.join("payload.bin")).unwrap(),
            b"authenticated payload"
        );
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn an_old_image_that_is_the_device_under_a_retargeted_parent_is_refused_and_the_device_is_untouched()
     {
        let root = setup("retarget-source");
        let payload = fs::File::open(root.join("payload.bin")).unwrap();
        fs::hard_link(root.join("device.img"), root.join("other").join("old")).unwrap();
        let ns = namespace(&root);
        let device = path(&root, "device.img");
        let checkpoint = path(&root, "link/ckpt");
        let source = path(&root, "other/old");
        let names = Names {
            checkpoint: &checkpoint,
            device: &device,
            source: Some(&source),
        };
        let refused = open(&ns, &payload, &names, None).err().unwrap();
        assert!(
            refused.contains("the device and the old image are the same file"),
            "{}",
            refused
        );
        assert_eq!(fs::read(root.join("device.img")).unwrap(), vec![0u8; 64]);
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn distinct_roles_open_with_no_record_and_no_alias() {
        let root = setup("distinct");
        let payload = fs::File::open(root.join("payload.bin")).unwrap();
        let ns = namespace(&root);
        let device = path(&root, "device.img");
        let checkpoint = path(&root, "link/ckpt");
        let names = Names {
            checkpoint: &checkpoint,
            device: &device,
            source: None,
        };
        let roles = open(&ns, &payload, &names, None).unwrap();
        assert_eq!(roles.store.entry_identity().unwrap(), None);
        assert!(roles.source.is_none());
        drop(roles);
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_record_that_is_a_second_name_for_the_payload_is_refused_once_it_exists() {
        let root = setup("record-is-payload");
        let payload = fs::File::open(root.join("payload.bin")).unwrap();
        fs::hard_link(root.join("payload.bin"), root.join("real").join("ckpt")).unwrap();
        let ns = namespace(&root);
        let device = path(&root, "device.img");
        let checkpoint = path(&root, "real/ckpt");
        let names = Names {
            checkpoint: &checkpoint,
            device: &device,
            source: None,
        };
        let refused = open(&ns, &payload, &names, None).err().unwrap();
        assert!(
            refused.contains("the payload and the checkpoint are the same file"),
            "{}",
            refused
        );
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_payload_path_replaced_after_it_was_opened_is_compared_by_the_file_that_was_opened() {
        let root = setup("payload-replaced");
        let payload = fs::File::open(root.join("payload.bin")).unwrap();
        fs::hard_link(root.join("payload.bin"), root.join("real").join("ckpt")).unwrap();
        fs::write(root.join("replacement"), b"other bytes").unwrap();
        fs::rename(root.join("replacement"), root.join("payload.bin")).unwrap();
        let ns = namespace(&root);
        let device = path(&root, "device.img");
        let checkpoint = path(&root, "real/ckpt");
        let names = Names {
            checkpoint: &checkpoint,
            device: &device,
            source: None,
        };
        let refused = open(&ns, &payload, &names, None).err().unwrap();
        assert!(
            refused.contains("the payload and the checkpoint are the same file"),
            "{}",
            refused
        );
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_device_that_is_the_payload_is_refused_as_an_alias_even_while_the_payload_is_held_for_reading()
     {
        let root = setup("alias-under-read-lock");
        let payload = fs::File::open(root.join("payload.bin")).unwrap();
        let ns = namespace(&root);
        let _payload_lock = lock_role(&payload, "payload", LockMode::Shared, &ns).unwrap();
        fs::hard_link(root.join("payload.bin"), root.join("other").join("dev")).unwrap();
        let device = path(&root, "other/dev");
        let checkpoint = path(&root, "real/ckpt");
        let names = Names {
            checkpoint: &checkpoint,
            device: &device,
            source: None,
        };
        let refused = open(&ns, &payload, &names, None).err().unwrap();
        assert!(
            refused.contains("the payload and the device are the same file"),
            "{}",
            refused
        );
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn an_old_image_held_for_writing_by_another_run_is_refused_and_the_device_lock_it_took_is_released()
     {
        let root = setup("held-old-image");
        let payload = fs::File::open(root.join("payload.bin")).unwrap();
        let ns = namespace(&root);
        fs::write(root.join("old.img"), [0x11u8; 64]).unwrap();
        let writer = UnlockedDevice::open(&path(&root, "old.img"))
            .unwrap()
            .lock_exclusive(&ns)
            .unwrap();
        let device = path(&root, "device.img");
        let checkpoint = path(&root, "real/ckpt");
        let source = path(&root, "old.img");
        let names = Names {
            checkpoint: &checkpoint,
            device: &device,
            source: Some(&source),
        };
        let refused = open(&ns, &payload, &names, None).err().unwrap();
        assert!(refused.contains("another update holds"), "{}", refused);
        assert_eq!(fs::read(root.join("device.img")).unwrap(), vec![0u8; 64]);
        assert!(
            UnlockedDevice::open(&device)
                .unwrap()
                .lock_exclusive(&ns)
                .is_ok()
        );
        drop(writer);
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn an_old_image_read_by_another_run_is_shared_with_this_run_but_a_writer_on_it_is_refused() {
        let root = setup("shared-old-image");
        let payload = fs::File::open(root.join("payload.bin")).unwrap();
        let ns = namespace(&root);
        fs::write(root.join("old.img"), [0x11u8; 64]).unwrap();
        let reader = fs::File::open(root.join("old.img")).unwrap();
        let _other_run = lock_role(&reader, "old.img", LockMode::Shared, &ns).unwrap();
        let checkpoint = path(&root, "real/ckpt");
        let source = path(&root, "old.img");
        let device = path(&root, "device.img");
        let names = Names {
            checkpoint: &checkpoint,
            device: &device,
            source: Some(&source),
        };
        assert!(open(&ns, &payload, &names, None).is_ok());
        let names = Names {
            checkpoint: &checkpoint,
            device: &source,
            source: None,
        };
        let refused = open(&ns, &payload, &names, None).err().unwrap();
        assert!(refused.contains("another update holds"), "{}", refused);
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn an_unrelated_device_and_old_image_proceed_while_another_run_holds_a_third_file() {
        let root = setup("unrelated-roles");
        let payload = fs::File::open(root.join("payload.bin")).unwrap();
        let ns = namespace(&root);
        fs::write(root.join("third.img"), [0x33u8; 64]).unwrap();
        let _held = UnlockedDevice::open(&path(&root, "third.img"))
            .unwrap()
            .lock_exclusive(&ns)
            .unwrap();
        fs::write(root.join("old.img"), [0x11u8; 64]).unwrap();
        let checkpoint = path(&root, "real/ckpt");
        let device = path(&root, "device.img");
        let source = path(&root, "old.img");
        let names = Names {
            checkpoint: &checkpoint,
            device: &device,
            source: Some(&source),
        };
        assert!(open(&ns, &payload, &names, None).is_ok());
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn an_old_image_that_is_a_named_pipe_is_refused_without_waiting_for_a_writer() {
        let root = setup("pipe-source");
        let payload = fs::File::open(root.join("payload.bin")).unwrap();
        let ns = namespace(&root);
        let pipe = root.join("pipe");
        let name = std::ffi::CString::new(pipe.to_str().unwrap()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
        let device = path(&root, "device.img");
        let checkpoint = path(&root, "real/ckpt");
        let source = path(&root, "pipe");
        let names = Names {
            checkpoint: &checkpoint,
            device: &device,
            source: Some(&source),
        };
        let refused = open(&ns, &payload, &names, None).err().unwrap();
        assert!(
            refused.contains("neither a regular file nor a block device"),
            "{}",
            refused
        );
        assert_eq!(fs::read(root.join("device.img")).unwrap(), vec![0u8; 64]);
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn refuse_aliases_compares_identities_in_the_order_the_roles_are_given() {
        let file = |ino| Identity::File { dev: 1, ino: ino };
        let distinct = [
            ("payload", file(1)),
            ("device", file(2)),
            ("checkpoint", file(3)),
        ];
        assert!(refuse_aliases(&distinct).is_ok());
        let aliased = [
            ("payload", file(1)),
            ("device", file(2)),
            ("old image", file(2)),
        ];
        assert_eq!(
            refuse_aliases(&aliased).err().unwrap(),
            "the device and the old image are the same file"
        );
    }

    #[test]
    fn two_block_device_nodes_with_one_device_number_are_one_device_and_another_number_is_another()
    {
        let rdev = libc::makedev(8, 1);
        let node_a = Identity::from_stat(libc::S_IFBLK | 0o600, 1, 10, rdev);
        let node_b = Identity::from_stat(libc::S_IFBLK | 0o600, 1, 11, rdev);
        let refused = refuse_aliases(&[("device", node_a), ("old image", node_b)])
            .err()
            .unwrap();
        assert_eq!(
            refused,
            "the device and the old image are the same block device"
        );
        let partition = Identity::from_stat(libc::S_IFBLK | 0o600, 1, 12, libc::makedev(8, 2));
        assert!(refuse_aliases(&[("device", node_a), ("old image", partition)]).is_ok());
    }

    #[test]
    fn role_opens_and_entry_locks_use_the_same_parent_after_retargeting() {
        let root = setup("pinned-role-parent");
        let ns = namespace(&root);
        for name in ["device", "old"] {
            fs::write(root.join("real").join(name), b"original").unwrap();
            fs::write(root.join("other").join(name), b"replacement").unwrap();
        }
        let device_path = path(&root, "link/device");
        let old_path = path(&root, "link/old");
        let device_entry = role_entry(&device_path).unwrap();
        let old_entry = role_entry(&old_path).unwrap();
        let _device_lock = ns
            .take_entry(&device_entry.entry, LockMode::Exclusive)
            .unwrap();
        let _old_lock = ns.take_entry(&old_entry.entry, LockMode::Shared).unwrap();
        fs::remove_file(root.join("link")).unwrap();
        symlink(root.join("other"), root.join("link")).unwrap();

        let device = UnlockedDevice::open_entry(&device_path, &device_entry).unwrap();
        let image = UnlockedImage::open_entry(old_path, &old_entry).unwrap();
        assert_eq!(
            device.identity().unwrap(),
            Identity::of(&fs::File::open(root.join("real/device")).unwrap()).unwrap()
        );
        assert_eq!(
            image.identity().unwrap(),
            Identity::of(&fs::File::open(root.join("real/old")).unwrap()).unwrap()
        );
        assert!(matches!(
            ns.take_entry(
                &Entry::of(&path(&root, "real/device")).unwrap(),
                LockMode::Exclusive
            ),
            Err(crate::filelock::LockError::Held)
        ));
        assert!(
            ns.take_entry(&Entry::of(&device_path).unwrap(), LockMode::Exclusive)
                .is_ok()
        );
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn final_symlinks_are_rejected_for_device_and_old_image() {
        let root = setup("role-final-symlink");
        let ns = namespace(&root);
        let payload = fs::File::open(root.join("payload.bin")).unwrap();
        symlink(root.join("device.img"), root.join("device-link")).unwrap();
        symlink(root.join("payload.bin"), root.join("old-link")).unwrap();
        let checkpoint = path(&root, "real/ck");
        let device = path(&root, "device.img");
        let link = path(&root, "device-link");
        assert!(FileDevice::open_for_update(&link, &ns).is_err());
        let names = Names {
            checkpoint: &checkpoint,
            device: &link,
            source: None,
        };
        assert!(open(&ns, &payload, &names, None).is_err());
        let old = path(&root, "old-link");
        let names = Names {
            checkpoint: &checkpoint,
            device: &device,
            source: Some(&old),
        };
        assert!(open(&ns, &payload, &names, None).is_err());
        assert_eq!(fs::read(root.join("device.img")).unwrap(), vec![0u8; 64]);
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn reserved_staging_ancestors_are_rejected_through_parent_aliases() {
        let root = setup("reserved-parent");
        let staging = root.join("real/.ota-checkpoint.staging");
        fs::create_dir_all(staging.join("nested")).unwrap();
        symlink(&staging, root.join("stage-alias")).unwrap();
        symlink(staging.join("nested"), root.join("nested-alias")).unwrap();
        assert!(inside_staging(&fs::File::open(root.join("stage-alias")).unwrap()).unwrap());
        assert!(inside_staging(&fs::File::open(root.join("nested-alias")).unwrap()).unwrap());
        assert!(!inside_staging(&fs::File::open(root.join("real")).unwrap()).unwrap());
        for name in [
            "real/.ota-checkpoint.staging/file",
            "stage-alias/file",
            "nested-alias/file",
            "stage-alias/nested/file",
        ] {
            let refused = role_entry(&path(&root, name)).err().unwrap();
            assert!(
                refused.contains("updater's own namespace"),
                "{}: {}",
                name,
                refused
            );
        }
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_role_may_name_no_entry_under_the_stores_reserved_prefix() {
        let root = setup("reserved-role");
        let ns = namespace(&root);
        let payload = fs::File::open(root.join("payload.bin")).unwrap();
        let checkpoint = path(&root, "real/ck");
        let device = path(&root, "device.img");
        for name in [".ota-checkpoint.stage.4242-1-2", ".OTA-CHECKPOINT.probe"] {
            let joined = "real/".to_string() + name;
            let foreign = path(&root, &joined);
            let names = Names {
                checkpoint: &checkpoint,
                device: &device,
                source: Some(&foreign),
            };
            let refused = open(&ns, &payload, &names, None).err().unwrap();
            assert!(refused.contains("updater's own namespace"), "{}", refused);
            assert!(refused.contains(name), "{}", refused);
        }
        let ordinary = path(&root, "real/ck");
        let names = Names {
            checkpoint: &ordinary,
            device: &device,
            source: None,
        };
        assert!(open(&ns, &payload, &names, None).is_ok());
        let almost = path(&root, "real/.ota-checkpointx");
        assert!(role_entry(&almost).is_ok(), "almost-reserved");
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_block_device_is_never_the_same_object_as_a_regular_file() {
        let rdev = libc::makedev(8, 1);
        let block = Identity::from_stat(libc::S_IFBLK | 0o600, 1, 10, rdev);
        let file = Identity::from_stat(libc::S_IFREG | 0o600, 1, 10, 0);
        assert!(refuse_aliases(&[("device", block), ("old image", file)]).is_ok());
    }

    #[test]
    fn a_run_naming_another_runs_checkpoint_entry_as_its_device_is_refused_before_the_entry_is_opened()
     {
        let root = setup("entry-device");
        let payload = fs::File::open(root.join("payload.bin")).unwrap();
        let ns = namespace(&root);
        let ck = path(&root, "real/ck");
        let other = FileCheckpoint::open(&ns, &ck).unwrap();
        assert!(!root.join("real").join("ck").exists());
        let own = path(&root, "other/own-ck");
        let names = Names {
            checkpoint: &own,
            device: &ck,
            source: None,
        };
        let refused = open(&ns, &payload, &names, None).err().unwrap();
        assert!(refused.contains("another update holds"), "{}", refused);
        drop(other);
        fs::write(root.join("real").join("ck"), [0u8; 64]).unwrap();
        assert!(open(&ns, &payload, &names, None).is_ok());
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_checkpoint_and_a_device_naming_one_entry_refuse_each_other_in_both_orders() {
        let root = setup("entry-checkpoint");
        let ns = namespace(&root);
        fs::write(root.join("other").join("dev.img"), [0u8; 64]).unwrap();
        let device = path(&root, "other/dev.img");
        let held = FileDevice::open_for_update(&device, &ns).unwrap();
        let refused = FileCheckpoint::open(&ns, &device).err().unwrap();
        assert!(refused.contains("another update holds"), "{}", refused);
        drop(held);
        let ck = path(&root, "real/ck");
        let store = FileCheckpoint::open(&ns, &ck).unwrap();
        let refused = FileDevice::open_for_update(&ck, &ns).err().unwrap();
        assert!(refused.contains("another update holds"), "{}", refused);
        drop(store);
        fs::write(root.join("real").join("ck"), [0u8; 64]).unwrap();
        assert!(FileDevice::open_for_update(&ck, &ns).is_ok());
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn an_old_image_naming_another_runs_checkpoint_entry_is_refused_before_it_is_opened() {
        let root = setup("entry-old-image");
        let payload = fs::File::open(root.join("payload.bin")).unwrap();
        let ns = namespace(&root);
        let ck = path(&root, "real/ck");
        let other = FileCheckpoint::open(&ns, &ck).unwrap();
        let own = path(&root, "other/own-ck");
        let device = path(&root, "device.img");
        let names = Names {
            checkpoint: &own,
            device: &device,
            source: Some(&ck),
        };
        let refused = open(&ns, &payload, &names, None).err().unwrap();
        assert!(refused.contains("another update holds"), "{}", refused);
        drop(other);
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn two_entries_of_one_directory_are_held_apart_from_each_other() {
        let root = setup("entry-unrelated");
        let ns = namespace(&root);
        fs::write(root.join("real").join("first.img"), vec![0x11u8; 64]).unwrap();
        fs::write(root.join("real").join("second.img"), vec![0x22u8; 64]).unwrap();
        let first_path = path(&root, "real/first.img");
        let second_path = path(&root, "real/second.img");
        let first = FileDevice::open_for_update(&first_path, &ns).unwrap();
        assert!(
            FileDevice::open_for_update(&second_path, &ns).is_ok(),
            "a different entry of the same directory is a different lock"
        );
        drop(first);
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_device_that_names_the_records_entry_is_refused_as_an_alias_and_not_as_a_held_lock() {
        let root = setup("entry-self-alias");
        let payload = fs::File::open(root.join("payload.bin")).unwrap();
        let ns = namespace(&root);
        let shared = path(&root, "device.img");
        let names = Names {
            checkpoint: &shared,
            device: &shared,
            source: None,
        };
        let refused = open(&ns, &payload, &names, None).err().unwrap();
        assert!(
            refused.contains("the device and the checkpoint are the same file"),
            "{}",
            refused
        );
        fs::remove_dir_all(&root).unwrap();
    }
}
