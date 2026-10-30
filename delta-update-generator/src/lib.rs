//! `std` OTA delta generator: the host-side counterpart to the `no_std` client in
//! `delta-update-client`. Given an old and a new partition image it produces a signed `CrAU`
//! (major version 2) payload that `delta-update-client` can verify and apply. Given only the new
//! image, [`build_full_payload`] produces a self-contained payload that writes the whole
//! partition without an old image (`full`, and `docs/full-image.md`).
//!
//! The images are byte addresses. Their length is arbitrary, and nothing about an
//! operation is aligned: an operation may write any byte ranges, copy from any byte
//! offset, and carry partial final chunks. `block_size` is only the codec chunk size
//! that frames are cut at. The format is in `docs/format.md`, and the search is in
//! `docs/generator.md`.
//!
//! The generator searches over ordered programs of operations (`program`). The search is a
//! genetic algorithm that runs entirely on the GPU (`gpu`, `search`, and the shaders): both images
//! are uploaded once, and the population, the mutation, the replay on the memory model, the repair
//! and the exact **uncompressed** payload size all stay on the device. The host reads back a
//! small archive of the best programs when the search ends.
//!
//! The CPU then chooses encodings (`compress`): for each finalist, and for the whole-image raw
//! replacement, it tries `REPLACE_FRAMES` and `REPLACE_SPARSE` per operation and keeps a change only
//! when the whole payload shrinks and the profile admits it. A program is accepted only if replaying
//! it on the memory model (`memory`, `mapper`) reproduces the new image exactly and its manifest
//! passes the profile (`evaluate`). The output is then signed and checked by the same rules the
//! client applies (`check`).
//!
//! The payload byte layout is identical to what `delta_update_payload::layout::frame` consumes:
//!
//! ```text
//!   "CrAU" | u64 major=2 | u64 manifest_size | u32 metadata_sig_size
//!   | manifest proto
//!   | metadata Signatures message          (fixed size, key-size padded)
//!   | blobs                                   (data, in operation order)
//!   | payload Signatures message            (offset/size recorded in manifest)
//! ```

use std::str;
use std::time::Instant;

use delta_update_payload::error::Code;
use delta_update_payload::error::PayloadError;
use delta_update_payload::error::err;
use delta_update_payload::layout;
use delta_update_payload::manifest::Manifest;
use delta_update_payload::manifest::Operation;
use delta_update_payload::manifest::PartInfo;
use delta_update_payload::manifest::PartitionUpdate;
use delta_update_payload::profile::ResourceProfile;
use delta_update_payload::rsa;
use delta_update_payload::sha256;
use delta_update_payload::sha256::SHA256_SIZE;
use delta_update_payload::sign;
use prost::Message;

pub mod builder;
pub mod check;
pub mod compress;
pub mod control;
pub mod evaluate;
pub mod full;
pub mod gpu;
pub mod gpu_layout;
pub mod mapper;
pub mod memory;
pub mod program;
pub mod search;

#[cfg(test)]
mod verification;

pub use control::Stop;
pub use gpu::DevicePreference;
pub use gpu::SessionStats;
pub use search::SearchConfig;

use control::Control;
use control::Halt;
use evaluate::Problem;
use program::Op;
use program::Program;

pub const DEFAULT_BLOCK_SIZE: usize = 4096;

/// A built payload and how the search that chose it ended.
///
/// The device scores programs by their **raw** size, with every data operation stored uncompressed,
/// and the CPU then chooses encodings, so the two sizes are reported apart: [`raw_fitness`](Self::raw_fitness)
/// is what the search optimized, and the payload's own length is what was written.
#[derive(Debug)]
pub struct Built {
    pub payload: Vec<u8>,
    /// Why the search stopped: a clock, stagnation, the generation cap, or equal images.
    pub stop: Stop,
    /// Generations the search ran on the device.
    pub generations: u64,
    /// The best raw payload size the device scored, before any encoding was chosen. `None` when
    /// no device search ran: a zero budget, or equal images.
    pub raw_fitness: Option<u64>,
    /// The smallest raw size among the programs the CPU admitted, finalists and baselines alike.
    pub raw_admitted: Option<u64>,
    /// Programs the CPU considered: the device's finalists and the baselines.
    pub candidates: usize,
    /// Programs the encoding pass evaluated, trials included.
    pub trials: usize,
    /// Whole-program replays the encoding pass made, which is what a trial costs.
    pub encoding_mappings: u64,
    /// The device that searched, if one did.
    pub device: Option<String>,
    /// Candidate programs the device evaluated, refused ones included.
    pub evaluated: u64,
    /// Seconds the device spent making the images resident and evaluating the first population,
    /// seconds it spent in generations, seconds the CPU spent choosing encodings, and seconds it spent
    /// building, signing and checking the payload.
    pub upload_seconds: f64,
    pub search_seconds: f64,
    pub compress_seconds: f64,
    pub finalize_seconds: f64,
    /// What the device search allocated and moved.
    pub gpu: SessionStats,
}

