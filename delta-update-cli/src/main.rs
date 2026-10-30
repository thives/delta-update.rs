use std::fs;
use std::io;
use std::io::Read;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::process;

use delta_update_cli::filelock::Entry;
use delta_update_cli::filelock::LockDir;
use delta_update_cli::filesource::PayloadPin;
use delta_update_cli::roles;
use delta_update_cli::roles::Names;
use delta_update_cli::roles::Roles;
use delta_update_client::apply;
use delta_update_client::apply::Cleared;
use delta_update_client::apply::Outcome;
use delta_update_client::apply::Policy;
use delta_update_client::apply::Progress;
use delta_update_client::apply::Trust;
use delta_update_client::checkpoint::TargetId;
use delta_update_client::policy::VolatileFloor;
use delta_update_client::stream;
use delta_update_payload::error::Code;
use delta_update_payload::error::PayloadError;
use delta_update_payload::plan::Geometry;

use delta_update_payload::profile::ResourceProfile;
use delta_update_payload::rsa::PubKey;
use delta_update_payload::rsa::parse_pubkey_pem;
use delta_update_payload::sha256;

use clap::Parser;
use delta_update_cli_args::ResourceArgs;

const DEFAULT_MAX_BUFFERED_PAYLOAD: u64 = 64 * 1024 * 1024;
const MAX_KEY_BYTES: u64 = 64 * 1024;
const READ_CHUNK: usize = 8 * 1024;

#[derive(Parser)]
#[command(
    name = "ota-apply",
    version,
    about = "Apply a signed OTA payload to an explicitly selected file-backed device",
    after_help = "The partition name in the payload is an authenticated logical label used by the adapter. It does not automatically select a device path or A/B slot; DEVICE_FILE is explicit.\n\nResource limits default to the embedded baseline. Byte limits are bytes, not KiB. Integer limits are decimal and must fit the host usize (max-block-size: positive u32). Zero caps forbid the corresponding resource; max-operations=0 permits no-op updates."
)]
struct Args {
    #[arg(
        short = 'k',
        long = "key",
        value_name = "PUBLIC_PEM",
        required_unless_present = "development"
    )]
    key_pems: Vec<String>,
    #[arg(long)]
    development: bool,
    #[arg(long, value_name = "N", default_value_t = 0)]
    release_floor: u64,
    #[arg(short = 's', long, value_name = "OLD_IMAGE")]
    source: Option<String>,
    #[arg(long)]
    stream: bool,
    #[arg(
        long,
        value_name = "BYTES",
        default_value_t = DEFAULT_MAX_BUFFERED_PAYLOAD,
        value_parser = clap::value_parser!(u64).range(1..)
    )]
    max_buffered_payload: u64,
    #[arg(long, value_name = "FILE")]
    checkpoint: String,
    #[arg(long)]
    reset_checkpoint: bool,
    #[arg(long, value_name = "HEX", value_parser = parse_target_id)]
    target_id: TargetId,
    #[arg(long, value_name = "DIR")]
    lock_dir: String,
    payload_file: String,
    device_file: String,
    #[command(flatten)]
    resources: ResourceArgs,
}

struct PrintProgress {}

impl Progress for PrintProgress {
    fn report(self: &PrintProgress, done_ops: usize, total_ops: usize) {
        let denom = if total_ops > 0 { total_ops } else { 1 };
        eprintln!(
            "progress: {}/{} operations ({}%)",
            done_ops,
            total_ops,
            done_ops * 100 / denom
        );
    }
}

fn main() {
    let args: Args = delta_update_cli_args::parse();
    let profile = ResourceProfile::from(args.resources);

    let keys = parse_keys(args.key_pems);
    let floor = VolatileFloor::new(args.release_floor);
    let trust = if keys.len() > 0 {
        Trust::Verified(&keys)
    } else {
        eprintln!("WARNING: --development without -k: signatures will NOT be verified");
        Trust::Unverified
    };
    let policy = if args.development {
        Policy::Development
    } else {
        Policy::Strict(&floor)
    };
    let run = Run {
        payload_path: args.payload_file,
        device_path: args.device_file,
        source: args.source,
        target: args.target_id,
        checkpoint: args.checkpoint,
        lock_dir: args.lock_dir,
        trust: trust,
        policy: policy,
        profile: &profile,
        stream: args.stream,
        max_buffered: args.max_buffered_payload,
    };
    if args.reset_checkpoint {
        run_reset(run);
        return;
    }
    if args.stream {
        run_stream(run);
        return;
    }
    run_buffered(run);
}

struct Run<'a> {
    payload_path: String,
    device_path: String,
    source: Option<String>,
    target: TargetId,
    checkpoint: String,
    lock_dir: String,
    trust: Trust<'a>,
    policy: Policy<'a>,
    profile: &'a ResourceProfile,
    stream: bool,
    max_buffered: u64,
}

