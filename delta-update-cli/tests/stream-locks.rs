use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;

use delta_update_cli::filecheckpoint::FileCheckpoint;
use delta_update_cli::filedevice::FileDevice;
use delta_update_cli::filelock::Entry;
use delta_update_cli::filelock::LockDir;
use delta_update_cli::filelock::LockError;
use delta_update_cli::filelock::LockMode;
use delta_update_cli::filelock::lock_role;
use delta_update_cli::filesource::PayloadPin;
use delta_update_client::apply::Outcome;
use delta_update_client::apply::Policy;
use delta_update_client::apply::Trust;
use delta_update_client::checkpoint::CheckpointStore;
use delta_update_client::checkpoint::Phase;
use delta_update_client::checkpoint::RECORD_SIZE;
use delta_update_client::checkpoint::Record;
use delta_update_client::checkpoint::TargetId;
use delta_update_client::engine::NoProgress;
use delta_update_client::sink::Readable;
use delta_update_client::sink::Sink;
use delta_update_client::stream;
use delta_update_generator::build_full_payload_with_release;
use delta_update_payload::error::PayloadError;
use delta_update_payload::profile::EMBEDDED;
use delta_update_payload::rsa::PubKey;
use delta_update_payload::rsa::parse_pubkey_pem;

const PRIVATE: &[u8] = include_bytes!("../../delta-update-payload/tests/fixtures/private.pem");
const PUBLIC_PATH: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../delta-update-payload/tests/fixtures/public.pem"
);
const TARGET: TargetId = [7u8; 32];
const BS: usize = 4096;

fn scratch_dir(name: &str) -> PathBuf {
    let dir =
        std::env::temp_dir().join(format!("ota-stream-locks-{}-{}", std::process::id(), name));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    return dir;
}

fn namespace(dir: &Path) -> PathBuf {
    let ns = dir.join("lock-dir");
    fs::create_dir_all(&ns).unwrap();
    fs::set_permissions(&ns, fs::Permissions::from_mode(0o700)).unwrap();
    return ns;
}

fn path(p: &Path) -> &str {
    return p.to_str().unwrap();
}

fn new_image() -> Vec<u8> {
    return (0..3 * BS as u32).map(|i| (i * 7 % 251) as u8).collect();
}

fn payload() -> Vec<u8> {
    return match build_full_payload_with_release(&new_image(), b"A/B", PRIVATE, BS, &EMBEDDED, 0) {
        Err(e) => panic!("build failed: {}", e.msg),
        Ok(v) => v,
    };
}

fn keys() -> Vec<PubKey> {
    let pem = fs::read(Path::new(PUBLIC_PATH)).unwrap();
    return match parse_pubkey_pem(&pem) {
        Err(e) => panic!("cannot parse the public key: {}", e.msg),
        Ok(k) => vec![k],
    };
}

struct Seam {
    ns_path: PathBuf,
    payload: String,
    entry_lock: String,
    reached: AtomicUsize,
    writer_refused: AtomicBool,
    entry_refused: AtomicBool,
}

impl Seam {
    fn probe(self: &Seam) {
        self.reached.fetch_add(1, Ordering::SeqCst);
        std::thread::scope(|scope| scope.spawn(|| self.probe_writer()).join().unwrap());
    }

    fn probe_writer(&self) {
        let ns = match LockDir::open(&self.ns_path) {
            Err(msg) => panic!("cannot open the lock namespace to probe it: {}", msg),
            Ok(ns) => ns,
        };
        let file = match fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(Path::new(&self.payload))
        {
            Err(e) => panic!("cannot open the payload to probe its lock: {}", e),
            Ok(f) => f,
        };
        let writer = lock_role(&file, &self.payload, LockMode::Exclusive, &ns);
        self.writer_refused.store(
            match writer {
                Err(msg) => msg.contains("another update holds"),
                Ok(_lock) => false,
            },
            Ordering::SeqCst,
        );
        self.entry_refused.store(
            match ns.take_mode(&self.entry_lock, LockMode::Exclusive) {
                Err(e) => matches!(e, LockError::Held),
                Ok(_lock) => false,
            },
            Ordering::SeqCst,
        );
    }
}

struct SeamDevice {
    inner: FileDevice,
    seam: Seam,
}

impl Sink for SeamDevice {
    fn capacity(self: &SeamDevice) -> u64 {
        return self.inner.capacity();
    }

