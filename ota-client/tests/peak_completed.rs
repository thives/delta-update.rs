












#[allow(dead_code)]
mod common;

use common::Counting;
use common::Window;
use common::charged;
use ota_client::policy::Policy;
use ota_client::apply::NoProgress;
use ota_client::apply::Trust;
use ota_client::checkpoint::MemoryStore;
use ota_client::checkpoint::TargetId;
use ota_client::sink::Readable;
use ota_client::sink::Sink;
use ota_client::sink::check_range;
use ota_client::source::Source;
use ota_client::stream;
use ota_generator::builder::Kind;
use ota_generator::builder::Op;
use ota_generator::builder::Spec;
use ota_generator::builder::build;
use ota_payload::error::PayloadError;
use ota_payload::layout;
use ota_payload::manifest::TYPE_REPLACE_FRAMES;
use ota_payload::profile::EMBEDDED;

#[global_allocator]
static ALLOC: Counting = Counting;

const PRIVATE: &[u8] = include_bytes!("../../ota-payload/tests/fixtures/private.pem");
const TARGET: TargetId = [0x77; 32];
const BS: usize = 4096;


struct Disk {
    data: Vec<u8>,
    writes: usize,
}

impl Sink for Disk {
    fn capacity(&self) -> u64 {
        return self.data.len() as u64;
    }

    fn write_at(&mut self, offset: u64, data: &[u8]) -> Result<(), PayloadError> {
        check_range(offset, data.len(), self.capacity())?;
        self.writes += 1;
        let off = offset as usize;
        self.data[off..(off + data.len())].copy_from_slice(data);
        return Ok(());
    }

    fn flush(&mut self) -> Result<(), PayloadError> {
        return Ok(());
    }
}

impl Readable for Disk {
    fn read_at(&self, offset: u64, out: &mut [u8]) -> Result<(), PayloadError> {
        check_range(offset, out.len(), self.capacity())?;
        let off = offset as usize;
        out.copy_from_slice(&self.data[off..(off + out.len())]);
        return Ok(());
    }
}



struct Borrowed<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Source for Borrowed<'a> {
    fn read(&mut self, out: &mut [u8]) -> Result<usize, PayloadError> {
        let n = out.len().min(64 * 1024).min(self.data.len() - self.pos);
        out[..n].copy_from_slice(&self.data[self.pos..(self.pos + n)]);
        self.pos += n;
        return Ok(n);
    }
}



fn compressed_update() -> (Vec<u8>, Vec<u8>, Vec<u8>) {
    let old = vec![0x11u8; 4 * BS];
    let new: Vec<u8> = (0..4 * BS).map(|i| b"update-engine "[i % 14]).collect();
    let mut op = Op::new(Kind::Frames, vec![(0, (4 * BS) as u64)]);
    op.data = new.clone();
    let spec = Spec {
        chunk: BS,
        partition: old.len() as u64,
        old_hash: None,
        ops: vec![op],
        sign: Some(PRIVATE),
    };
    let payload = match build(&spec) {
        Err(e) => panic!("builder failed: {}", e.msg),
        Ok(p) => p,
    };
    return (old, new, payload);
}

#[test]
fn a_completed_compressed_update_allocates_no_staging_frame() {
    let (old, new, payload) = compressed_update();
    let parsed = match layout::parse_unverified(&payload, &EMBEDDED) {
        Err(e) => panic!("fixture does not frame: {}", e.msg),
        Ok(u) => u.into_parsed(),
    };
    let manifest = &parsed.manifest;
    assert!(
        manifest.partitions[0]
            .operations
            .iter()
            .any(|op| op.r#type == TYPE_REPLACE_FRAMES),
        "the fixture must replay a compressed chunk"
    );
    let sig = manifest.signatures_size.unwrap_or(0) as usize;


    let prepared = match stream::prepare(
        Borrowed {
            data: &payload,
            pos: 0,
        },
        Trust::Unverified,
        &EMBEDDED,
    ) {
        Err(e) => panic!("prepare refused a valid payload: {}", e.msg),
        Ok(p) => p,
    };
    let mut dev = Disk {
        data: old,
        writes: 0,
    };
    let mut store = MemoryStore::new();

    match prepared.apply(
        |offset| Borrowed {
            data: &payload,
            pos: offset as usize,
        },
        &mut dev,
        &mut store,
        &TARGET,
        Policy::Development,
        &NoProgress {},
    ) {
        Err(e) => panic!("the first run was refused: {}", e.msg),
        Ok(_) => {}
    };
    assert!(dev.data == new, "the first run diverged from the target");
    let writes = dev.writes;



    let bound = charged(BS) + charged(sig);
    let window = Window::new();
    let (applied, peak) = window.measure(|| {
        prepared.apply(
            |offset| Borrowed {
                data: &payload,
                pos: offset as usize,
            },
            &mut dev,
            &mut store,
            &TARGET,
            Policy::Development,
            &NoProgress {},
        )
    });
    match applied {
        Err(e) => panic!("the completed run was refused: {}", e.msg),
        Ok(_) => {}
    };
    assert_eq!(dev.writes, writes, "a completed update wrote");
    assert!(dev.data == new, "the completed run diverged from the target");

    println!(
        "completed re-apply peak={} bound={} (chunk {} + signature {})",
        peak,
        bound,
        charged(BS),
        charged(sig)
    );

    assert!(
        peak >= charged(sig),
        "the window did not see the signature buffer: peak {}",
        peak
    );
    assert!(
        peak <= bound,
        "a completed update allocated {} bytes above its prepared state, more than its chunk and signature buffers ({})",
        peak,
        bound
    );
}