fn parse_target_id(text: &str) -> Result<TargetId, &'static str> {
    let digits = text.as_bytes();
    if digits.len() != 2 * sha256::SHA256_SIZE {
        return Err("--target-id must be 64 hexadecimal characters");
    }
    let mut id: TargetId = [0u8; sha256::SHA256_SIZE];
    let mut i = 0;
    while i < id.len() {
        id[i] = match (hex_value(digits[2 * i]), hex_value(digits[2 * i + 1])) {
            (Some(high), Some(low)) => (high << 4) | low,
            _ => return Err("--target-id must be 64 hexadecimal characters"),
        };
        i += 1;
    }
    return Ok(id);
}

fn hex_value(c: u8) -> Option<u8> {
    return match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    };
}

fn read_key(path: &str) -> Vec<u8> {
    let mut file = match fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(path)
    {
        Err(e) => fail(&format!("cannot open key {}: {}", path, e)),
        Ok(f) => f,
    };
    return match read_limited(&mut file, path, MAX_KEY_BYTES, "") {
        Err(msg) => fail(&msg),
        Ok(bytes) => bytes,
    };
}

fn read_payload(file: &mut fs::File, run: &Run) -> Vec<u8> {
    let hint = " (use --stream, or raise --max-buffered-payload)";
    return match read_limited(file, &run.payload_path, run.max_buffered, hint) {
        Err(msg) => fail(&msg),
        Ok(bytes) => bytes,
    };
}

fn read_limited(
    file: &mut fs::File,
    path: &str,
    limit: u64,
    hint: &str,
) -> Result<Vec<u8>, String> {
    let meta = match file.metadata() {
        Err(e) => return Err(format!("cannot inspect {}: {}", path, e)),
        Ok(m) => m,
    };
    if !meta.is_file() {
        return Err(format!("{} is not a regular file", path));
    }
    if meta.len() > limit {
        return Err(format!(
            "{} is {} bytes, more than the limit of {} bytes{}",
            path,
            meta.len(),
            limit,
            hint
        ));
    }
    return read_bounded(file, path, meta.len(), limit, hint);
}

fn read_bounded<R: Read>(
    reader: &mut R,
    path: &str,
    expected: u64,
    limit: u64,
    hint: &str,
) -> Result<Vec<u8>, String> {
    let limit_bytes = usize::try_from(limit).unwrap_or(usize::MAX);
    let start = usize::try_from(expected)
        .unwrap_or(usize::MAX)
        .min(limit_bytes);
    let mut data: Vec<u8> = Vec::new();
    if data.try_reserve_exact(start).is_err() {
        return Err(format!("cannot allocate {} bytes for {}", start, path));
    }
    let mut chunk = [0u8; READ_CHUNK];
    loop {
        let n = match reader.read(&mut chunk) {
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(format!("cannot read {}: {}", path, e)),
            Ok(0) => break,
            Ok(n) => n,
        };
        if n as u64 > limit - data.len() as u64 {
            return Err(format!(
                "{} grew past the limit of {} bytes while it was read{}",
                path, limit, hint
            ));
        }
        if reserve_within(&mut data, n, limit_bytes).is_err() {
            return Err(format!("cannot allocate memory for {}", path));
        }
        data.extend_from_slice(&chunk[..n]);
    }
    return Ok(data);
}

fn reserve_within(
    data: &mut Vec<u8>,
    extra: usize,
    limit: usize,
) -> Result<(), std::collections::TryReserveError> {
    let need = data.len() + extra;
    if need <= data.capacity() {
        return Ok(());
    }
    let target = need.max(data.capacity().saturating_mul(2)).min(limit);
    return data.try_reserve_exact(target - data.len());
}

fn open_buffered_payload(path: &str) -> fs::File {
    let entry = match roles::role_entry(path) {
        Ok(entry) => entry,
        Err(message) => fail(&message),
    };
    return match entry.open(libc::O_RDONLY) {
        Ok(file) => file,
        Err(error) => fail(&format!("cannot open payload {}: {}", path, error)),
    };
}

fn parse_keys(pems: Vec<String>) -> Vec<PubKey> {
    let mut keys: Vec<PubKey> = Vec::new();
    let mut i = 0;
    while i < pems.len() {
        let pem = read_key(&pems.as_slice()[i]);
        let key = match parse_pubkey_pem(pem.as_ref()) {
            Err(e) => {
                eprintln!("ota-apply: {}", e.msg);
                process::exit(1);
            }
            Ok(v) => v,
        };
        keys.push(key);
        i += 1;
    }
    return keys;
}

fn open_namespace(run: &Run) -> LockDir {
    return match LockDir::open(Path::new(&run.lock_dir)) {
        Err(msg) => fail(&msg),
        Ok(ns) => ns,
    };
}