/// Build a signed `CrAU` major-2 payload for `old` -> `new`, signed with the RSA
/// private key in `priv_pem` (PEM bytes), using the default search configuration.
pub fn build_payload(
    old: &[u8],
    new: &[u8],
    partition_name: &[u8],
    priv_pem: &[u8],
    block_size: usize,
    profile: &ResourceProfile,
) -> Result<Vec<u8>, PayloadError> {
    return build_payload_with(
        old,
        new,
        partition_name,
        priv_pem,
        block_size,
        profile,
        &SearchConfig::default(),
    );
}

/// [`build_payload`] with an explicit search configuration: the stop criteria, the
/// random seed, and the device to search on.
///
/// The configuration, the key and its signature size under the profile, and the inputs are
/// all checked before the search clock starts. Final signing and verification come after the
/// search and are not counted against its time limit.
pub fn build_payload_with(
    old: &[u8],
    new: &[u8],
    partition_name: &[u8],
    priv_pem: &[u8],
    block_size: usize,
    profile: &ResourceProfile,
    config: &SearchConfig,
) -> Result<Vec<u8>, PayloadError> {
    return match build_report(
        old,
        new,
        partition_name,
        priv_pem,
        block_size,
        profile,
        config,
    ) {
        Err(e) => Err(e),
        Ok(built) => Ok(built.payload),
    };
}

/// [`build_payload_with`], also reporting why the search stopped.
pub fn build_report(
    old: &[u8],
    new: &[u8],
    partition_name: &[u8],
    priv_pem: &[u8],
    block_size: usize,
    profile: &ResourceProfile,
    config: &SearchConfig,
) -> Result<Built, PayloadError> {
    let name = match checked_name(partition_name) {
        Err(e) => return Err(e),
        Ok(s) => s,
    };
    match checked_block_size(block_size, profile) {
        Err(e) => return Err(e),
        Ok(()) => {}
    };

    if let Err(e) = config.validate() {
        return Err(e);
    }
    let key = match rsa::parse_privkey_pem(priv_pem) {
        Err(e) => return Err(e),
        Ok(k) => k,
    };
    let problem = match Problem::new(old, new, block_size, &name, profile, key.size) {
        Err(e) => return Err(e),
        Ok(mut p) => {
            p.release_version = config.release_version;
            p
        }
    };
    let found = match search::run(&problem, config) {
        Err(e) => return Err(e),
        Ok(f) => f,
    };
    let candidates = candidate_programs(&problem, &found);
    let considered = candidates.len();
    let compressing = Instant::now();
    let outcome = match compress::optimize(&problem, candidates, &Control::unbounded()) {
        Err(Halt::Failed(e)) => return Err(e),
        Err(Halt::Deadline) => {
            return Err(err(
                Code::resource_limit,
                "the encoding pass was interrupted",
            ));
        }
        Ok(None) => return Err(err(Code::resource_limit, NO_CANDIDATE)),
        Ok(Some(o)) => o,
    };
    let compress_seconds = compressing.elapsed().as_secs_f64();
    let finalizing = Instant::now();
    let payload = match assemble(&problem, &outcome.best.program, &key, old, new) {
        Err(e) => return Err(e),
        Ok(v) => v,
    };
    let finalize_seconds = finalizing.elapsed().as_secs_f64();
    return Ok(Built {
        payload: payload,
        stop: found.stop,
        generations: found.generations,
        raw_fitness: found.finalists.first().map(|f| f.raw_size),
        raw_admitted: outcome.raw_best,
        candidates: considered,
        trials: outcome.trials,
        encoding_mappings: outcome.mappings,
        device: found.device,
        evaluated: found.counters.evaluated as u64,
        upload_seconds: found.upload_time.as_secs_f64(),
        search_seconds: found.search_time.as_secs_f64(),
        compress_seconds: compress_seconds,
        finalize_seconds: finalize_seconds,
        gpu: found.stats,
    });
}

