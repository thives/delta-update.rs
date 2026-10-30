
use std::fs;
use std::io::Write;
use std::os::unix::fs::MetadataExt;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process;

use clap::{CommandFactory, Parser, ValueEnum, error::ErrorKind};
use ota_cli_args::ResourceArgs;
use ota_generator::DEFAULT_BLOCK_SIZE;
use ota_generator::DevicePreference;
use ota_generator::SearchConfig;
use ota_generator::Stop;
use ota_generator::build_full_payload_with_release;
use ota_generator::build_report;
use ota_payload::profile::ResourceProfile;

#[derive(Parser)]
#[command(
    name = "ota-gen",
    version,
    about = "Generate a signed delta or self-contained full-image OTA payload",
    override_usage = "ota-gen [OPTIONS] <OLD_IMAGE> <NEW_IMAGE> <OUT_PAYLOAD> <PRIVATE_KEY>\n       ota-gen --full [OPTIONS] <NEW_IMAGE> <OUT_PAYLOAD> <PRIVATE_KEY>",
    after_help = "The partition name is an authenticated logical label used by the adapter. It does not automatically select a device path or A/B slot.\n\nResource limits default to the embedded baseline. Byte limits are bytes, not KiB. Integer limits are decimal and must fit the host usize (max-block-size: positive u32). Zero caps forbid the corresponding resource; max-operations=0 permits no-op updates.\n\nThe output is written owner-only (mode 0600). An existing output is replaced, and its mode, ACL and group are not kept."
)]
struct Args {
    #[arg(long)]
    full: bool,
    #[arg(short = 'b', long, default_value_t = DEFAULT_BLOCK_SIZE as u32, value_parser = clap::value_parser!(u32).range(1..), value_name = "BYTES")]
    block_size: u32,
    #[arg(short = 'n', long, default_value = "A/B", value_name = "NAME")]
    partition_name: String,
    #[arg(short = 't', long, default_value_t = 30.0, value_parser = nonnegative_seconds, conflicts_with = "full", value_name = "SECONDS")]
    max_search_seconds: f64,
    #[arg(short = 's', long, default_value_t = 5.0, value_parser = positive_seconds, conflicts_with = "full", value_name = "SECONDS")]
    stagnation_seconds: f64,
    #[arg(
        short = 'm',
        long,
        default_value_t = 1,
        conflicts_with = "full",
        value_name = "BYTES"
    )]
    min_improvement_bytes: u64,
    #[arg(short = 'r', long, default_value_t = 1, conflicts_with = "full")]
    seed: u64,
    #[arg(short = 'g', long, conflicts_with = "full", value_name = "COUNT")]
    max_generations: Option<u64>,
    #[arg(long, default_value_t = 0, value_name = "N")]
    release_version: u64,
    #[arg(short = 'd', long, value_enum, default_value_t = Device::Auto, conflicts_with = "full")]
    device: Device,
    #[arg(required = true, num_args = 1.., value_name = "FILES")]
    files: Vec<String>,
    #[command(flatten)]
    resources: ResourceArgs,
}

#[derive(Clone, Copy, ValueEnum)]
enum Device {
    Auto,
    Software,
}

fn nonnegative_seconds(text: &str) -> Result<f64, &'static str> {
    match text.parse::<f64>() {
        Ok(value) if value.is_finite() && value >= 0.0 => Ok(value),
        _ => Err("must be a finite, non-negative number of seconds"),
    }
}

fn positive_seconds(text: &str) -> Result<f64, &'static str> {
    let value = nonnegative_seconds(text)?;
    if value == 0.0 {
        return Err("must be a finite, strictly positive number of seconds");
    }
    Ok(value)
}

fn fail(message: &str) -> ! {
    eprintln!("ota-gen: {}", message);
    process::exit(1);
}

fn read_or_fail(path: &str) -> Vec<u8> {
    return match fs::read(path) {
        Err(e) => fail(&format!("{}: {}", path, e)),
        Ok(v) => v,
    };
}

fn stop_text(stop: Stop) -> &'static str {
    return match stop {
        Stop::Unchanged => "the images are equal",
        Stop::Deadline => "the time limit",
        Stop::Stagnation => "no progress within the stagnation window",
        Stop::Generations => "the generation limit",
    };
}

const NEW_OUTPUT_MODE: u32 = 0o600;

type ModeFn = fn(&fs::File, u32) -> std::io::Result<()>;