fn open_roles(
    ns: &LockDir,
    payload: &fs::File,
    run: &Run,
    source: Option<&str>,
    payload_entry: Option<&Entry>,
) -> Roles {
    let names = Names {
        checkpoint: &run.checkpoint,
        device: &run.device_path,
        source: source,
    };
    return match roles::open(ns, payload, &names, payload_entry) {
        Err(msg) => fail(&msg),
        Ok(roles) => roles,
    };
}

fn fail(message: &str) -> ! {
    eprintln!("ota-apply: {}", message);
    process::exit(1);
}

fn show_summary(g: &Geometry, streaming: bool) {
    let note = if streaming { " (streaming)" } else { "" };
    eprintln!(
        "payload: revision={}, chunk_size={}, partitions=1, operations={}{}",
        g.minor_version, g.chunk_size, g.operations, note
    );
}

fn report(result: Result<Outcome, PayloadError>) {
    match result {
        Err(e) => {
            eprintln!("ota-apply: {}", e.msg);
            process::exit(1);
        }
        Ok(Outcome::Verified) => {
            eprintln!("update applied successfully and verified against the signed new image");
        }
        Ok(Outcome::Applied) => {
            eprintln!("update applied, NOT verified against the signed new image (--development)");
        }
    };
}

fn prepare_failed(e: PayloadError) -> ! {
    eprintln!("ota-apply: {}", e.msg);
    process::exit(1);
}

fn run_buffered(run: Run) {
    let mut payload = open_buffered_payload(&run.payload_path);
    let bytes = read_payload(&mut payload, &run);
    let prepared = match apply::prepare(bytes.as_ref(), run.trust, run.profile) {
        Err(e) => prepare_failed(e),
        Ok(v) => v,
    };
    show_summary(prepared.geometry(), false);
    let ns = open_namespace(&run);
    let mut roles = open_roles(&ns, &payload, &run, run.source.as_deref(), None);
    let target = run.target;
    match &roles.source {
        None => report(prepared.apply_to_device(
            &mut roles.device,
            &mut roles.store,
            &target,
            run.policy,
            &PrintProgress {},
        )),
        Some(img) => report(prepared.apply_to_device_with_image(
            &mut roles.device,
            img,
            &mut roles.store,
            &target,
            run.policy,
            &PrintProgress {},
        )),
    };
}

fn run_stream(run: Run) {
    let ns = open_namespace(&run);
    let pin = match PayloadPin::open(&run.payload_path, &ns) {
        Err(msg) => fail(&msg),
        Ok(pin) => pin,
    };
    let first = match pin.source_at(0) {
        Err(msg) => fail(&msg),
        Ok(source) => source,
    };
    let prepared = match stream::prepare(first, run.trust, run.profile) {
        Err(e) => prepare_failed(e),
        Ok(v) => v,
    };
    show_summary(prepared.geometry(), true);
    let mut roles = open_roles(
        &ns,
        pin.file(),
        &run,
        run.source.as_deref(),
        Some(&pin.entry()),
    );
    let open = |offset: u64| match pin.source_at(offset) {
        Err(msg) => fail(&msg),
        Ok(source) => source,
    };
    let target = run.target;
    match &roles.source {
        None => report(prepared.apply(
            open,
            &mut roles.device,
            &mut roles.store,
            &target,
            run.policy,
            &PrintProgress {},
        )),
        Some(img) => report(prepared.apply_with_image(
            open,
            &mut roles.device,
            img,
            &mut roles.store,
            &target,
            run.policy,
            &PrintProgress {},
        )),
    };
}

fn run_reset(run: Run) {
    let result = if run.stream {
        reset_streamed(&run)
    } else {
        reset_buffered(&run)
    };
    report_reset(result);
}

fn reset_buffered(run: &Run) -> Result<Option<Cleared>, PayloadError> {
    let mut payload = open_buffered_payload(&run.payload_path);
    let bytes = read_payload(&mut payload, run);
    let prepared = match apply::prepare(bytes.as_ref(), run.trust, run.profile) {
        Err(e) => prepare_failed(e),
        Ok(v) => v,
    };
    let ns = open_namespace(run);
    let mut roles = open_roles(&ns, &payload, run, None, None);
    return prepared.reset_checkpoint(&roles.device, &mut roles.store, &run.target);
}

fn reset_streamed(run: &Run) -> Result<Option<Cleared>, PayloadError> {
    let ns = open_namespace(run);
    let pin = match PayloadPin::open(&run.payload_path, &ns) {
        Err(msg) => fail(&msg),
        Ok(pin) => pin,
    };
    let first = match pin.source_at(0) {
        Err(msg) => fail(&msg),
        Ok(source) => source,
    };
    let prepared = match stream::prepare(first, run.trust, run.profile) {
        Err(e) => prepare_failed(e),
        Ok(v) => v,
    };
    let mut roles = open_roles(&ns, pin.file(), run, None, Some(&pin.entry()));
    return prepared.reset_checkpoint(&roles.device, &mut roles.store, &run.target);
}