const NO_CANDIDATE: &str = "no admissible candidate found within the budget";

fn candidate_programs(problem: &Problem, found: &search::Found) -> Vec<Program> {
    let mut out: Vec<Program> = Vec::new();
    if found.stop == Stop::Unchanged {
        out.push(Vec::new());
        return out;
    }
    let baseline = program::raw_baseline(problem.mapping.images().new.len() as u64);
    let mut finalists = found.finalists.iter().map(|f| f.program.clone());
    if let Some(best) = finalists.next() {
        out.push(best);
    }
    out.push(baseline);
    out.extend(finalists);
    if found.device.is_some() {
        out.push(Vec::new());
    }
    return out;
}

/// Build a signed, self-contained `CrAU` major-2 payload that writes the whole partition image `new`.
///
/// The payload records no old image and writes the new-image prefix of any sufficiently large target.
/// It needs no search and no device: the smaller admissible of a raw and a compressed candidate is signed,
/// and the output is checked by the full-image rules before it is returned. See [`full`] and
/// `docs/full-image.md`.
pub fn build_full_payload(
    new: &[u8],
    partition_name: &[u8],
    priv_pem: &[u8],
    block_size: usize,
    profile: &ResourceProfile,
) -> Result<Vec<u8>, PayloadError> {
    return build_full_payload_with_release(new, partition_name, priv_pem, block_size, profile, 0);
}

/// [`build_full_payload`] with the signed release version the payload records. Zero is what
/// [`build_full_payload`] records.
pub fn build_full_payload_with_release(
    new: &[u8],
    partition_name: &[u8],
    priv_pem: &[u8],
    block_size: usize,
    profile: &ResourceProfile,
    release_version: u64,
) -> Result<Vec<u8>, PayloadError> {
    let name = match checked_name(partition_name) {
        Err(e) => return Err(e),
        Ok(s) => s,
    };
    let key = match rsa::parse_privkey_pem(priv_pem) {
        Err(e) => return Err(e),
        Ok(k) => k,
    };
    return full::build(new, &name, &key, block_size, profile, release_version);
}

fn checked_name(partition_name: &[u8]) -> Result<String, PayloadError> {
    let name = match str::from_utf8(partition_name) {
        Err(_) => return Err(err(Code::parse, "partition name must be valid UTF-8")),
        Ok(s) => s.to_string(),
    };
    if name.is_empty() {
        return Err(err(
            Code::required_field_missing,
            "partition name must not be empty",
        ));
    }
    return Ok(name);
}

fn checked_block_size(block_size: usize, profile: &ResourceProfile) -> Result<(), PayloadError> {
    if block_size == 0 || block_size > u32::MAX as usize {
        return Err(err(
            Code::unsupported_block_size,
            "block size must be positive and fit u32",
        ));
    }
    if (block_size as u64) > (profile.max_block_size as u64) {
        return Err(err(
            Code::unsupported_block_size,
            "block size exceeds the resource profile",
        ));
    }
    return Ok(());
}

pub(crate) fn assemble(
    problem: &Problem,
    program: &[Op],
    key: &rsa::PrivKey,
    old: &[u8],
    new: &[u8],
) -> Result<Vec<u8>, PayloadError> {
    let mapped = match problem.mapping.map(program, &Control::unbounded(), true) {
        Err(Halt::Failed(e)) => return Err(e),
        Err(Halt::Deadline) => {
            return Err(err(Code::resource_limit, "final assembly was interrupted"));
        }
        Ok(m) => m,
    };
    if !mapped.mismatches.is_empty() {
        return Err(err(
            Code::blob_mismatch,
            "the search returned a program that does not produce the new image",
        ));
    }
    let blob_len = mapped.blob_len;
    let blobs = match mapped.blobs {
        None => Vec::new(),
        Some(b) => b,
    };
    let block_size = problem.mapping.chunk();
    let manifest = manifest_of(
        problem.name,
        mapped.ops,
        Some(part_info(old)),
        part_info(new),
        block_size,
        blob_len,
        problem.sig_msg_size,
        problem.release_version,
    )
    .encode_to_vec();
    let out = match seal(&manifest, &blobs, key, problem.sig_msg_size) {
        Err(e) => return Err(e),
        Ok(v) => v,
    };
    let public = rsa::public_key_of(key);
    match check::check_payload(&out, public, old, new, problem.profile) {
        Err(e) => return Err(e),
        Ok(()) => {}
    };
    return Ok(out);
}