fn set_mode(file: &fs::File, mode: u32) -> std::io::Result<()> {
    return file.set_permissions(fs::Permissions::from_mode(mode));
}

fn write_output(out: &str, inputs: &[&str], bytes: &[u8]) -> Result<(), String> {
    return write_output_with(out, inputs, bytes, set_mode);
}

fn write_output_with(out: &str, inputs: &[&str], bytes: &[u8], apply_mode: ModeFn) -> Result<(), String> {
    if let Ok(target) = fs::metadata(out) {
        for input in inputs {
            if let Ok(src) = fs::metadata(input) {
                if src.dev() == target.dev() && src.ino() == target.ino() {
                    return Err(format!("{} is the same file as {}: refusing to overwrite an input", out, input));
                }
            }
        }
    }
    let (tmp, mut file) = match create_sibling(out) {
        Err(e) => return Err(format!("cannot create a temporary file beside {}: {}", out, e)),
        Ok(v) => v,
    };
    let staged = apply_mode(&file, NEW_OUTPUT_MODE)
        .and_then(|()| file.write_all(bytes))
        .and_then(|()| file.sync_all());
    drop(file);
    if let Err(e) = staged.and_then(|()| fs::rename(&tmp, out)) {
        let _ = fs::remove_file(&tmp);
        return Err(format!("cannot write {}: {}", out, e));
    }
    let dir = match Path::new(out).parent() {
        Some(d) if !d.as_os_str().is_empty() => d,
        _ => Path::new("."),
    };
    if let Ok(d) = fs::File::open(dir) {
        let _ = d.sync_all();
    }
    return Ok(());
}

fn create_sibling(out: &str) -> std::io::Result<(String, fs::File)> {
    let pid = std::process::id();
    let mut attempt: u32 = 0;
    loop {
        let tmp = format!("{}.{}.{}.tmp", out, pid, attempt);
        match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(NEW_OUTPUT_MODE)
            .open(&tmp)
        {
            Ok(file) => return Ok((tmp, file)),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists && attempt < 16 => {
                attempt += 1;
            }
            Err(e) => return Err(e),
        };
    }
}

fn run_full(
    new_path: &str,
    out_path: &str,
    key_path: &str,
    block_size: usize,
    name: &str,
    profile: &ResourceProfile,
    release_version: u64,
) {
    let new = read_or_fail(new_path);
    let pem = read_or_fail(key_path);
    let payload = match build_full_payload_with_release(
        &new,
        name.as_bytes(),
        &pem,
        block_size,
        profile,
        release_version,
    ) {
        Err(e) => fail(e.msg),
        Ok(v) => v,
    };
    if let Err(e) = write_output(out_path, &[new_path, key_path], &payload) {
        fail(&e);
    }
    eprintln!("full-image payload written: {} bytes", payload.len());
}