    fn write_at(self: &mut SeamDevice, offset: u64, data: &[u8]) -> Result<(), PayloadError> {
        return self.inner.write_at(offset, data);
    }

    fn flush(self: &mut SeamDevice) -> Result<(), PayloadError> {
        return self.inner.flush();
    }
}

impl Readable for SeamDevice {
    fn read_at(self: &SeamDevice, offset: u64, out: &mut [u8]) -> Result<(), PayloadError> {
        if self.seam.reached.load(Ordering::SeqCst) == 0 {
            self.seam.probe();
        }
        return self.inner.read_at(offset, out);
    }
}

struct SeamStore {
    inner: FileCheckpoint,
    seam: Seam,
}

impl CheckpointStore for SeamStore {
    fn load(self: &SeamStore, out: &mut [u8; RECORD_SIZE]) -> Result<bool, PayloadError> {
        return self.inner.load(out);
    }

    fn replace(self: &mut SeamStore, record: &[u8; RECORD_SIZE]) -> Result<(), PayloadError> {
        match Record::decode(record) {
            Err(_e) => {}
            Ok(r) if r.phase == Phase::Complete => {
                self.seam.probe();
            }
            Ok(_r) => {}
        };
        return self.inner.replace(record);
    }

    fn clear(self: &mut SeamStore) -> Result<(), PayloadError> {
        return self.inner.clear();
    }

    fn confirm_absent(self: &mut SeamStore) -> Result<(), PayloadError> {
        return self.inner.confirm_absent();
    }

    fn confirm_stored(self: &mut SeamStore) -> Result<(), PayloadError> {
        return self.inner.confirm_stored();
    }
}

struct Case {
    dir: PathBuf,
    ns_path: PathBuf,
    payload: PathBuf,
    device: PathBuf,
    checkpoint: PathBuf,
    ns: LockDir,
}

fn one_case(name: &str) -> Case {
    let dir = scratch_dir(name);
    let payload_path = dir.join("p.bin");
    let device_path = dir.join("dev.img");
    let checkpoint_path = dir.join("ck");
    fs::write(&payload_path, payload()).unwrap();
    fs::write(&device_path, vec![0u8; 3 * BS]).unwrap();
    let ns_path = namespace(&dir);
    let ns = LockDir::open(&ns_path).unwrap();
    return Case {
        dir: dir,
        ns_path: ns_path,
        payload: payload_path,
        device: device_path,
        checkpoint: checkpoint_path,
        ns: ns,
    };
}

fn one_seam(c: &Case) -> Seam {
    let entry = Entry::of(path(&c.payload)).unwrap();
    return Seam {
        ns_path: c.ns_path.clone(),
        payload: path(&c.payload).to_string(),
        entry_lock: c.ns.entry_lock_name(&entry),
        reached: AtomicUsize::new(0),
        writer_refused: AtomicBool::new(false),
        entry_refused: AtomicBool::new(false),
    };
}

fn streamed<D: Sink + Readable, C: CheckpointStore>(
    c: &Case,
    dev: &mut D,
    store: &mut C,
) -> Result<Outcome, PayloadError> {
    let trust = keys();
    let pin = match PayloadPin::open(path(&c.payload), &c.ns) {
        Err(msg) => panic!("cannot pin the payload: {}", msg),
        Ok(pin) => pin,
    };
    let first = match pin.source_at(0) {
        Err(msg) => panic!("cannot read the payload: {}", msg),
        Ok(source) => source,
    };
    let prepared = match stream::prepare(first, Trust::Verified(&trust), &EMBEDDED) {
        Err(e) => panic!("cannot prepare the payload: {}", e.msg),
        Ok(v) => v,
    };
    let open = |offset: u64| match pin.source_at(offset) {
        Err(msg) => panic!("cannot read the payload again: {}", msg),
        Ok(source) => source,
    };
    return prepared.apply(
        open,
        dev,
        store,
        &TARGET,
        Policy::Development,
        &NoProgress {},
    );
}

