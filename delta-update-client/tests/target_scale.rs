use delta_update_client::apply::Progress;
use delta_update_client::apply::Trust;
use delta_update_client::apply::prepare;
use delta_update_client::checkpoint::MemoryStore;
use delta_update_client::checkpoint::TargetId;
use delta_update_client::image::OldImage;
use delta_update_client::policy::Policy;
use delta_update_client::sink::Readable;
use delta_update_client::sink::Sink;
use delta_update_client::sink::check_range;
use delta_update_client::source::Source;
use delta_update_client::stream;
use delta_update_generator::SearchConfig;
use delta_update_generator::build_payload_with;
use delta_update_payload::error::PayloadError;
use delta_update_payload::profile::EMBEDDED;
use delta_update_payload::rsa::PubKey;
use delta_update_payload::rsa::parse_pubkey_pem;

const BS: usize = 4096;
const BLOCKS: usize = 16 * 1024 * 1024 / BS;
const TARGET: TargetId = [0x44; 32];

const PRIVATE: &[u8] = include_bytes!("../../delta-update-payload/tests/fixtures/private.pem");
const PUBLIC: &[u8] = include_bytes!("../../delta-update-payload/tests/fixtures/public.pem");

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

struct Old {
    data: Vec<u8>,
}

impl OldImage for Old {
    fn capacity(&self) -> u64 {
        return self.data.len() as u64;
    }

    fn read_at(&self, offset: u64, out: &mut [u8]) -> Result<(), PayloadError> {
        check_range(offset, out.len(), self.capacity())?;
        let off = offset as usize;
        out.copy_from_slice(&self.data[off..(off + out.len())]);
        return Ok(());
    }
}

struct Pieces {
    data: Vec<u8>,
    pos: usize,
}

impl Source for Pieces {
    fn read(&mut self, out: &mut [u8]) -> Result<usize, PayloadError> {
        let n = out.len().min(64 * 1024).min(self.data.len() - self.pos);
        out[..n].copy_from_slice(&self.data[self.pos..(self.pos + n)]);
        self.pos += n;
        return Ok(n);
    }
}

fn block(seed: usize, variant: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(BS);
    for i in 0..BS {
        out.push((((seed * 251 + variant * 37 + i * 13) % 254) + 1) as u8);
    }
    return out;
}

fn image(f: impl Fn(usize) -> Vec<u8>) -> Vec<u8> {
    let mut out = Vec::with_capacity(BLOCKS * BS);
    for b in 0..BLOCKS {
        out.extend(f(b));
    }
    return out;
}

fn keys() -> Vec<PubKey> {
    let mut v = Vec::new();
    v.push(parse_pubkey_pem(PUBLIC).map_err(|e| e.msg).unwrap());
    return v;
}

struct Quiet {}

impl Progress for Quiet {
    fn report(&self, done_ops: usize, total_ops: usize) {
        let _ = (done_ops, total_ops);
    }
}

fn round_trip(old: Vec<u8>, new: Vec<u8>) {
    let payload = build_payload_with(
        &old,
        &new,
        b"A/B",
        PRIVATE,
        BS,
        &EMBEDDED,
        &SearchConfig::quick(),
    )
    .map_err(|e| e.msg)
    .unwrap();
    apply_both(&payload, &old, &new);
}

fn apply_both(payload: &[u8], old: &[u8], new: &[u8]) {
    let keys = keys();

    let mut disk = Disk {
        data: old.to_vec(),
        writes: 0,
    };
    let prepared = prepare(payload, Trust::Verified(&keys), &EMBEDDED)
        .map_err(|e| e.msg)
        .unwrap();
    let image = Old { data: old.to_vec() };
    prepared
        .apply_to_device_with_image(
            &mut disk,
            &image,
            &mut MemoryStore::new(),
            &TARGET,
            Policy::Development,
            &Quiet {},
        )
        .map_err(|e| e.msg)
        .unwrap();
    assert!(
        disk.data == new,
        "buffered device path diverged from the target"
    );

    let mut disk = Disk {
        data: old.to_vec(),
        writes: 0,
    };
    let src = Pieces {
        data: payload.to_vec(),
        pos: 0,
    };
    let prepared = stream::prepare(src, Trust::Verified(&keys), &EMBEDDED)
        .map_err(|e| e.msg)
        .unwrap();
    prepared
        .apply_with_image(
            |off| Pieces {
                data: payload.to_vec(),
                pos: off as usize,
            },
            &mut disk,
            &image,
            &mut MemoryStore::new(),
            &TARGET,
            Policy::Development,
            &Quiet {},
        )
        .map_err(|e| e.msg)
        .unwrap();
    assert!(disk.data == new, "streaming path diverged from the target");
}