pub(crate) fn manifest_of(
    name: &str,
    ops: Vec<Operation>,
    old: Option<PartInfo>,
    new: PartInfo,
    block_size: usize,
    blob_len: u64,
    sig_msg_size: usize,
    release_version: u64,
) -> Manifest {
    return Manifest {
        block_size: Some(block_size as u32),
        signatures_offset: Some(blob_len),
        signatures_size: Some(sig_msg_size as u64),
        minor_version: Some(layout::PROJECT_MINOR),
        release_version: Some(release_version),
        partitions: vec![PartitionUpdate {
            partition_name: name.to_string(),
            old_partition_info: old,
            new_partition_info: Some(new),
            operations: ops,
            ..Default::default()
        }],
        ..Default::default()
    };
}

pub(crate) fn part_info(img: &[u8]) -> PartInfo {
    let digest = sha256::hash_bytes(img);
    return PartInfo {
        size: Some(img.len() as u64),
        hash: Some(digest[0..SHA256_SIZE].to_vec()),
    };
}

/// Frame a manifest and its blob region into a payload and sign both signed ranges
/// with `key`. `manifest` must already declare the signature fields for
/// `sig_msg_size`, which is the padded size of one signature message under this key.
pub fn seal(
    manifest: &[u8],
    blobs: &[u8],
    key: &rsa::PrivKey,
    sig_msg_size: usize,
) -> Result<Vec<u8>, PayloadError> {
    let mut hdr: Vec<u8> = Vec::new();
    hdr.extend(layout::MAGIC);
    hdr.extend(layout::MAJOR_VERSION.to_be_bytes());
    hdr.extend((manifest.len() as u64).to_be_bytes());
    hdr.extend((sig_msg_size as u32).to_be_bytes());

    let mut h1 = sha256::new_hash();
    sha256::update(&mut h1, &hdr);
    sha256::update(&mut h1, manifest);
    let metadata_hash = sha256::finish(h1);

    let mut h2 = sha256::new_hash();
    sha256::update(&mut h2, &hdr);
    sha256::update(&mut h2, manifest);
    sha256::update(&mut h2, blobs);
    let payload_hash = sha256::finish(h2);

    let meta_sig = match rsa::rsa_sign(key, &metadata_hash) {
        Err(e) => return Err(e),
        Ok(s) => s,
    };
    let pay_sig = match rsa::rsa_sign(key, &payload_hash) {
        Err(e) => return Err(e),
        Ok(s) => s,
    };

    let meta_msg = match sign::build_signature_message(key.size, &meta_sig.bytes[..meta_sig.len]) {
        Err(e) => return Err(e),
        Ok(m) => m,
    };
    let pay_msg = match sign::build_signature_message(key.size, &pay_sig.bytes[..pay_sig.len]) {
        Err(e) => return Err(e),
        Ok(m) => m,
    };
    if meta_msg.len() != sig_msg_size || pay_msg.len() != sig_msg_size {
        return Err(err(
            Code::signature_mismatch,
            "signature message size drift",
        ));
    }

    let mut out: Vec<u8> = Vec::new();
    out.reserve_exact(layout::HEADER_SIZE + manifest.len() + 2 * sig_msg_size + blobs.len());
    out.extend(hdr);
    out.extend_from_slice(manifest);
    out.extend(meta_msg);
    out.extend_from_slice(blobs);
    out.extend(pay_msg);
    return Ok(out);
}

#[cfg(test)]
mod tests {
    use super::*;

    use delta_update_payload::profile::EMBEDDED;
    use delta_update_payload::profile::HOST;
    use delta_update_payload::rsa::PubKey;
    use delta_update_payload::rsa::parse_pubkey_pem;
    use delta_update_payload::verify::verify_payload;

    const PRIV: &[u8] = include_bytes!("../../delta-update-payload/tests/fixtures/private.pem");
    const PUB: &[u8] = include_bytes!("../../delta-update-payload/tests/fixtures/public.pem");
    const PRIV_4096: &[u8] =
        include_bytes!("../../delta-update-payload/tests/fixtures/private4096.pem");

    const SIG_2048: usize = 267;
    const SIG_4096: usize = 523;