#[test]
fn a_writer_is_refused_through_the_final_verification_of_a_fresh_streamed_run() {
    let c = one_case("fresh-verify");
    let mut dev = SeamDevice {
        inner: FileDevice::open(path(&c.device).to_string()).unwrap(),
        seam: one_seam(&c),
    };
    let mut store = SeamStore {
        inner: FileCheckpoint::open(&c.ns, path(&c.checkpoint)).unwrap(),
        seam: one_seam(&c),
    };
    let got = streamed(&c, &mut dev, &mut store);
    assert!(got.is_ok(), "{}", got.err().unwrap().msg);
    assert_eq!(dev.seam.reached.load(Ordering::SeqCst), 1);
    assert!(
        dev.seam.writer_refused.load(Ordering::SeqCst),
        "a writer was let in during final verification"
    );
    assert!(
        dev.seam.entry_refused.load(Ordering::SeqCst),
        "the payload's entry was replaceable during final verification"
    );
    assert_eq!(fs::read(&c.device).unwrap(), new_image());
    let after = one_seam(&c);
    after.probe();
    assert!(
        !after.writer_refused.load(Ordering::SeqCst),
        "the payload stayed locked after the run"
    );
    assert!(
        !after.entry_refused.load(Ordering::SeqCst),
        "the payload's entry stayed locked after the run"
    );
    fs::remove_dir_all(&c.dir).unwrap();
}

#[test]
fn a_writer_is_refused_through_the_completion_record_of_a_fresh_streamed_run() {
    let c = one_case("fresh-complete");
    let mut dev = FileDevice::open(path(&c.device).to_string()).unwrap();
    let mut store = SeamStore {
        inner: FileCheckpoint::open(&c.ns, path(&c.checkpoint)).unwrap(),
        seam: one_seam(&c),
    };
    let got = streamed(&c, &mut dev, &mut store);
    assert!(got.is_ok(), "{}", got.err().unwrap().msg);
    assert_eq!(store.seam.reached.load(Ordering::SeqCst), 1);
    assert!(
        store.seam.writer_refused.load(Ordering::SeqCst),
        "a writer was let in at the completion record"
    );
    assert!(
        store.seam.entry_refused.load(Ordering::SeqCst),
        "the payload's entry was replaceable at the completion record"
    );
    fs::remove_dir_all(&c.dir).unwrap();
}

#[test]
fn a_writer_is_refused_through_the_reverification_of_an_already_complete_streamed_run() {
    let c = one_case("already-complete");
    {
        let mut dev = SeamDevice {
            inner: FileDevice::open(path(&c.device).to_string()).unwrap(),
            seam: one_seam(&c),
        };
        let mut store = SeamStore {
            inner: FileCheckpoint::open(&c.ns, path(&c.checkpoint)).unwrap(),
            seam: one_seam(&c),
        };
        assert!(streamed(&c, &mut dev, &mut store).is_ok());
    }
    assert_eq!(fs::read(&c.device).unwrap(), new_image());
    let mut second_dev = SeamDevice {
        inner: FileDevice::open(path(&c.device).to_string()).unwrap(),
        seam: one_seam(&c),
    };
    let mut second_store = SeamStore {
        inner: FileCheckpoint::open(&c.ns, path(&c.checkpoint)).unwrap(),
        seam: one_seam(&c),
    };
    assert!(streamed(&c, &mut second_dev, &mut second_store).is_ok());
    assert_eq!(second_dev.seam.reached.load(Ordering::SeqCst), 1);
    assert_eq!(second_store.seam.reached.load(Ordering::SeqCst), 0);
    assert!(
        second_dev.seam.writer_refused.load(Ordering::SeqCst),
        "a writer was let in during reverification"
    );
    assert!(
        second_dev.seam.entry_refused.load(Ordering::SeqCst),
        "the payload's entry was replaceable during reverification"
    );
    assert_eq!(fs::read(&c.device).unwrap(), new_image());
    fs::write(&c.device, vec![0u8; 3 * BS]).unwrap();
    let mut failed_dev = SeamDevice {
        inner: FileDevice::open(path(&c.device).to_string()).unwrap(),
        seam: one_seam(&c),
    };
    assert!(streamed(&c, &mut failed_dev, &mut second_store).is_err());
    assert_eq!(failed_dev.seam.reached.load(Ordering::SeqCst), 1);
    assert!(failed_dev.seam.writer_refused.load(Ordering::SeqCst));
    let after = one_seam(&c);
    after.probe();
    assert!(!after.writer_refused.load(Ordering::SeqCst));
    assert!(!after.entry_refused.load(Ordering::SeqCst));
    fs::remove_dir_all(&c.dir).unwrap();
}