#[test]
fn alternating_changed_and_unchanged_blocks_round_trip() {
    let old = image(|b| block(b, 0));
    let new = image(|b| if b % 2 == 0 { block(b, 1) } else { block(b, 0) });
    round_trip(old, new);
}

#[test]
fn alternating_replacement_and_zero_runs_round_trip() {
    let old = image(|b| block(b, 0));
    let new = image(|b| {
        if b % 2 == 0 {
            block(b, 1)
        } else {
            vec![0u8; BS]
        }
    });
    round_trip(old, new);
}

#[test]
fn entirely_changed_image_round_trips() {
    round_trip(image(|b| block(b, 0)), image(|b| block(b, 1)));
}

#[test]
fn entirely_unchanged_image_round_trips_with_no_writes() {
    let old = image(|b| block(b, 0));
    let payload = build_payload_with(
        &old,
        &old,
        b"A/B",
        PRIVATE,
        BS,
        &EMBEDDED,
        &SearchConfig::quick(),
    )
    .map_err(|e| e.msg)
    .unwrap();
    let keys = keys();
    let mut disk = Disk {
        data: old,
        writes: 0,
    };
    let prepared = prepare(&payload, Trust::Verified(&keys), &EMBEDDED)
        .map_err(|e| e.msg)
        .unwrap();
    prepared
        .apply_to_device(
            &mut disk,
            &mut MemoryStore::new(),
            &TARGET,
            Policy::Development,
            &Quiet {},
        )
        .map_err(|e| e.msg)
        .unwrap();
    assert_eq!(disk.writes, 0);
}

#[test]
fn entirely_zeroed_image_round_trips() {
    round_trip(image(|b| block(b, 0)), vec![0u8; BLOCKS * BS]);
}

fn project_target() -> (Vec<u8>, Vec<u8>) {
    let old = image(|b| noise_block(b));
    let new = image(|b| {
        if b < 1024 {
            noise_block(b + 2048)
        } else if b < 2048 {
            match b % 4 {
                0 => text_block(b),
                1 => vec![0u8; BS],
                2 => noise_block(b),
                _ => noise_block(b + 7000),
            }
        } else if b % 64 == 0 {
            noise_block(b + 9000)
        } else {
            noise_block(b)
        }
    });
    return (old, new);
}

fn text_block(seed: usize) -> Vec<u8> {
    let word = format!("target block {} ", seed);
    let mut out = Vec::new();
    while out.len() < BS {
        out.extend_from_slice(word.as_bytes());
    }
    out.truncate(BS);
    return out;
}

fn noise_block(seed: usize) -> Vec<u8> {
    let mut x = (seed as u64).wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1;
    let mut out = Vec::with_capacity(BS);
    for _ in 0..BS {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        out.push((x >> 24) as u8);
    }
    return out;
}

