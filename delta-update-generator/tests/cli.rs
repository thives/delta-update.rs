use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::process::Output;

use delta_update_generator::check::check_full_payload;
use delta_update_payload::profile::EMBEDDED;
use delta_update_payload::rsa;
use delta_update_payload::verify::verify_payload;

const PRIVATE: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../delta-update-payload/tests/fixtures/private.pem"
);
const PUBLIC: &[u8] = include_bytes!("../../delta-update-payload/tests/fixtures/public.pem");

fn gen_bin() -> &'static str {
    return env!("CARGO_BIN_EXE_ota-gen");
}

fn scratch_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("ota-gen-{}-{}", std::process::id(), name));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    return dir;
}

fn run(args: &[&str]) -> Output {
    return Command::new(gen_bin()).args(args).output().unwrap();
}

fn run_limited(args: &[&str]) -> Output {
    return Command::new("sh")
        .arg("-c")
        .arg("ulimit -v 196608 && exec \"$0\" \"$@\"")
        .arg(gen_bin())
        .args(args)
        .output()
        .unwrap();
}

fn text(n: usize) -> Vec<u8> {
    return b"partition contents, repeated. "
        .iter()
        .cycle()
        .take(n)
        .cloned()
        .collect();
}

fn stderr_of(out: &Output) -> String {
    return String::from_utf8_lossy(&out.stderr).into_owned();
}

