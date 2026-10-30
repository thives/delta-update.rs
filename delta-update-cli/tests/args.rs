use delta_update_generator::{SearchConfig, build_full_payload, build_payload_with};
use delta_update_payload::profile::EMBEDDED;
use std::{
    fs,
    process::{Command, Output},
};

mod support;

use support::namespace;

const TARGET: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
const PRIVATE: &[u8] = include_bytes!("../../delta-update-payload/tests/fixtures/private.pem");
const PUBLIC: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../delta-update-payload/tests/fixtures/public.pem"
);

fn run(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_ota-apply"))
        .args(args)
        .output()
        .unwrap()
}

fn usage_error(args: &[&str]) {
    let out = run(args);
    let err = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(2), "{args:?}: {err}");
    assert!(err.contains("error:"), "{err}");
    assert!(err.contains("Usage:"), "{err}");
    assert!(err.contains("--help"), "{err}");
    assert!(out.stdout.is_empty());
}

#[test]
fn help_and_version_succeed_without_required_arguments() {
    for flag in ["-h", "--help"] {
        let out = run(&[flag]);
        assert!(out.status.success());
        assert!(out.stderr.is_empty());
        let help = String::from_utf8(out.stdout).unwrap();
        for text in [
            "Usage:",
            "--key",
            "--source",
            "--checkpoint",
            "--lock-dir",
            "--target-id",
            "--stream",
            "--max-encoded-manifest",
            "--max-signature-message",
            "--max-decoded-bytes",
            "--max-operations",
            "--max-extents-per-op",
            "--max-total-extents",
            "--max-partitions",
            "--max-signatures",
            "--max-signature-data",
            "--max-block-size",
            "--max-frame-table-bytes",
            "--max-map-bytes",
            "[default: 16384]",
            "[default: 2048]",
            "[default: 32768]",
            "[default: 64]",
            "[default: 256]",
            "[default: 128]",
            "[default: 1]",
            "[default: 4]",
            "[default: 512]",
            "[default: 4096]",
            "[default: 8192]",
            "[default: 1024]",
            "authenticated logical label",
            "does not automatically select",
            "usize",
            "u32",
            "COUNT",
            "BYTES",
            "no-op",
        ] {
            assert!(help.contains(text), "missing {text}: {help}");
        }
        assert!(!help.contains("--profile"));
        assert!(!help.contains("-p "));
    }
    for flag in ["-V", "--version"] {
        let out = run(&[flag]);
        assert!(out.status.success());
        assert!(out.stderr.is_empty());
        assert_eq!(
            String::from_utf8(out.stdout).unwrap(),
            format!("ota-apply {}\n", env!("CARGO_PKG_VERSION"))
        );
    }
}

#[test]
fn missing_arguments_unknown_flags_and_removed_profiles_are_usage_errors() {
    for args in [
        vec![],
        vec!["--unknown"],
        vec!["-p", "embedded"],
        vec!["--profile", "host"],
        vec!["--key"],
        vec!["--source"],
        vec!["--checkpoint"],
        vec!["--target-id"],
        vec!["--max-operations"],
        vec!["--checkpoint", "state", "--target-id", TARGET, "payload"],
        vec![
            "--checkpoint",
            "state",
            "--target-id",
            TARGET,
            "payload",
            "device",
            "extra",
        ],
    ] {
        usage_error(&args);
    }
}

