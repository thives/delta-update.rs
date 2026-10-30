
mod support;

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::process::Output;
use std::process::Stdio;
use std::time::Duration;
use std::time::Instant;

use ota_payload::profile::EMBEDDED;
use support::namespace;

const PRIVATE: &[u8] = include_bytes!("../../ota-payload/tests/fixtures/private.pem");
const PUBLIC_PATH: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../ota-payload/tests/fixtures/public.pem"
);
const TARGET_ID: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
const BS: usize = 4096;
const PATIENCE: Duration = Duration::from_secs(30);

fn scratch_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("ota-limits-{}-{}", std::process::id(), name));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    return dir;
}

fn new_image() -> Vec<u8> {
    return (0..3 * BS as u32).map(|i| (i * 7 % 251) as u8).collect();
}

fn full_payload() -> Vec<u8> {
    return match ota_generator::build_full_payload_with_release(&new_image(), b"A/B", PRIVATE, BS, &EMBEDDED, 0) {
        Err(e) => panic!("build failed: {}", e.msg),
        Ok(v) => v,
    };
}

fn apply_args(dir: &Path, payload: &Path, device: &Path, extra: &[&str]) -> Vec<String> {
    let mut args: Vec<String> = vec![
        "-k".into(),
        PUBLIC_PATH.into(),
        "--lock-dir".into(),
        namespace(dir).to_str().unwrap().into(),
        "--checkpoint".into(),
        dir.join("ck").to_str().unwrap().into(),
        "--target-id".into(),
        TARGET_ID.into(),
    ];
    args.extend(extra.iter().map(|a| a.to_string()));
    args.push(payload.to_str().unwrap().into());
    args.push(device.to_str().unwrap().into());
    return args;
}