fn main() {
    let args: Args = ota_cli_args::parse();
    let expected = if args.full { 3 } else { 4 };
    if args.files.len() != expected {
        Args::command()
            .error(
                ErrorKind::WrongNumberOfValues,
                format!(
                    "{} generation requires exactly {expected} positional files",
                    if args.full { "full-image" } else { "delta" }
                ),
            )
            .exit();
    }
    let profile = ResourceProfile::from(args.resources);
    if args.full {
        run_full(
            &args.files[0],
            &args.files[1],
            &args.files[2],
            args.block_size as usize,
            &args.partition_name,
            &profile,
            args.release_version,
        );
        return;
    }
    let config = SearchConfig {
        max_search_seconds: args.max_search_seconds,
        stagnation_seconds: args.stagnation_seconds,
        min_improvement_bytes: args.min_improvement_bytes,
        seed: args.seed,
        release_version: args.release_version,
        max_generations: args.max_generations,
        device: match args.device {
            Device::Auto => DevicePreference::Auto,
            Device::Software => DevicePreference::Software,
        },
        ..SearchConfig::default()
    };
    let old = read_or_fail(&args.files[0]);
    let new = read_or_fail(&args.files[1]);
    let pem = read_or_fail(&args.files[3]);

    let built = match build_report(
        &old,
        &new,
        args.partition_name.as_bytes(),
        &pem,
        args.block_size as usize,
        &profile,
        &config,
    ) {
        Err(e) => fail(e.msg),
        Ok(v) => v,
    };
    if let Err(e) = write_output(
        &args.files[2],
        &[&args.files[0], &args.files[1], &args.files[3]],
        &built.payload,
    ) {
        fail(&e);
    }
    eprintln!(
        "search stopped: {} after {} generations",
        stop_text(built.stop),
        built.generations
    );
    if let Some(device) = &built.device {
        eprintln!(
            "device: {} ({} bytes resident, images uploaded once; {} bytes read back; {} candidates evaluated)",
            device, built.gpu.device_bytes, built.gpu.read_back, built.evaluated
        );
    }
    if let Some(raw) = built.raw_fitness {
        eprintln!("best raw size scored on the device: {} bytes (uncompressed)", raw);
    }
    eprintln!(
        "time: upload {:.3} s, search {:.3} s, encoding {:.3} s ({} programs, {} trials), finalizing {:.3} s",
        built.upload_seconds,
        built.search_seconds,
        built.compress_seconds,
        built.candidates,
        built.trials,
        built.finalize_seconds
    );
    eprintln!("payload written: {} bytes", built.payload.len());
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("ota-gen-main-{}-{}", std::process::id(), name));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        return dir;
    }

    fn leftovers(dir: &Path) -> Vec<String> {
        return fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().to_string())
            .filter(|n| n.ends_with(".tmp"))
            .collect();
    }

    fn refuse_mode(_file: &fs::File, _mode: u32) -> std::io::Result<()> {
        return Err(std::io::Error::other("injected permission failure"));
    }

    #[test]
    fn a_permission_failure_leaves_the_existing_output_and_no_temporary_file() {
        let dir = scratch("mode-fails");
        let out = dir.join("out.payload");
        fs::write(&out, b"old").unwrap();
        fs::set_permissions(&out, fs::Permissions::from_mode(0o640)).unwrap();
        let err = write_output_with(out.to_str().unwrap(), &[], b"new", refuse_mode).unwrap_err();
        assert!(err.contains("injected permission failure"), "{err}");
        assert_eq!(fs::read(&out).unwrap(), b"old");
        assert_eq!(fs::metadata(&out).unwrap().permissions().mode() & 0o777, 0o640);
        assert!(leftovers(&dir).is_empty());
        fs::remove_dir_all(dir).unwrap();
    }

    fn set_mode_while_empty(file: &fs::File, mode: u32) -> std::io::Result<()> {
        assert_eq!(file.metadata()?.len(), 0, "payload bytes were written before the mode was set");
        assert_eq!(mode, NEW_OUTPUT_MODE);
        return set_mode(file, mode);
    }

    #[test]
    fn the_mode_is_set_before_the_first_payload_byte_and_replaces_the_old_mode() {
        let dir = scratch("mode-first");
        let out = dir.join("out.payload");
        fs::write(&out, b"old").unwrap();
        fs::set_permissions(&out, fs::Permissions::from_mode(0o644)).unwrap();
        write_output_with(out.to_str().unwrap(), &[], b"new bytes", set_mode_while_empty).unwrap();
        assert_eq!(fs::read(&out).unwrap(), b"new bytes");
        assert_eq!(fs::metadata(&out).unwrap().permissions().mode() & 0o777, NEW_OUTPUT_MODE);
        assert!(leftovers(&dir).is_empty());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_permission_failure_for_a_new_output_creates_nothing() {
        let dir = scratch("mode-fails-new");
        let out = dir.join("out.payload");
        assert!(write_output_with(out.to_str().unwrap(), &[], b"new", refuse_mode).is_err());
        assert!(!out.exists());
        assert!(leftovers(&dir).is_empty());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_symbolic_link_output_is_replaced_by_a_private_file_and_its_target_keeps_its_mode() {
        let dir = scratch("link-mode");
        let target = dir.join("target.payload");
        fs::write(&target, b"keep").unwrap();
        fs::set_permissions(&target, fs::Permissions::from_mode(0o644)).unwrap();
        let out = dir.join("out.payload");
        std::os::unix::fs::symlink(&target, &out).unwrap();
        write_output(out.to_str().unwrap(), &[], b"new").unwrap();
        assert!(!fs::symlink_metadata(&out).unwrap().file_type().is_symlink());
        assert_eq!(fs::metadata(&out).unwrap().permissions().mode() & 0o777, NEW_OUTPUT_MODE);
        assert_eq!(fs::read(&target).unwrap(), b"keep");
        assert_eq!(fs::metadata(&target).unwrap().permissions().mode() & 0o777, 0o644);
        fs::remove_dir_all(dir).unwrap();
    }
}