#[test]
fn the_lock_namespace_is_a_required_option() {
    let out = run(&[
        "-k",
        PUBLIC,
        "--checkpoint",
        "state",
        "--target-id",
        TARGET,
        "payload",
        "device",
    ]);
    assert_eq!(out.status.code(), Some(2));
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("--lock-dir"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn error_line(stderr: &[u8]) -> String {
    let text = String::from_utf8_lossy(stderr).into_owned();
    return text
        .lines()
        .find(|line| line.starts_with("error:"))
        .unwrap_or("")
        .to_string();
}

#[test]
fn malformed_and_out_of_range_resource_values_are_usage_errors() {
    for (flag, bad) in [
        ("--max-operations", "-1"),
        ("--max-operations", "1.5"),
        ("--max-operations", "18446744073709551616"),
        ("--max-encoded-manifest", "NaN"),
        ("--max-decoded-bytes", "infinity"),
        ("--max-block-size", "0"),
        ("--max-block-size", "4294967296"),
        ("--max-buffered-payload", "0"),
        ("--max-buffered-payload", "-1"),
        ("--max-buffered-payload", "1.5"),
    ] {
        let out = run(&[
            "-k",
            PUBLIC,
            "--lock-dir",
            "lock",
            "--checkpoint",
            "state",
            "--target-id",
            TARGET,
            flag,
            bad,
            "payload",
            "device",
        ]);
        let err = String::from_utf8_lossy(&out.stderr).into_owned();
        assert_eq!(out.status.code(), Some(2), "{flag} {bad}: {err}");
        assert!(err.contains("Usage:"), "{flag} {bad}: {err}");
        assert!(out.stdout.is_empty(), "{flag} {bad}");
        let line = error_line(&out.stderr);
        assert!(line.contains(&format!("'{bad}'")), "{flag} {bad}: {line}");
        if !bad.starts_with('-') {
            assert!(line.contains(flag), "{flag} {bad}: {line}");
        }
    }
    let out = run(&[
        "-k",
        PUBLIC,
        "--lock-dir",
        "lock",
        "--checkpoint",
        "state",
        "--target-id",
        TARGET,
        "--max-operations",
        "1",
        "payload",
        "device",
    ]);
    let err = String::from_utf8_lossy(&out.stderr).into_owned();
    assert_eq!(out.status.code(), Some(1), "{err}");
    assert!(!err.contains("Usage:"), "{err}");
}

#[test]
fn explicit_caps_reject_a_valid_signed_payload_before_writing_on_both_paths() {
    let dir = std::env::temp_dir().join(format!("ota-apply-caps-{}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();
    let payload = dir.join("full.payload");
    let device = dir.join("device.img");
    let checkpoint = dir.join("state");
    let lock_dir = namespace(&dir);
    let new = vec![0x5au8; 1024];
    fs::write(
        &payload,
        build_full_payload(&new, b"system", PRIVATE, 512, &EMBEDDED).unwrap(),
    )
    .unwrap();
    let original = vec![0x33u8; 1536];
    for stream in [false, true] {
        for (flag, value) in [
            ("--max-encoded-manifest", "1"),
            ("--max-signature-message", "1"),
            ("--max-decoded-bytes", "1"),
            ("--max-operations", "0"),
            ("--max-extents-per-op", "0"),
            ("--max-total-extents", "0"),
            ("--max-partitions", "0"),
            ("--max-signatures", "0"),
            ("--max-signature-data", "0"),
            ("--max-block-size", "1"),
        ] {
            fs::write(&device, &original).unwrap();
            let mut args = vec![
                "--key",
                PUBLIC,
                "--development",
                "--lock-dir",
                lock_dir.to_str().unwrap(),
                "--checkpoint",
                checkpoint.to_str().unwrap(),
                "--target-id",
                TARGET,
                flag,
                value,
            ];
            if stream {
                args.push("--stream");
            }
            args.extend([payload.to_str().unwrap(), device.to_str().unwrap()]);
            let out = run(&args);
            let err = String::from_utf8_lossy(&out.stderr);
            assert_eq!(out.status.code(), Some(1), "stream={stream} {flag}: {err}");
            assert!(!err.contains("Usage:"), "{err}");
            assert_eq!(fs::read(&device).unwrap(), original);
            assert!(!checkpoint.exists());
        }
        let mut args = vec![
            "--key",
            PUBLIC,
            "--development",
            "--lock-dir",
            lock_dir.to_str().unwrap(),
            "--checkpoint",
            checkpoint.to_str().unwrap(),
            "--target-id",
            TARGET,
        ];
        if stream {
            args.push("--stream");
        }
        args.extend([payload.to_str().unwrap(), device.to_str().unwrap()]);
        let out = run(&args);
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let mut expected = new.clone();
        expected.extend_from_slice(&original[new.len()..]);
        assert_eq!(fs::read(&device).unwrap(), expected);
        fs::remove_file(&checkpoint).unwrap();
    }
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn zero_operations_allows_a_noop_and_missing_files_exit_one() {
    let dir = std::env::temp_dir().join(format!("ota-apply-noop-{}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();
    let payload = dir.join("noop.payload");
    let device = dir.join("device.img");
    let checkpoint = dir.join("state");
    let lock_dir = namespace(&dir);
    let image = vec![0x33u8; 1024];
    fs::write(
        &payload,
        build_payload_with(
            &image,
            &image,
            b"system",
            PRIVATE,
            512,
            &EMBEDDED,
            &SearchConfig::default(),
        )
        .unwrap(),
    )
    .unwrap();
    for stream in [false, true] {
        fs::write(&device, &image).unwrap();
        let mut args = vec![
            "--key",
            PUBLIC,
            "--development",
            "--lock-dir",
            lock_dir.to_str().unwrap(),
            "--checkpoint",
            checkpoint.to_str().unwrap(),
            "--target-id",
            TARGET,
            "--max-operations",
            "0",
        ];
        if stream {
            args.push("--stream");
        }
        args.extend([payload.to_str().unwrap(), device.to_str().unwrap()]);
        let out = run(&args);
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert_eq!(fs::read(&device).unwrap(), image);
        fs::remove_file(&checkpoint).unwrap();
    }
    fs::remove_file(&payload).unwrap();
    let out = run(&[
        "--lock-dir",
        lock_dir.to_str().unwrap(),
        "--checkpoint",
        checkpoint.to_str().unwrap(),
        "--target-id",
        TARGET,
        "--development",
        payload.to_str().unwrap(),
        device.to_str().unwrap(),
    ]);
    assert_eq!(out.status.code(), Some(1));
    assert!(!String::from_utf8_lossy(&out.stderr).contains("Usage:"));
    fs::remove_dir_all(dir).unwrap();
}