    fn success<T>(result: Result<T, PayloadError>) -> T {
        return result.unwrap_or_else(|error| panic!("{}", error.msg));
    }

    fn fast() -> SearchConfig {
        return SearchConfig {
            max_search_seconds: 2.0,
            stagnation_seconds: 0.5,
            ..SearchConfig::default()
        };
    }

    fn public_key() -> PubKey {
        return success(parse_pubkey_pem(PUB));
    }

    #[test]
    fn build_payload_rejects_bad_name_or_key() {
        let old = vec![0u8; 16];
        let new = vec![1u8; 16];
        let empty = build_payload_with(&old, &new, b"", PRIV, 4, &EMBEDDED, &fast());
        assert_eq!(
            empty.err().map(|e| e.code),
            Some(Code::required_field_missing)
        );
        let bad_key = build_payload_with(&old, &new, b"A/B", b"not a key", 4, &EMBEDDED, &fast());
        assert!(bad_key.is_err());
    }

    #[test]
    fn bad_block_sizes_are_refused_before_searching() {
        let old = vec![0u8; 16];
        let zero = build_payload_with(&old, &old, b"A/B", PRIV, 0, &EMBEDDED, &fast());
        assert_eq!(
            zero.err().map(|e| e.code),
            Some(Code::unsupported_block_size)
        );
        let big = build_payload_with(&old, &old, b"A/B", PRIV, 8192, &EMBEDDED, &fast());
        assert_eq!(
            big.err().map(|e| e.code),
            Some(Code::unsupported_block_size)
        );
    }

    #[test]
    fn key_size_drives_signature_slot_sizes() {
        assert_eq!(sign::signature_message_size(256), SIG_2048);
        assert_eq!(sign::signature_message_size(512), SIG_4096);
        let old = vec![0u8; 32];
        let mut new = old.clone();
        new[3..9].fill(5);
        let payload = success(build_payload_with(
            &old,
            &new,
            b"A/B",
            PRIV_4096,
            8,
            &HOST,
            &fast(),
        ));
        let frame = success(layout::frame(&payload, &HOST));
        assert_eq!(frame.header.meta_sig_size as usize, SIG_4096);
    }

    #[test]
    fn a_signed_payload_verifies_and_replays_to_the_new_image() {
        let old: Vec<u8> = (0..200u32).map(|k| (k * 7 % 251) as u8).collect();
        let mut new = old.clone();
        new[13..61].fill(0);
        new[100..117].copy_from_slice(&old[3..20]);
        let payload = success(build_payload_with(
            &old,
            &new,
            b"A/B",
            PRIV,
            16,
            &EMBEDDED,
            &fast(),
        ));
        let authenticated = success(verify_payload(&payload, &[public_key()], &EMBEDDED));
        let parsed = authenticated.into_parsed();
        assert_eq!(parsed.manifest.minor_version, Some(layout::PROJECT_MINOR));
        success(check::check_payload(
            &payload,
            public_key(),
            &old,
            &new,
            &EMBEDDED,
        ));
    }

    #[test]
    fn identical_images_need_no_operations() {
        let old: Vec<u8> = (0..64u8).collect();
        let payload = success(build_payload_with(
            &old,
            &old,
            b"A/B",
            PRIV,
            8,
            &EMBEDDED,
            &fast(),
        ));
        let parsed = success(layout::parse_unverified(&payload, &EMBEDDED)).into_parsed();
        assert!(parsed.manifest.partitions[0].operations.is_empty());
    }

    #[test]
    fn an_empty_image_is_a_valid_empty_payload() {
        let payload = success(build_payload_with(
            &[],
            &[],
            b"A/B",
            PRIV,
            8,
            &EMBEDDED,
            &fast(),
        ));
        let parsed = success(layout::parse_unverified(&payload, &EMBEDDED)).into_parsed();
        assert!(parsed.manifest.partitions[0].operations.is_empty());
    }

    #[test]
    fn an_odd_length_image_with_a_partial_final_chunk_round_trips() {
        let old: Vec<u8> = (0..73u8).map(|k| k.wrapping_mul(3)).collect();
        let mut new = old.clone();
        new[40..73].fill(0x11);
        let payload = success(build_payload_with(
            &old,
            &new,
            b"A/B",
            PRIV,
            16,
            &EMBEDDED,
            &fast(),
        ));
        success(check::check_payload(
            &payload,
            public_key(),
            &old,
            &new,
            &EMBEDDED,
        ));
    }
}