fn report_reset(result: Result<Option<Cleared>, PayloadError>) {
    match result {
        Err(e) if e.code == Code::checkpoint_removal_uncertain => {
            fail(&uncertain_reset_message(e.msg))
        }
        Err(e) => {
            eprintln!("ota-apply: {}", e.msg);
            process::exit(1);
        }
        Ok(None) => {
            eprintln!(
                "ota-apply: no checkpoint to reset; its absence is confirmed durable, and the next run starts this update from its first operation"
            );
        }
        Ok(Some(cleared)) => {
            eprintln!(
                "ota-apply: reset the checkpoint in phase {:?}, with {} operations committed; the next run starts this update again from its first operation",
                cleared.phase, cleared.committed
            );
        }
    };
}

fn uncertain_reset_message(detail: &str) -> String {
    return format!(
        "{}. After a crash the record may be present again, so do not assume either state. Run \
         --reset-checkpoint again: it removes the record if one is present, and it succeeds only when the \
         absence is confirmed durable. Until it succeeds, do not apply",
        detail
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_uncertain_reset_tells_the_operator_to_run_the_reset_again_and_not_to_apply() {
        let detail = "checkpoint absence cannot be checked, so it is not confirmed";
        let message = uncertain_reset_message(detail);
        assert!(message.starts_with(detail), "{}", message);
        assert!(message.contains(". After a crash"), "{}", message);
        assert!(message.contains("--reset-checkpoint again"), "{}", message);
        assert!(
            message.contains("do not assume either state"),
            "{}",
            message
        );
        assert!(
            message.ends_with("Until it succeeds, do not apply"),
            "{}",
            message
        );
    }

    #[test]
    fn a_reader_of_exactly_the_limit_is_read_whole_and_a_byte_more_is_refused_as_it_grows() {
        let data = vec![7u8; 3 * READ_CHUNK + 5];
        let limit = data.len() as u64;
        assert_eq!(
            read_bounded(&mut &data[..], "p", limit, limit, "").unwrap(),
            data
        );
        let more = vec![7u8; data.len() + 1];
        let err = read_bounded(&mut &more[..], "p", 10, limit, " (hint)")
            .err()
            .unwrap();
        assert_eq!(
            err,
            format!(
                "p grew past the limit of {} bytes while it was read (hint)",
                limit
            )
        );
    }

    #[test]
    fn a_read_error_is_reported_with_its_path_and_no_data_is_returned() {
        struct Broken;
        impl Read for Broken {
            fn read(&mut self, _out: &mut [u8]) -> io::Result<usize> {
                return Err(io::Error::other("injected read failure"));
            }
        }
        let err = read_bounded(&mut Broken, "p", 0, 10, "").err().unwrap();
        assert_eq!(err, "cannot read p: injected read failure");
    }

    #[test]
    fn growing_the_buffer_within_the_limit_never_takes_its_capacity_past_the_limit() {
        let limit = 3 * READ_CHUNK;
        let mut data: Vec<u8> = Vec::new();
        for _ in 0..3 {
            reserve_within(&mut data, READ_CHUNK, limit).unwrap();
            data.extend_from_slice(&[0u8; READ_CHUNK]);
        }
        assert_eq!(data.len(), limit);
        assert!(data.capacity() <= limit, "{}", data.capacity());
    }

    #[test]
    fn a_regular_file_over_its_limit_is_refused_before_it_is_read_and_one_at_the_limit_is_read() {
        let dir = std::env::temp_dir().join(format!("ota-apply-limit-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("payload");
        fs::write(&path, [1u8; 11]).unwrap();
        let name = path.to_str().unwrap();
        let mut file = fs::File::open(&path).unwrap();
        assert_eq!(
            read_limited(&mut file, name, 10, " (hint)").err().unwrap(),
            format!(
                "{} is 11 bytes, more than the limit of 10 bytes (hint)",
                name
            )
        );
        let mut file = fs::File::open(&path).unwrap();
        assert_eq!(
            read_limited(&mut file, name, 11, "").unwrap(),
            vec![1u8; 11]
        );
        let mut dir_file = fs::File::open(&dir).unwrap();
        assert_eq!(
            read_limited(&mut dir_file, dir.to_str().unwrap(), 10, "")
                .err()
                .unwrap(),
            format!("{} is not a regular file", dir.to_str().unwrap())
        );
        fs::remove_dir_all(&dir).unwrap();
    }
}