#[test]
fn full_mode_writes_a_payload_that_the_client_verifies() {
    let dir = scratch_dir("full-ok");
    let new = text(9000);
    let new_path = dir.join("new.img");
    let out_path = dir.join("out.payload");
    fs::write(&new_path, &new).unwrap();

    let out = run(&[
        "--full",
        new_path.to_str().unwrap(),
        out_path.to_str().unwrap(),
        PRIVATE,
    ]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    let err = stderr_of(&out);
    assert!(err.contains("full-image payload written"), "{}", err);
    assert!(
        !err.contains("search stopped"),
        "a full build runs no search: {}",
        err
    );

    let payload = fs::read(&out_path).unwrap();
    let key = rsa::parse_pubkey_pem(PUBLIC).unwrap_or_else(|e| panic!("{}", e.msg));
    let authenticated =
        verify_payload(&payload, &[key], &EMBEDDED).unwrap_or_else(|e| panic!("{}", e.msg));
    assert_eq!(
        authenticated.into_parsed().manifest.partitions[0].partition_name,
        "A/B"
    );
    let key = rsa::parse_pubkey_pem(PUBLIC).unwrap_or_else(|e| panic!("{}", e.msg));
    check_full_payload(&payload, key, &new, &EMBEDDED).unwrap_or_else(|e| panic!("{}", e.msg));
    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn full_mode_takes_a_block_size_a_name_and_explicit_limits() {
    let dir = scratch_dir("full-options");
    let new = text(2100);
    let new_path = dir.join("new.img");
    let out_path = dir.join("out.payload");
    fs::write(&new_path, &new).unwrap();

    let out = run(&[
        "--full",
        "--block-size",
        "512",
        "--partition-name",
        "system",
        "--max-block-size",
        "512",
        new_path.to_str().unwrap(),
        out_path.to_str().unwrap(),
        PRIVATE,
    ]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    let payload = fs::read(&out_path).unwrap();
    let key = rsa::parse_pubkey_pem(PUBLIC).unwrap_or_else(|e| panic!("{}", e.msg));
    let authenticated =
        verify_payload(&payload, &[key], &EMBEDDED).unwrap_or_else(|e| panic!("{}", e.msg));
    let manifest = authenticated.into_parsed().manifest;
    assert_eq!(manifest.partitions[0].partition_name, "system");
    assert_eq!(manifest.block_size, Some(512));
    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn full_mode_alternating_bytes_at_block_size_one_fit_a_192_mib_address_space() {
    let dir = scratch_dir("full-alternating-limit");
    let new: Vec<u8> = (0..1usize << 20)
        .map(|k| if k % 2 == 0 { 0 } else { 0x5a })
        .collect();
    let new_path = dir.join("new.img");
    let out_path = dir.join("out.payload");
    fs::write(&new_path, &new).unwrap();

    let out = run_limited(&[
        "--full",
        "-b",
        "1",
        new_path.to_str().unwrap(),
        out_path.to_str().unwrap(),
        PRIVATE,
    ]);
    assert!(out.status.success(), "{}", stderr_of(&out));

    let payload = fs::read(&out_path).unwrap();
    let key = rsa::parse_pubkey_pem(PUBLIC).unwrap_or_else(|e| panic!("{}", e.msg));
    let authenticated =
        verify_payload(&payload, &[key], &EMBEDDED).unwrap_or_else(|e| panic!("{}", e.msg));
    let operations = authenticated.into_parsed().manifest.partitions[0]
        .operations
        .len();
    assert_eq!(operations, 1, "the raw replacement is the one operation");
    let key = rsa::parse_pubkey_pem(PUBLIC).unwrap_or_else(|e| panic!("{}", e.msg));
    check_full_payload(&payload, key, &new, &EMBEDDED).unwrap_or_else(|e| panic!("{}", e.msg));
    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn full_mode_takes_exactly_three_positional_arguments() {
    let dir = scratch_dir("full-args");
    let new_path = dir.join("new.img");
    let out_path = dir.join("out.payload");
    fs::write(&new_path, text(64)).unwrap();
    let new = new_path.to_str().unwrap();
    let out_file = out_path.to_str().unwrap();

    let four = run(&["--full", new, out_file, PRIVATE, new]);
    assert_eq!(four.status.code(), Some(2), "{}", stderr_of(&four));
    assert!(
        stderr_of(&four).contains("Usage: ota-gen"),
        "{}",
        stderr_of(&four)
    );

    let two = run(&["--full", new, out_file]);
    assert_eq!(two.status.code(), Some(2), "{}", stderr_of(&two));
    assert!(!out_path.exists(), "a usage error writes nothing");
    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn full_mode_refuses_each_search_only_option_and_writes_nothing() {
    let dir = scratch_dir("full-flags");
    let new_path = dir.join("new.img");
    let out_path = dir.join("out.payload");
    fs::write(&new_path, text(64)).unwrap();
    let new = new_path.to_str().unwrap();
    let out_file = out_path.to_str().unwrap();

    let refusals = [
        ("-t", "--max-search-seconds", "5"),
        ("-s", "--stagnation-seconds", "1"),
        ("-m", "--min-improvement-bytes", "1"),
        ("-r", "--seed", "1"),
        ("-g", "--max-generations", "1"),
        ("-d", "--device", "software"),
    ];
    for (short, long, value) in refusals {
        for flag in [short, long] {
            let out = run(&["--full", flag, value, new, out_file, PRIVATE]);
            assert_eq!(out.status.code(), Some(2), "{} {}", flag, stderr_of(&out));
            assert!(
                stderr_of(&out).contains(long),
                "{} is named: {}",
                long,
                stderr_of(&out)
            );
            assert!(stderr_of(&out).contains("cannot be used with"));
            assert!(stderr_of(&out).contains("Usage:"));
            assert!(!out_path.exists(), "{} was refused without writing", flag);
        }
    }
    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn the_delta_form_is_unchanged_and_still_searches() {
    let dir = scratch_dir("delta");
    let old: Vec<u8> = (0..4096u32).map(|k| (k % 251) as u8).collect();
    let mut new = old.clone();
    new[100..300].fill(7);
    let old_path = dir.join("old.img");
    let new_path = dir.join("new.img");
    let out_path = dir.join("out.payload");
    fs::write(&old_path, &old).unwrap();
    fs::write(&new_path, &new).unwrap();

    let out = run(&[
        "-d",
        "software",
        "-g",
        "1",
        "-r",
        "1",
        old_path.to_str().unwrap(),
        new_path.to_str().unwrap(),
        out_path.to_str().unwrap(),
        PRIVATE,
    ]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    let err = stderr_of(&out);
    assert!(err.contains("search stopped"), "{}", err);
    assert!(err.contains("payload written"), "{}", err);
    fs::remove_dir_all(&dir).unwrap();
}

fn usage_error(args: &[&str]) {
    let out = run(args);
    let err = stderr_of(&out);
    assert_eq!(out.status.code(), Some(2), "{args:?}: {err}");
    assert!(err.contains("error:"), "{err}");
    assert!(err.contains("Usage:"), "{err}");
    assert!(err.contains("--help"), "{err}");
    assert!(out.stdout.is_empty());
}

#[test]
fn help_and_version_succeed_without_files() {
    for flag in ["-h", "--help"] {
        let out = run(&[flag]);
        assert!(out.status.success(), "{}", stderr_of(&out));
        assert!(out.stderr.is_empty());
        let help = String::from_utf8(out.stdout).unwrap();
        for text in [
            "Usage:",
            "--full",
            "--block-size",
            "--partition-name",
            "--max-search-seconds",
            "--stagnation-seconds",
            "--min-improvement-bytes",
            "--seed",
            "--max-generations",
            "--device",
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
            "[default: 30]",
            "[default: 5]",
            "[default: A/B]",
            "authenticated logical label",
            "does not automatically select",
            "COUNT",
            "BYTES",
            "usize",
            "u32",
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
            format!("ota-gen {}\n", env!("CARGO_PKG_VERSION"))
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
        vec!["--block-size"],
        vec!["--partition-name"],
        vec!["--max-operations"],
        vec!["--device", "other", "old", "new", "out", "key"],
        vec!["--full"],
        vec!["--full", "new", "out"],
        vec!["old", "new", "out"],
        vec!["old", "new", "out", "key", "extra"],
    ] {
        usage_error(&args);
    }
}

#[test]
fn malformed_and_out_of_range_numbers_are_usage_errors_before_io() {
    for (flag, values) in [
        ("--block-size", vec!["0", "-1", "1.5", "4294967296"]),
        ("--max-block-size", vec!["0", "4294967296"]),
        (
            "--max-operations",
            vec!["-1", "1.5", "18446744073709551616"],
        ),
        ("--max-encoded-manifest", vec!["NaN"]),
        (
            "--max-search-seconds",
            vec!["NaN", "inf", "-inf", "-1", "1e999", "junk"],
        ),
        ("--stagnation-seconds", vec!["0", "NaN", "inf", "-1"]),
        (
            "--min-improvement-bytes",
            vec!["-1", "1.5", "18446744073709551616"],
        ),
        ("--seed", vec!["-1", "18446744073709551616"]),
        ("--max-generations", vec!["-1", "18446744073709551616"]),
    ] {
        for bad in values {
            usage_error(&[flag, bad, "old", "new", "out", "key"]);
        }
    }
}

#[test]
fn explicit_resource_caps_reject_full_generation_without_writing() {
    let dir = scratch_dir("explicit-caps");
    let new_path = dir.join("new.img");
    let out_path = dir.join("out.payload");
    fs::write(&new_path, text(1024)).unwrap();
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
        let out = run(&[
            "--full",
            flag,
            value,
            new_path.to_str().unwrap(),
            out_path.to_str().unwrap(),
            PRIVATE,
        ]);
        assert_eq!(out.status.code(), Some(1), "{flag}: {}", stderr_of(&out));
        assert!(!stderr_of(&out).contains("Usage:"));
        assert!(!out_path.exists(), "{flag}: wrote an inadmissible payload");
    }
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn zero_operation_cap_allows_noops_and_zero_search_options_are_valid() {
    let dir = scratch_dir("zero-options");
    let image = dir.join("image.img");
    let out_path = dir.join("out.payload");
    fs::write(&image, text(1024)).unwrap();
    let out = run(&[
        "--max-operations",
        "0",
        image.to_str().unwrap(),
        image.to_str().unwrap(),
        out_path.to_str().unwrap(),
        PRIVATE,
    ]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    let key = rsa::parse_pubkey_pem(PUBLIC).unwrap();
    let payload = fs::read(&out_path).unwrap();
    let authenticated = verify_payload(&payload, &[key], &EMBEDDED).unwrap();
    assert!(
        authenticated.into_parsed().manifest.partitions[0]
            .operations
            .is_empty()
    );

    let out = run(&[
        "--max-search-seconds",
        "0",
        "--stagnation-seconds",
        "0.5",
        "--min-improvement-bytes",
        "0",
        "--seed",
        "0",
        "--max-generations",
        "0",
        "--device",
        "software",
        image.to_str().unwrap(),
        image.to_str().unwrap(),
        out_path.to_str().unwrap(),
        PRIVATE,
    ]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert!(stderr_of(&out).contains("after 0 generations"));
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn options_can_be_interleaved_with_files_and_double_dash_ends_option_parsing() {
    let dir = scratch_dir("option-placement");
    let image = dir.join("image.img");
    let out_path = dir.join("out.payload");
    fs::write(&image, text(1024)).unwrap();
    let img = image.to_str().unwrap();
    let payload = out_path.to_str().unwrap();
    for args in [
        vec![
            img,
            "--full",
            payload,
            "--partition-name",
            "system",
            PRIVATE,
            "--block-size",
            "512",
        ],
        vec![
            img,
            "--max-search-seconds",
            "0",
            img,
            payload,
            "--seed",
            "0",
            PRIVATE,
        ],
        vec!["--full", "--", img, payload, PRIVATE],
    ] {
        let out = run(&args);
        assert!(out.status.success(), "{args:?}: {}", stderr_of(&out));
        assert!(out_path.exists());
        fs::remove_file(&out_path).unwrap();
    }
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn runtime_file_key_and_output_errors_exit_one_without_usage() {
    let dir = scratch_dir("runtime-errors");
    let new_path = dir.join("new.img");
    let bad_key = dir.join("bad.pem");
    let missing = dir.join("absent.img");
    let out_path = dir.join("out.payload");
    fs::write(&new_path, text(1024)).unwrap();
    fs::write(&bad_key, b"not a key").unwrap();
    for (image, output, key) in [
        (missing.as_path(), out_path.as_path(), PRIVATE),
        (
            new_path.as_path(),
            out_path.as_path(),
            bad_key.to_str().unwrap(),
        ),
        (new_path.as_path(), dir.as_path(), PRIVATE),
    ] {
        let out = run(&[
            "--full",
            image.to_str().unwrap(),
            output.to_str().unwrap(),
            key,
        ]);
        assert_eq!(out.status.code(), Some(1), "{}", stderr_of(&out));
        assert!(!stderr_of(&out).contains("Usage:"));
    }
    assert!(!out_path.exists());
    fs::remove_dir_all(dir).unwrap();
}