#[test]
fn project_operations_round_trip_at_target_scale() {
    let (old, new) = project_target();
    let payload = build_payload_with(&old, &new, b"A/B", PRIVATE, BS, &EMBEDDED, &generous())
        .map_err(|e| e.msg)
        .unwrap();
    let parsed = delta_update_payload::layout::parse_unverified(&payload, &EMBEDDED)
        .map_err(|e| e.msg)
        .unwrap();
    let types: Vec<i32> = parsed.parsed().manifest.partitions[0]
        .operations
        .iter()
        .map(|op| op.r#type)
        .collect();
    assert!(types.contains(&delta_update_payload::manifest::TYPE_SOURCECOPY));
    assert!(
        types.contains(&delta_update_payload::manifest::TYPE_REPLACE_FRAMES)
            || types.contains(&delta_update_payload::manifest::TYPE_REPLACE_SPARSE)
    );
    apply_both(&payload, &old, &new);
}

#[test]
fn an_interrupted_project_payload_resumes_at_target_scale() {
    let (old, new) = project_target();
    let payload = build_payload_with(
        &old,
        &new,
        b"A/B",
        PRIVATE,
        BS,
        &EMBEDDED,
        &SearchConfig::quick(),
    )
    .map_err(|e| e.msg)
    .unwrap();
    let parsed = delta_update_payload::layout::parse_unverified(&payload, &EMBEDDED)
        .map_err(|e| e.msg)
        .unwrap();
    let blobs = parsed.parsed().info.blobs_start as usize;
    let blob_len = parsed.parsed().manifest.partitions[0]
        .operations
        .iter()
        .map(|op| op.data_length.unwrap_or(0) as usize)
        .sum::<usize>();
    let cut = blobs + blob_len * 3 / 4;
    let keys = keys();
    let image = Old { data: old.clone() };
    let mut store = MemoryStore::new();
    let mut disk = Disk {
        data: old.clone(),
        writes: 0,
    };

    let truncated = payload[..cut].to_vec();
    let partial = Pieces {
        data: truncated.clone(),
        pos: 0,
    };
    let prepared = stream::prepare(partial, Trust::Verified(&keys), &EMBEDDED)
        .map_err(|e| e.msg)
        .unwrap();
    assert!(
        prepared
            .apply_with_image(
                |off| Pieces {
                    data: truncated.clone(),
                    pos: off as usize,
                },
                &mut disk,
                &image,
                &mut store,
                &TARGET,
                Policy::Development,
                &Quiet {},
            )
            .is_err()
    );
    let whole = Pieces {
        data: payload.clone(),
        pos: 0,
    };
    let prepared = stream::prepare(whole, Trust::Verified(&keys), &EMBEDDED)
        .map_err(|e| e.msg)
        .unwrap();

    prepared
        .apply_with_image(
            |off| Pieces {
                data: payload.clone(),
                pos: off as usize,
            },
            &mut disk,
            &image,
            &mut store,
            &TARGET,
            Policy::Development,
            &Quiet {},
        )
        .map_err(|e| e.msg)
        .unwrap();
    assert!(disk.data == new, "the resumed run diverged from the target");
}

fn generous() -> SearchConfig {
    return SearchConfig {
        max_search_seconds: 300.0,
        stagnation_seconds: 300.0,
        max_generations: Some(40),
        ..SearchConfig::quick()
    };
}

struct CountingOld {
    data: Vec<u8>,
    reads: std::cell::Cell<usize>,
}

impl OldImage for CountingOld {
    fn capacity(&self) -> u64 {
        return self.data.len() as u64;
    }

    fn read_at(&self, offset: u64, out: &mut [u8]) -> Result<(), PayloadError> {
        check_range(offset, out.len(), self.capacity())?;
        self.reads.set(self.reads.get() + 1);
        let off = offset as usize;
        out.copy_from_slice(&self.data[off..(off + out.len())]);
        return Ok(());
    }
}

fn bench_patterns() -> Vec<(&'static str, Vec<u8>, Vec<u8>)> {
    let mut out = Vec::new();
    let (old, new) = project_target();
    out.push(("project", old, new));
    let old = image(|b| block(b, 0));
    let new = image(|b| {
        if b % 2 == 0 {
            text_block(b)
        } else {
            block(b, 0)
        }
    });
    out.push(("alternating", old, new));
    let old = image(|b| noise_block(b));
    let new = image(|b| noise_block((b + 1024) % BLOCKS));
    out.push(("rotated", old, new));

    let old = image(|b| noise_block(b));
    let new = image(|b| noise_block(b % 8));
    out.push(("duplicates", old, new));
    return out;
}

#[test]
#[ignore]
fn bench_report() {
    use std::time::Instant;
    let keys = keys();
    println!(
        "pattern      payload  metadata  ops (copy/frames/sparse/raw/zero)  gen-ms  apply-buf-ms  apply-str-ms  src-reads  writes  decode-model"
    );
    for (name, old, new) in bench_patterns() {
        let t0 = Instant::now();
        let payload = build_payload_with(
            &old,
            &new,
            b"A/B",
            PRIVATE,
            BS,
            &EMBEDDED,
            &SearchConfig::quick(),
        )
        .map_err(|e| e.msg)
        .unwrap();
        let gen_ms = t0.elapsed().as_millis();
        let parsed = delta_update_payload::layout::parse_unverified(&payload, &EMBEDDED)
            .map_err(|e| e.msg)
            .unwrap();
        let info = &parsed.parsed().info;
        let metadata = info.blobs_start as usize;
        let ops = &parsed.parsed().manifest.partitions[0].operations;
        let count = |t: i32| ops.iter().filter(|op| op.r#type == t).count();
        let mix = format!(
            "{}/{}/{}/{}/{}",
            count(delta_update_payload::manifest::TYPE_SOURCECOPY),
            count(delta_update_payload::manifest::TYPE_REPLACE_FRAMES),
            count(delta_update_payload::manifest::TYPE_REPLACE_SPARSE),
            count(delta_update_payload::manifest::TYPE_REPLACE),
            count(delta_update_payload::manifest::TYPE_ZERO)
        );
        let decode_model = match delta_update_payload::prescan::scan_manifest(
            &payload[delta_update_payload::layout::HEADER_SIZE..metadata],
            &EMBEDDED,
        ) {
            Err(e) => panic!("{}: the bench manifest is refused: {}", name, e.msg),
            Ok(u) => u.accounted(),
        };

        let image = CountingOld {
            data: old.clone(),
            reads: std::cell::Cell::new(0),
        };
        let mut dev = Disk {
            data: old.clone(),
            writes: 0,
        };
        let t1 = Instant::now();
        let prepared = prepare(&payload, Trust::Verified(&keys), &EMBEDDED)
            .map_err(|e| e.msg)
            .unwrap();
        prepared
            .apply_to_device_with_image(
                &mut dev,
                &image,
                &mut MemoryStore::new(),
                &TARGET,
                Policy::Development,
                &Quiet {},
            )
            .map_err(|e| e.msg)
            .unwrap();
        let buf_ms = t1.elapsed().as_millis();
        assert!(dev.data == new, "{}: buffered path diverged", name);
        let reads = image.reads.get();
        let writes = dev.writes;

        let image = CountingOld {
            data: old.clone(),
            reads: std::cell::Cell::new(0),
        };
        let mut dev = Disk {
            data: old.clone(),
            writes: 0,
        };
        let t2 = Instant::now();
        let src = Pieces {
            data: payload.clone(),
            pos: 0,
        };
        let prepared = stream::prepare(src, Trust::Verified(&keys), &EMBEDDED)
            .map_err(|e| e.msg)
            .unwrap();
        prepared
            .apply_with_image(
                |off| Pieces {
                    data: payload.clone(),
                    pos: off as usize,
                },
                &mut dev,
                &image,
                &mut MemoryStore::new(),
                &TARGET,
                Policy::Development,
                &Quiet {},
            )
            .map_err(|e| e.msg)
            .unwrap();
        let str_ms = t2.elapsed().as_millis();
        assert!(dev.data == new, "{}: streamed path diverged", name);

        println!(
            "{:<12} {:>7}  {:>8}  {:<32}  {:>6}  {:>12}  {:>12}  {:>9}  {:>6}  {:>12}",
            name,
            payload.len(),
            metadata,
            mix,
            gen_ms,
            buf_ms,
            str_ms,
            reads,
            writes,
            decode_model
        );
    }
}