fn run_bounded(args: &[String]) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_ota-apply"))
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let start = Instant::now();
    loop {
        if child.try_wait().unwrap().is_some() {
            return child.wait_with_output().unwrap();
        }
        if start.elapsed() > PATIENCE {
            let _ = child.kill();
            let _ = child.wait();
            panic!("ota-apply did not finish within {:?}: {:?}", PATIENCE, args);
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn stderr_of(out: &Output) -> String {
    return String::from_utf8_lossy(&out.stderr).to_string();
}

#[test]
fn a_payload_over_the_buffered_limit_is_refused_before_any_write_and_names_the_way_out() {
    let dir = scratch_dir("over-limit");
    let payload = dir.join("p.bin");
    let device = dir.join("dev.img");
    let image = full_payload();
    fs::write(&payload, &image).unwrap();
    fs::write(&device, vec![0x33u8; new_image().len()]).unwrap();
    let limit = (image.len() - 1).to_string();
    let out = run_bounded(&apply_args(&dir, &payload, &device, &["--max-buffered-payload", &limit]));
    assert_eq!(out.status.code(), Some(1), "{}", stderr_of(&out));
    let err = stderr_of(&out);
    assert!(err.contains("more than the limit"), "{err}");
    assert!(err.contains("use --stream"), "{err}");
    assert_eq!(fs::read(&device).unwrap(), vec![0x33u8; new_image().len()]);
    assert!(!dir.join("ck").exists());
    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn a_payload_of_exactly_the_buffered_limit_applies_and_one_byte_less_is_refused() {
    let dir = scratch_dir("at-limit");
    let payload = dir.join("p.bin");
    let device = dir.join("dev.img");
    let image = full_payload();
    fs::write(&payload, &image).unwrap();
    fs::write(&device, vec![0u8; new_image().len()]).unwrap();
    let below = (image.len() - 1).to_string();
    let out = run_bounded(&apply_args(&dir, &payload, &device, &["--max-buffered-payload", &below]));
    assert_eq!(out.status.code(), Some(1), "{}", stderr_of(&out));
    assert_eq!(fs::read(&device).unwrap(), vec![0u8; new_image().len()]);
    let exact = image.len().to_string();
    let out = run_bounded(&apply_args(&dir, &payload, &device, &["--max-buffered-payload", &exact]));
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert_eq!(fs::read(&device).unwrap(), new_image());
    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn a_streamed_apply_is_not_bound_by_the_buffered_limit() {
    let dir = scratch_dir("stream-unbounded");
    let payload = dir.join("p.bin");
    let device = dir.join("dev.img");
    fs::write(&payload, full_payload()).unwrap();
    fs::write(&device, vec![0u8; new_image().len()]).unwrap();
    let buffered = run_bounded(&apply_args(&dir, &payload, &device, &["--max-buffered-payload", "1"]));
    assert_eq!(buffered.status.code(), Some(1), "{}", stderr_of(&buffered));
    let streamed = run_bounded(&apply_args(
        &dir,
        &payload,
        &device,
        &["--stream", "--max-buffered-payload", "1"],
    ));
    assert!(streamed.status.success(), "{}", stderr_of(&streamed));
    assert_eq!(fs::read(&device).unwrap(), new_image());
    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn a_named_pipe_is_refused_as_a_payload_without_waiting_for_a_writer() {
    let dir = scratch_dir("fifo-payload");
    let pipe = dir.join("pipe");
    let name = std::ffi::CString::new(pipe.to_str().unwrap()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
    let device = dir.join("dev.img");
    fs::write(&device, vec![0x44u8; new_image().len()]).unwrap();
    let out = run_bounded(&apply_args(&dir, &pipe, &device, &[]));
    assert_eq!(out.status.code(), Some(1), "{}", stderr_of(&out));
    assert!(stderr_of(&out).contains("is not a regular file"), "{}", stderr_of(&out));
    assert_eq!(fs::read(&device).unwrap(), vec![0x44u8; new_image().len()]);
    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn a_character_device_is_refused_as_a_payload_and_as_a_key_without_a_read() {
    if !Path::new("/dev/zero").exists() {
        eprintln!("skipped: /dev/zero is not present");
        return;
    }
    let dir = scratch_dir("char-device");
    let device = dir.join("dev.img");
    fs::write(&device, vec![0x55u8; new_image().len()]).unwrap();
    let out = run_bounded(&apply_args(&dir, Path::new("/dev/zero"), &device, &[]));
    assert_eq!(out.status.code(), Some(1), "{}", stderr_of(&out));
    assert!(stderr_of(&out).contains("is not a regular file"), "{}", stderr_of(&out));
    let payload = dir.join("p.bin");
    fs::write(&payload, full_payload()).unwrap();
    let mut args = apply_args(&dir, &payload, &device, &[]);
    args[1] = "/dev/zero".into();
    let out = run_bounded(&args);
    assert_eq!(out.status.code(), Some(1), "{}", stderr_of(&out));
    assert!(stderr_of(&out).contains("is not a regular file"), "{}", stderr_of(&out));
    assert_eq!(fs::read(&device).unwrap(), vec![0x55u8; new_image().len()]);
    assert!(!dir.join("ck").exists());
    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn an_oversized_key_file_is_refused_before_it_is_parsed_and_the_payload_limit_does_not_change_it() {
    let dir = scratch_dir("oversized-key");
    let key = dir.join("key.pem");
    fs::write(&key, vec![b'A'; 70 * 1024]).unwrap();
    fs::set_permissions(&key, fs::Permissions::from_mode(0o600)).unwrap();
    let payload = dir.join("p.bin");
    let device = dir.join("dev.img");
    fs::write(&payload, full_payload()).unwrap();
    fs::write(&device, vec![0x66u8; new_image().len()]).unwrap();
    let mut args = apply_args(&dir, &payload, &device, &[]);
    args[1] = key.to_str().unwrap().into();
    let out = run_bounded(&args);
    assert_eq!(out.status.code(), Some(1), "{}", stderr_of(&out));
    let err = stderr_of(&out);
    assert!(err.contains("more than the limit of 65536 bytes"), "{err}");
    assert_eq!(fs::read(&device).unwrap(), vec![0x66u8; new_image().len()]);
    fs::remove_dir_all(&dir).unwrap();
}
