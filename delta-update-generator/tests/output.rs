use std::fs;
use std::os::unix::fs::MetadataExt;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::Command;

const KEY: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../delta-update-payload/tests/fixtures/private.pem"
);

fn gen_bin() -> &'static str {
    return env!("CARGO_BIN_EXE_ota-gen");
}

fn scratch_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("ota-gen-output-{}-{}", std::process::id(), name));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    return dir;
}

fn new_image() -> Vec<u8> {
    return (0..8192u32).map(|i| (i * 13 % 251) as u8).collect();
}

fn leftovers(dir: &PathBuf) -> Vec<String> {
    return fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().to_string())
        .filter(|n| n.ends_with(".tmp"))
        .collect();
}

#[test]
fn an_output_that_is_the_new_image_is_refused_and_the_image_is_untouched() {
    let dir = scratch_dir("alias-image");
    let new = dir.join("new.img");
    fs::write(&new, new_image()).unwrap();
    let out = Command::new(gen_bin())
        .args(["--full", new.to_str().unwrap(), new.to_str().unwrap(), KEY])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("same file"));
    assert_eq!(fs::read(&new).unwrap(), new_image());
    assert!(leftovers(&dir).is_empty());
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn a_symlink_or_hard_link_to_the_key_is_refused_and_the_key_is_untouched() {
    let dir = scratch_dir("alias-key");
    let new = dir.join("new.img");
    fs::write(&new, new_image()).unwrap();
    let key_copy = dir.join("key-copy.pem");
    fs::copy(KEY, &key_copy).unwrap();
    let before = fs::read(&key_copy).unwrap();
    let link = dir.join("link.pem");
    std::os::unix::fs::symlink(&key_copy, &link).unwrap();
    let hard = dir.join("hard.pem");
    fs::hard_link(&key_copy, &hard).unwrap();
    for alias in [&link, &hard] {
        let out = Command::new(gen_bin())
            .args([
                "--full",
                new.to_str().unwrap(),
                alias.to_str().unwrap(),
                key_copy.to_str().unwrap(),
            ])
            .output()
            .unwrap();
        assert_eq!(out.status.code(), Some(1), "{}", alias.display());
        assert_eq!(fs::read(&key_copy).unwrap(), before);
    }
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn a_successful_write_replaces_an_existing_output_and_leaves_no_temporary_file() {
    let dir = scratch_dir("replace");
    let new = dir.join("new.img");
    fs::write(&new, new_image()).unwrap();
    let out_path = dir.join("out.payload");
    fs::write(&out_path, vec![0xffu8; 4]).unwrap();
    let out = Command::new(gen_bin())
        .args([
            "--full",
            new.to_str().unwrap(),
            out_path.to_str().unwrap(),
            KEY,
        ])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let written = fs::read(&out_path).unwrap();
    assert!(
        written.starts_with(b"CrAU"),
        "the old contents were not replaced"
    );
    assert!(leftovers(&dir).is_empty());
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn a_failed_write_is_reported_and_leaves_no_temporary_file() {
    let dir = scratch_dir("failed");
    let new = dir.join("new.img");
    fs::write(&new, new_image()).unwrap();
    let out_path = dir.join("missing").join("out.payload");
    let out = Command::new(gen_bin())
        .args([
            "--full",
            new.to_str().unwrap(),
            out_path.to_str().unwrap(),
            KEY,
        ])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("temporary file"));
    assert!(!out_path.exists());
    fs::remove_dir_all(dir).unwrap();
}

fn mode_of(path: &PathBuf) -> u32 {
    return fs::metadata(path).unwrap().permissions().mode() & 0o777;
}

#[test]
fn a_new_output_is_created_private() {
    let dir = scratch_dir("new-mode");
    let new = dir.join("new.img");
    fs::write(&new, new_image()).unwrap();
    let out_path = dir.join("out.payload");
    let out = Command::new(gen_bin())
        .args([
            "--full",
            new.to_str().unwrap(),
            out_path.to_str().unwrap(),
            KEY,
        ])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(mode_of(&out_path), 0o600);
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn a_replaced_private_output_stays_private() {
    let dir = scratch_dir("replace-private");
    let new = dir.join("new.img");
    fs::write(&new, new_image()).unwrap();
    let out_path = dir.join("out.payload");
    fs::write(&out_path, vec![0xffu8; 4]).unwrap();
    fs::set_permissions(&out_path, fs::Permissions::from_mode(0o600)).unwrap();
    let out = Command::new(gen_bin())
        .args([
            "--full",
            new.to_str().unwrap(),
            out_path.to_str().unwrap(),
            KEY,
        ])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(fs::read(&out_path).unwrap().starts_with(b"CrAU"));
    assert_eq!(mode_of(&out_path), 0o600);
    assert!(leftovers(&dir).is_empty());
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn a_replaced_output_takes_the_owner_only_mode_and_does_not_keep_the_old_one() {
    let dir = scratch_dir("replace-mode");
    let new = dir.join("new.img");
    fs::write(&new, new_image()).unwrap();
    let out_path = dir.join("out.payload");
    fs::write(&out_path, vec![0xffu8; 4]).unwrap();
    fs::set_permissions(&out_path, fs::Permissions::from_mode(0o644)).unwrap();
    let out = Command::new(gen_bin())
        .args([
            "--full",
            new.to_str().unwrap(),
            out_path.to_str().unwrap(),
            KEY,
        ])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(mode_of(&out_path), 0o600);
    fs::remove_dir_all(dir).unwrap();
}

fn acl_blob(entries: &[(u16, u16, u32)]) -> Vec<u8> {
    let mut blob = 2u32.to_le_bytes().to_vec();
    for (tag, perm, id) in entries {
        blob.extend_from_slice(&tag.to_le_bytes());
        blob.extend_from_slice(&perm.to_le_bytes());
        blob.extend_from_slice(&id.to_le_bytes());
    }
    return blob;
}

const ACL_UNDEFINED_ID: u32 = u32::MAX;
const ACL_USER_OBJ: u16 = 0x01;
const ACL_GROUP_OBJ: u16 = 0x04;
const ACL_GROUP: u16 = 0x08;
const ACL_MASK: u16 = 0x10;
const ACL_OTHER: u16 = 0x20;
const XATTR_DEFAULT_ACL: &[u8] = b"system.posix_acl_default\0";
const XATTR_ACCESS_ACL: &[u8] = b"system.posix_acl_access\0";

fn grant_default_group_acl(dir: &PathBuf, gid: u32) -> std::io::Result<()> {
    let blob = acl_blob(&[
        (ACL_USER_OBJ, 0o7, ACL_UNDEFINED_ID),
        (ACL_GROUP_OBJ, 0o7, ACL_UNDEFINED_ID),
        (ACL_GROUP, 0o6, gid),
        (ACL_MASK, 0o7, ACL_UNDEFINED_ID),
        (ACL_OTHER, 0o0, ACL_UNDEFINED_ID),
    ]);
    let path = std::ffi::CString::new(dir.to_str().unwrap()).unwrap();
    let rc = unsafe {
        libc::setxattr(
            path.as_ptr(),
            XATTR_DEFAULT_ACL.as_ptr() as *const libc::c_char,
            blob.as_ptr() as *const libc::c_void,
            blob.len(),
            0,
        )
    };
    if rc != 0 {
        return Err(std::io::Error::last_os_error());
    }
    return Ok(());
}

fn mask_of(path: &PathBuf) -> Option<u16> {
    let c = std::ffi::CString::new(path.to_str().unwrap()).unwrap();
    let size = unsafe {
        libc::getxattr(
            c.as_ptr(),
            XATTR_ACCESS_ACL.as_ptr() as *const libc::c_char,
            std::ptr::null_mut(),
            0,
        )
    };
    if size <= 0 {
        return None;
    }
    let mut buf = vec![0u8; size as usize];
    let got = unsafe {
        libc::getxattr(
            c.as_ptr(),
            XATTR_ACCESS_ACL.as_ptr() as *const libc::c_char,
            buf.as_mut_ptr() as *mut libc::c_void,
            buf.len(),
        )
    };
    if got <= 0 {
        return None;
    }
    return buf[4..got as usize]
        .chunks_exact(8)
        .find(|e| u16::from_le_bytes([e[0], e[1]]) == ACL_MASK)
        .map(|e| u16::from_le_bytes([e[2], e[3]]));
}

#[test]
fn a_new_output_in_a_directory_with_a_default_group_acl_grants_that_group_nothing() {
    let dir = scratch_dir("default-acl");
    let new = dir.join("new.img");
    fs::write(&new, new_image()).unwrap();
    let gid = unsafe { libc::getegid() };
    if let Err(e) = grant_default_group_acl(&dir, gid) {
        if e.raw_os_error() != Some(libc::ENOTSUP) && e.raw_os_error() != Some(libc::EOPNOTSUPP) {
            panic!("cannot set the default ACL on the test directory: {}", e);
        }
        eprintln!("skipped: the filesystem has no ACLs: {}", e);
        fs::remove_dir_all(dir).unwrap();
        return;
    }
    let out_path = dir.join("out.payload");
    let out = Command::new(gen_bin())
        .args([
            "--full",
            new.to_str().unwrap(),
            out_path.to_str().unwrap(),
            KEY,
        ])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        mode_of(&out_path) & 0o077,
        0,
        "group or other access on the output"
    );
    assert_eq!(
        mask_of(&out_path),
        Some(0),
        "the ACL mask still grants access"
    );
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn an_output_takes_its_group_from_the_directory_and_not_from_the_file_it_replaces() {
    let dir = scratch_dir("setgid");
    let new = dir.join("new.img");
    fs::write(&new, new_image()).unwrap();
    let groups = unsafe {
        let mut buf = [0 as libc::gid_t; 64];
        let n = libc::getgroups(buf.len() as libc::c_int, buf.as_mut_ptr());
        buf[..n.max(0) as usize].to_vec()
    };
    let egid = unsafe { libc::getegid() };
    let Some(group) = groups.into_iter().find(|g| *g != egid) else {
        eprintln!("skipped: the user has no supplementary group to own the directory");
        fs::remove_dir_all(dir).unwrap();
        return;
    };
    let out_path = dir.join("out.payload");
    fs::write(&out_path, vec![0xffu8; 4]).unwrap();
    if std::os::unix::fs::chown(&dir, None, Some(group)).is_err() {
        eprintln!("skipped: the directory cannot be given to group {}", group);
        fs::remove_dir_all(dir).unwrap();
        return;
    }
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o2700)).unwrap();
    let out = Command::new(gen_bin())
        .args([
            "--full",
            new.to_str().unwrap(),
            out_path.to_str().unwrap(),
            KEY,
        ])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(fs::metadata(&out_path).unwrap().gid(), group);
    assert_eq!(mode_of(&out_path), 0o600);
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).unwrap();
    fs::remove_dir_all(dir).unwrap();
}
