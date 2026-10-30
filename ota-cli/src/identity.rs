//! The identity of a run's files, which decides whether two roles are one storage object.
//!
//! A regular file is one file to the filesystem: every path and every hard link to it has the same device and inode.
//! A block device is different. Each device node for one device carries the device's number (`st_rdev`), and two nodes
//! for one device have different inodes. So a block device is identified by its number, and any other file by its
//! device and inode. Two roles with one identity are one storage object, and the run refuses them.
//!
//! Two different device numbers can still be overlapping storage, such as two partitions of one disk. No identity here

use std::fs;
use std::io;
use std::os::unix::fs::MetadataExt;

/// What a role's open file is, compared with `==`. A block device is identified by its device number, and any other file
/// by its device and inode. The variant is part of the identity, so a block device never equals a file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Identity {
    /// A block device, by its device number. Every device node for the device carries it.
    Block { rdev: u64 },
    /// Any other file, by the device and inode of its open handle, whatever its type.
    File { dev: u64, ino: u64 },
}

impl Identity {
    /// The identity of an open file, read from its handle.
    pub fn of(file: &fs::File) -> io::Result<Identity> {
        let meta = file.metadata()?;
        return Ok(Identity::from_stat(meta.mode(), meta.dev(), meta.ino(), meta.rdev()));
    }

    /// The identity named by a file's mode, device, inode and device number, as `stat` reports them. The link is not
    /// followed by the caller, so a symbolic link has its own identity and not its target's.
    pub fn from_stat(mode: u32, dev: u64, ino: u64, rdev: u64) -> Identity {
        return if mode & libc::S_IFMT == libc::S_IFBLK {
            Identity::Block { rdev: rdev }
        } else {
            Identity::File { dev: dev, ino: ino }
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_block_device_is_identified_by_its_number_whatever_its_node() {
        let rdev = libc::makedev(8, 1);
        let first = Identity::from_stat(libc::S_IFBLK | 0o600, 1, 10, rdev);
        let second = Identity::from_stat(libc::S_IFBLK | 0o660, 2, 20, rdev);
        assert_eq!(first, second);
        let partition = Identity::from_stat(libc::S_IFBLK | 0o600, 1, 30, libc::makedev(8, 2));
        assert_ne!(first, partition);
    }

    #[test]
    fn a_block_device_is_never_the_same_as_a_file() {
        let rdev = libc::makedev(8, 1);
        let block = Identity::from_stat(libc::S_IFBLK | 0o600, 1, 10, rdev);
        let file = Identity::from_stat(libc::S_IFREG | 0o600, 1, 10, 0);
        assert_ne!(block, file);
        assert_ne!(file, block);
    }

    #[test]
    fn a_regular_file_is_identified_by_its_device_and_inode() {
        let file = Identity::from_stat(libc::S_IFREG | 0o600, 1, 10, 0);
        assert_eq!(file, Identity::from_stat(libc::S_IFREG | 0o644, 1, 10, 0));
        assert_ne!(file, Identity::from_stat(libc::S_IFREG | 0o600, 1, 11, 0));
        assert_ne!(file, Identity::from_stat(libc::S_IFREG | 0o600, 2, 10, 0));
    }

    #[test]
    fn a_symbolic_link_is_its_own_entry_and_not_its_targets() {
        let target = Identity::from_stat(libc::S_IFREG | 0o600, 1, 10, 0);
        let link = Identity::from_stat(libc::S_IFLNK | 0o777, 1, 11, 0);
        assert_ne!(target, link);
    }

    #[test]
    fn of_reads_the_identity_of_an_open_file_from_its_handle() {
        let dir = std::env::temp_dir().join(format!("ota-identity-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("file");
        fs::write(&path, b"x").unwrap();
        let file = fs::File::open(&path).unwrap();
        let meta = fs::metadata(&path).unwrap();
        assert_eq!(Identity::of(&file).unwrap(), Identity::File { dev: meta.dev(), ino: meta.ino() });
        fs::remove_dir_all(&dir).unwrap();
    }
}
