//! The apply engine, shared by the buffered and the streamed pipelines.
//!
//! Both pipelines authenticate and validate metadata in `prepare`, then hand the
//! result here. This module decides where the payload stream must resume, runs
//! every uncommitted operation, and records progress. The rules it follows are
//! the ones `docs/client.md` §6 and `docs/stream.md` state:
//!
//! ```text
//!   strict policy: authenticated, signed size and hash, readable target,
//!                  release at or above the floor, base present if needed
//!   bind capacity -> validate old image -> resume from checkpoint (no writes)
//!   fresh only:   preflight hash (base-dependent only) -> persist Initialized
//!   per operation, from next_op:
//!     execute (chunks are written as they are decoded)
//!     verify the operation's data hash
//!     flush -> persist the checkpoint -> report
//!   persist AwaitingFinal -> read and verify the payload signature
//!   require end of stream -> flush -> postflight hash
//!       match:    persist Complete, report Verified (or Applied, if unreadable)
//!       mismatch: persist Mismatch, refuse with recovery_required on resume
//!   resumed at Complete:  verify the signature and end of stream -> postflight
//!                         hash, with no write and no record change
//! ```
//!
//! An operation's destination bytes are the logical stream of its extents, in
//! declared order, cut into chunks of the manifest's block size. A chunk is
//! written to every extent piece it covers, in order, so the last write to any
//! byte is the one from the last extent that covers it.

use crate::checkpoint::CheckpointStore;
use crate::checkpoint::MetadataDigest;
use crate::checkpoint::Phase;
use crate::checkpoint::RECORD_SIZE;
use crate::checkpoint::Record;
use crate::checkpoint::TargetId;
use crate::image::OldImage;
use crate::image::missing_image;
use crate::policy::Outcome;
use crate::policy::Policy;
use crate::policy::ReleaseFloor;
use crate::sink::Sink;
use crate::sink::check_range;
use crate::source::Source;
use crate::source::expect_end;
use crate::source::read_full;
use alloc::vec::Vec;
use ota_payload::encoding::BlockDecoder;
use ota_payload::encoding::Mode;
use ota_payload::encoding::Varints;
use ota_payload::encoding::chunk_count;
use ota_payload::encoding::chunk_len;
use ota_payload::encoding::map_does_not_cover;
use ota_payload::encoding::mode_at;
use ota_payload::encoding::split_sparse;
use ota_payload::error::Code;
use ota_payload::error::PayloadError;
use ota_payload::error::err;
use ota_payload::manifest::Extent;
use ota_payload::manifest::Operation;
use ota_payload::manifest::PartInfo;
use ota_payload::manifest::TYPE_REPLACE;
use ota_payload::manifest::TYPE_REPLACE_FRAMES;
use ota_payload::manifest::TYPE_REPLACE_SPARSE;
use ota_payload::manifest::TYPE_SOURCECOPY;
use ota_payload::manifest::TYPE_TARGET_COPY;
use ota_payload::manifest::TYPE_ZERO;
use ota_payload::plan::Contract;
use ota_payload::plan::Geometry;
use ota_payload::plan::Validated;
use ota_payload::profile::ResourceProfile;
use ota_payload::rsa::PubKey;
use ota_payload::sha256;
use ota_payload::verify::verify_message;

/// How a payload's metadata is trusted before it is applied.
#[derive(Clone, Copy)]
pub enum Trust<'a> {
    /// Both embedded signatures must verify against at least one of these keys.
    Verified(&'a [PubKey]),
    /// No signature checks. Structural and policy checks still run.
    Unverified,
}

/// Called once per applied operation so hosts can report progress.
pub trait Progress {
    fn report(self: &Self, done_ops: usize, total_ops: usize);
}

/// The do-nothing progress callback.
pub struct NoProgress {}

impl Progress for NoProgress {
    fn report(self: &Self, done_ops: usize, total_ops: usize) {
        let _ = done_ops;
        let _ = total_ops;
    }
}

/// The read capability of a device, as a plain function. The pipelines pass
/// `Some(<D as Readable>::read_at)` only where `D: Readable` is known, so a
/// write-only sink passes `None`, and any operation that needs a read is refused
/// before the first write.
pub type ReadFn<D> = fn(&D, u64, &mut [u8]) -> Result<(), PayloadError>;

/// A payload whose metadata is authenticated (per the trust it was prepared under)
/// and validated against the profile. Nothing has been written to storage.
pub struct Prepared<'a> {
    validated: Validated,
    keys: &'a [PubKey],



    verified: bool,
    payload_sig_deferred: bool,
    profile: ResourceProfile,
    digest: MetadataDigest,


    payload_hash: sha256::Sha256,


    blobs_start: u64,
}

impl<'a> Prepared<'a> {
    /// The logical partition label for adapter routing, not a path or slot selector.
    /// It is authenticated only when prepared with [`Trust::Verified`].
    pub fn partition_name(&self) -> &str {
        return &self.validated.manifest().partitions[0].partition_name;
    }

    /// The validated geometry: new-image size, chunk size, operations and reach.
    pub fn geometry(&self) -> &Geometry {
        return self.validated.geometry();
    }

    /// The payload offset at which a fresh update's first blob begins.
    pub fn blobs_start(&self) -> u64 {
        return self.blobs_start;
    }

    /// Digest of the authenticated header and manifest, the update's identity in
    /// a checkpoint.
    pub fn metadata_digest(&self) -> &MetadataDigest {
        return &self.digest;
    }

    /// The signed release version, when the payload carries one. Authenticated only
    /// when prepared with [`Trust::Verified`](crate::engine::Trust).
    pub fn release_version(&self) -> Option<u64> {
        return self.validated.manifest().release_version;
    }

    /// What the destination must hold before the first write: the old image, for a
    /// base-dependent program, or nothing, for a destination-independent one. Derived
    /// from the operations, so a host decides recovery from it rather than from the
    /// payload's word (`docs/client.md` §11).
    pub fn contract(&self) -> Contract {
        return self.validated.contract();
    }
}





pub(crate) fn assemble<'a>(
    head: &[u8],
    manifest_bytes: &[u8],
    meta_sig_len: usize,
    validated: Validated,
    keys: &'a [PubKey],
    verified: bool,
    payload_sig_deferred: bool,
    profile: &ResourceProfile,
) -> Prepared<'a> {
    let mut md = sha256::new_hash();
    sha256::update(&mut md, head);
    sha256::update(&mut md, manifest_bytes);
    let mut payload_hash = sha256::new_hash();
    sha256::update(&mut payload_hash, head);
    sha256::update(&mut payload_hash, manifest_bytes);
    return Prepared {
        validated: validated,
        keys: keys,
        verified: verified,
        payload_sig_deferred: payload_sig_deferred,
        profile: *profile,
        digest: sha256::finish(md),
        payload_hash: payload_hash,
        blobs_start: (head.len() + manifest_bytes.len() + meta_sig_len) as u64,
    };
}



pub(crate) fn authenticate<'a>(
    head: &[u8],
    manifest_bytes: &[u8],
    meta_sig: &[u8],
    trust: Trust<'a>,
    profile: &ResourceProfile,
) -> Result<Prepared<'a>, PayloadError> {
    let verified = matches!(trust, Trust::Verified(_));
    let keys: &'a [PubKey] = match trust {
        Trust::Verified(k) => k,
        Trust::Unverified => &[],
    };
    if verified {
        if meta_sig.is_empty() {
            return Err(err(
                Code::signature_missing,
                "both embedded signatures are required",
            ));
        }

        let mut md = sha256::new_hash();
        sha256::update(&mut md, head);
        sha256::update(&mut md, manifest_bytes);
        let digest = sha256::finish(md);
        match verify_message(meta_sig, &digest, keys, profile) {
            Err(e) => return Err(e),
            Ok(()) => {}
        };
    }
    let manifest = match ota_payload::manifest::decode_manifest(manifest_bytes, profile) {
        Err(e) => return Err(e),
        Ok(v) => v,
    };
    let validated = match ota_payload::plan::validate(manifest, profile) {
        Err(e) => return Err(e),
        Ok(v) => v,
    };
    if verified {


        let m = validated.manifest();
        let has_sig = m.signatures_offset.is_some() && m.signatures_size.is_some();
        if !has_sig || m.signatures_size.unwrap_or(0) == 0 {
            return Err(err(
                Code::signature_missing,
                "both embedded signatures are required",
            ));
        }
    }
    return Ok(assemble(
        head,
        manifest_bytes,
        meta_sig.len(),
        validated,
        keys,
        verified,
        true,
        profile,
    ));
}



struct Resume {
    next_op: usize,
    offset: u64,
    hash: sha256::Sha256,
    phase: Phase,
    fresh: bool,
}







fn resume_point<C: CheckpointStore>(
    p: &Prepared,
    store: &mut C,
    target: &TargetId,
) -> Result<Resume, PayloadError> {
    let mut bytes = [0u8; RECORD_SIZE];
    let found = match store.load(&mut bytes) {
        Err(e) => return Err(e),
        Ok(f) => f,
    };
    if !found {
        return Ok(Resume {
            next_op: 0,
            offset: p.blobs_start,
            hash: p.payload_hash.clone(),
            phase: Phase::Initialized,
            fresh: true,
        });
    }
    let rec = match Record::decode(&bytes) {
        Err(e) => return Err(e),
        Ok(r) => r,
    };
    if rec.metadata != p.digest {
        return Err(err(
            Code::checkpoint_mismatch,
            "checkpoint belongs to another payload",
        ));
    }
    if rec.target != *target {
        return Err(err(
            Code::checkpoint_mismatch,
            "checkpoint belongs to another target",
        ));
    }
    if rec.phase == Phase::Mismatch {
        return Err(err(
            Code::recovery_required,
            "the final check failed; restore the target and reset the checkpoint",
        ));
    }
    let ops = &p.validated.manifest().partitions[0].operations;
    let count = ops.len();
    let next = rec.next_op as usize;
    if next > count {
        return Err(err(
            Code::checkpoint_invalid,
            "checkpoint is past the last operation",
        ));
    }


    let blob_at = if next < count {
        op_offset(&ops[next])
    } else {
        p.validated.geometry().blob_total
    };
    if rec.payload_offset != p.blobs_start + blob_at {
        return Err(err(
            Code::checkpoint_invalid,
            "checkpoint offset does not match its operation",
        ));
    }
    let phase_ok = match rec.phase {
        Phase::Initialized => next == 0,
        Phase::Applying => next > 0 && next < count,
        Phase::AwaitingFinal | Phase::Complete => next == count,

        Phase::Mismatch => false,
    };
    if !phase_ok {
        return Err(err(
            Code::checkpoint_invalid,
            "checkpoint phase does not match its operation index",
        ));
    }
    let hash = match sha256::restore(&rec.hash) {
        None => {
            return Err(err(
                Code::checkpoint_invalid,
                "checkpoint hash state is invalid",
            ));
        }
        Some(h) => h,
    };
    match store.confirm_stored() {
        Err(e) => return Err(e),
        Ok(()) => {}
    };
    return Ok(Resume {
        next_op: next,
        offset: rec.payload_offset,
        hash: hash,
        phase: rec.phase,
        fresh: false,
    });
}





pub(crate) fn run<'a, S, D, I, C, P>(
    p: &Prepared<'a>,
    target: &TargetId,
    open: impl FnOnce(u64) -> S,
    dev: &mut D,
    reads: Option<ReadFn<D>>,
    image: Option<&I>,
    store: &mut C,
    policy: Policy,
    prog: &P,
) -> Result<Outcome, PayloadError>
where
    S: Source,
    D: Sink,
    I: OldImage,
    C: CheckpointStore,
    P: Progress,
{
    let g = *p.geometry();


    match require_readable_contract(p, reads.is_some()) {
        Err(e) => return Err(e),
        Ok(()) => {}
    };
    if let Policy::Strict(floor) = policy {
        match require_strict(p, reads.is_some(), floor) {
            Err(e) => return Err(e),
            Ok(()) => {}
        };
    }
    match bind(&g, dev.capacity()) {
        Err(e) => return Err(e),
        Ok(()) => {}
    };
    let mut bufs = match Buffers::new(&p.validated) {
        Err(e) => return Err(e),
        Ok(b) => b,
    };
    match validate_image(&p.validated, image, &mut bufs.chunk) {
        Err(e) => return Err(e),
        Ok(()) => {}
    };
    let mut resume = match resume_point(p, store, target) {
        Err(e) => return Err(e),
        Ok(r) => r,
    };
    if resume.phase == Phase::Complete {






        let src = open(resume.offset);
        match verify_stream_end(p, src, resume.hash) {
            Err(e) => return Err(e),
            Ok(()) => {}
        };
        let mut checked = false;
        if let Some(read) = reads {
            checked = match postflight(&p.validated, &*dev, read, &mut bufs.chunk) {
                Err(e) => return Err(e),
                Ok(v) => v,
            };
        }
        return Ok(outcome(p, checked));
    }



    match bufs.stage_frames(&p.validated, resume.next_op) {
        Err(e) => return Err(e),
        Ok(()) => {}
    };
    if resume.fresh {





        if let Some(read) = reads {
            if p.contract() == Contract::BaseDependent {
                match preflight(&p.validated, &*dev, read, &mut bufs.chunk) {
                    Err(e) => return Err(e),
                    Ok(()) => {}
                };
            }
        }
        let rec = initialized(p, target);
        match store.replace(&rec) {
            Err(e) => return Err(e),
            Ok(()) => {}
        };
    }

    let mut src = open(resume.offset);
    let ops = &p.validated.manifest().partitions[0].operations;
    let count = ops.len();
    let mut next = resume.next_op;
    while next < count {
        let op = &ops[next];
        match execute(
            op,
            g.chunk_size,
            &mut src,
            &mut resume.hash,
            dev,
            reads,
            image,
            &mut bufs,
        ) {
            Err(e) => return Err(e),
            Ok(()) => {}
        };

        match dev.flush() {
            Err(e) => return Err(e),
            Ok(()) => {}
        };
        next += 1;
        let phase = if next == count {
            Phase::AwaitingFinal
        } else {
            Phase::Applying
        };
        let offset = p.blobs_start + op_offset(op) + op_length(op);
        let rec = record(p, target, phase, next, offset, &resume.hash);
        match store.replace(&rec) {
            Err(e) => return Err(e),
            Ok(()) => {}
        };
        prog.report(next, count);
    }

    if count == 0 && resume.phase != Phase::AwaitingFinal {
        let rec = record(
            p,
            target,
            Phase::AwaitingFinal,
            0,
            p.blobs_start,
            &resume.hash,
        );
        match store.replace(&rec) {
            Err(e) => return Err(e),
            Ok(()) => {}
        };
    }


    bufs.codec = None;
    bufs.frame = Vec::new();

    return finish(
        p,
        target,
        src,
        resume.hash,
        dev,
        reads,
        store,
        &mut bufs.chunk,
    );
}



fn require_strict(p: &Prepared, readable: bool, floor: &dyn ReleaseFloor) -> Result<(), PayloadError> {
    if !p.verified {
        return Err(err(
            Code::signature_missing,
            "the strict policy requires authenticated metadata",
        ));
    }
    if !readable {
        return Err(err(
            Code::target_not_readable,
            "the strict policy requires a readable target",
        ));
    }
    let pu = &p.validated.manifest().partitions[0];
    if !has_size_and_hash(&pu.new_partition_info) {
        return Err(err(
            Code::required_field_missing,
            "the strict policy requires the signed new-image size and hash",
        ));
    }
    let release = match p.release_version() {
        None => {
            return Err(err(
                Code::required_field_missing,
                "the strict policy requires a release version",
            ));
        }
        Some(v) => v,
    };
    let minimum = match floor.minimum() {
        Err(e) => return Err(e),
        Ok(v) => v,
    };
    if release < minimum {
        return Err(err(
            Code::rollback_refused,
            "payload release is below the rollback floor",
        ));
    }
    if p.contract() == Contract::BaseDependent && !has_size_and_hash(&pu.old_partition_info) {
        return Err(err(
            Code::required_field_missing,
            "a base-dependent update needs the signed old-image size and hash",
        ));
    }
    return Ok(());
}





fn require_readable_contract(p: &Prepared, readable: bool) -> Result<(), PayloadError> {
    if readable {
        return Ok(());
    }
    if p.geometry().reads_target {
        return Err(err(
            Code::target_not_readable,
            "an in-target copy needs a readable target",
        ));
    }
    if p.contract() == Contract::BaseDependent {
        return Err(err(
            Code::target_not_readable,
            "a base-dependent update needs a readable target: some bytes of the new image are the destination's own, and a write-only sink cannot supply them. Apply it to a readable target, or use a payload whose operations write every byte of the new image",
        ));
    }
    return Ok(());
}


fn has_size_and_hash(info: &Option<PartInfo>) -> bool {
    return match info {
        None => false,
        Some(pi) => {
            pi.size.is_some()
                && match &pi.hash {
                    None => false,
                    Some(h) => h.len() == sha256::SHA256_SIZE,
                }
        }
    };
}



fn outcome(p: &Prepared, checked: bool) -> Outcome {
    if checked && p.verified {
        return Outcome::Verified;
    }
    return Outcome::Applied;
}

/// The progress a reset removed: the phase its record was in, and how many operations
/// it had committed. A host reports it, so an operator sees what was discarded.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Cleared {
    pub phase: Phase,
    pub committed: u32,
}

/// Clear a checkpoint so the next run starts this update from its first operation.
/// Returns what was removed, or `None` when there was no record to remove.
///
/// The record must belong to this payload and this target. A base-dependent update
/// restarts only from the old image: the destination is checked against the signed old
/// hash here, before the record changes, so a partly written target keeps its
/// checkpoint. A destination-independent update restarts from any contents, since every
/// byte it leaves is written again. The host must hold the target exclusively across
/// the reset and the restart (`docs/platform.md` §2). A removal the store cannot confirm durable
/// is `checkpoint_removal_uncertain`: the record may reappear after a crash (`docs/checkpoint.md` §8.1).
/// When no record is found, the reset asks the store to confirm that its absence is durable, so an
/// earlier uncertain removal is settled before a retry reports success.
pub fn reset<D: Sink, C: CheckpointStore>(
    p: &Prepared,
    target: &TargetId,
    dev: &D,
    reads: Option<ReadFn<D>>,
    store: &mut C,
) -> Result<Option<Cleared>, PayloadError> {
    let mut bytes = [0u8; RECORD_SIZE];
    let found = match store.load(&mut bytes) {
        Err(e) => return Err(e),
        Ok(f) => f,
    };
    if !found {


        return match store.confirm_absent() {
            Err(e) => Err(e),
            Ok(()) => Ok(None),
        };
    }
    let rec = match Record::decode(&bytes) {
        Err(e) => return Err(e),
        Ok(r) => r,
    };
    if rec.metadata != p.digest || rec.target != *target {
        return Err(err(
            Code::checkpoint_mismatch,
            "checkpoint belongs to another payload or target",
        ));
    }
    if p.contract() == Contract::BaseDependent {
        let read = match reads {
            None => {
                return Err(err(
                    Code::target_not_readable,
                    "resetting a base-dependent update needs a readable target",
                ));
            }
            Some(r) => r,
        };
        if !has_size_and_hash(&p.validated.manifest().partitions[0].old_partition_info) {
            return Err(err(
                Code::required_field_missing,
                "resetting a base-dependent update needs the signed old-image hash",
            ));
        }
        let mut scratch = match alloc_buffer(p.validated.geometry().chunk_size as usize) {
            Err(e) => return Err(e),
            Ok(v) => v,
        };
        match preflight(&p.validated, dev, read, &mut scratch) {
            Err(e) => return Err(e),
            Ok(()) => {}
        };
    }
    match store.clear() {
        Err(e) => return Err(e),
        Ok(()) => {}
    };
    return Ok(Some(Cleared {
        phase: rec.phase,
        committed: rec.next_op,
    }));
}



fn verify_stream_end<'a, S: Source>(
    p: &Prepared<'a>,
    mut src: S,
    hash: sha256::Sha256,
) -> Result<(), PayloadError> {
    let m = p.validated.manifest();
    let has_sig = m.signatures_offset.is_some() && m.signatures_size.is_some();
    if has_sig {
        let sgn = m.signatures_size.unwrap_or(0) as usize;
        let mut sig = match alloc_buffer(sgn) {
            Err(e) => return Err(e),
            Ok(v) => v,
        };
        match read_full(&mut src, &mut sig) {
            Err(e) => return Err(e),
            Ok(()) => {}
        };
        if p.verified && p.payload_sig_deferred {
            let digest = sha256::finish(hash);
            match verify_message(&sig, &digest, p.keys, &p.profile) {
                Err(e) => return Err(e),
                Ok(()) => {}
            };
        }
    }
    return expect_end(&mut src);
}



fn finish<'a, S, D, C>(
    p: &Prepared<'a>,
    target: &TargetId,
    src: S,
    hash: sha256::Sha256,
    dev: &mut D,
    reads: Option<ReadFn<D>>,
    store: &mut C,
    scratch: &mut [u8],
) -> Result<Outcome, PayloadError>
where
    S: Source,
    D: Sink,
    C: CheckpointStore,
{
    let count = p.validated.manifest().partitions[0].operations.len();
    let end = p.blobs_start + p.validated.geometry().blob_total;
    let snapshot_for_complete = sha256::snapshot(&hash);
    match verify_stream_end(p, src, hash) {
        Err(e) => return Err(e),
        Ok(()) => {}
    };
    match dev.flush() {
        Err(e) => return Err(e),
        Ok(()) => {}
    };
    let mut checked = false;
    if let Some(read) = reads {
        match postflight(&p.validated, &*dev, read, scratch) {
            Err(e) => {
                if e.code == Code::hash_mismatch {




                    let mismatch = Record {
                        phase: Phase::Mismatch,
                        next_op: count as u32,
                        payload_offset: end,
                        metadata: p.digest,
                        target: *target,
                        hash: snapshot_for_complete,
                    };
                    match store.replace(&mismatch.encode()) {
                        Err(_) => {}
                        Ok(()) => {}
                    };
                }
                return Err(e);
            }
            Ok(v) => checked = v,
        };
    }
    let rec = Record {
        phase: Phase::Complete,
        next_op: count as u32,
        payload_offset: end,
        metadata: p.digest,
        target: *target,
        hash: snapshot_for_complete,
    };
    match store.replace(&rec.encode()) {
        Err(e) => return Err(e),
        Ok(()) => {}
    };
    return Ok(outcome(p, checked));
}


fn record(
    p: &Prepared,
    target: &TargetId,
    phase: Phase,
    next_op: usize,
    offset: u64,
    hash: &sha256::Sha256,
) -> [u8; RECORD_SIZE] {
    return Record {
        phase: phase,
        next_op: next_op as u32,
        payload_offset: offset,
        metadata: p.digest,
        target: *target,
        hash: sha256::snapshot(hash),
    }
    .encode();
}


fn initialized(p: &Prepared, target: &TargetId) -> [u8; RECORD_SIZE] {
    return record(
        p,
        target,
        Phase::Initialized,
        0,
        p.blobs_start,
        &p.payload_hash,
    );
}



fn bind(g: &Geometry, capacity: u64) -> Result<(), PayloadError> {
    if let Some(size) = g.size {
        if size > capacity {
            return Err(err(
                Code::partition_mismatch,
                "storage cannot hold the new image",
            ));
        }
    }
    if g.end > capacity {
        return Err(err(
            Code::extent_out_of_range,
            "an operation addresses bytes outside the storage",
        ));
    }
    return Ok(());
}






fn validate_image<I: OldImage>(
    validated: &Validated,
    image: Option<&I>,
    scratch: &mut [u8],
) -> Result<(), PayloadError> {
    let g = validated.geometry();
    let image = match image {
        None if !g.needs_source => return Ok(()),
        None => return Err(missing_image()),
        Some(i) => i,
    };
    let pu = &validated.manifest().partitions[0];
    if let Some(old) = &pu.old_partition_info {
        if let Some(size) = old.size {
            if size != image.capacity() {
                return Err(err(
                    Code::source_geometry,
                    "old image size differs from the manifest",
                ));
            }
        }
    }
    let ops = &pu.operations;
    for op in ops {
        if op.r#type != TYPE_SOURCECOPY {
            continue;
        }
        if furthest(&op.src_extents) > image.capacity() {
            return Err(err(
                Code::extent_out_of_range,
                "source extent is outside the old image",
            ));
        }
    }
    if let Some(old) = &pu.old_partition_info {
        if let Some(want) = &old.hash {
            if !want.is_empty() {
                let mut h = sha256::new_hash();
                let mut at: u64 = 0;
                let cap = image.capacity();
                while at < cap {
                    let n = core::cmp::min(scratch.len() as u64, cap - at) as usize;
                    match image.read_at(at, &mut scratch[..n]) {
                        Err(e) => return Err(e),
                        Ok(()) => {}
                    };
                    sha256::update(&mut h, &scratch[..n]);
                    at += n as u64;
                }
                if !hash_eq(sha256::finish(h), want) {
                    return Err(err(
                        Code::source_mismatch,
                        "old image does not match the old partition hash",
                    ));
                }
            }
        }
    }
    for op in ops {
        if op.r#type != TYPE_SOURCECOPY {
            continue;
        }
        let want = match &op.src_sha256_hash {
            None => return Err(err(Code::source_mismatch, "SOURCE_COPY has no source hash")),
            Some(h) => h,
        };
        let mut h = sha256::new_hash();
        let mut walk = Walk::new(&op.src_extents);
        let total = total_bytes(&op.src_extents);
        let mut at: u64 = 0;
        while at < total {
            let n = core::cmp::min(scratch.len() as u64, total - at) as usize;
            match gather(&mut walk, at, &mut scratch[..n], |off, out| {
                match check_range(off, out.len(), image.capacity()) {
                    Err(e) => return Err(e),
                    Ok(()) => {}
                };
                return image.read_at(off, out);
            }) {
                Err(e) => return Err(e),
                Ok(()) => {}
            };
            sha256::update(&mut h, &scratch[..n]);
            at += n as u64;
        }
        if !hash_eq(sha256::finish(h), want) {
            return Err(err(
                Code::source_mismatch,
                "source copy does not match its source hash",
            ));
        }
    }
    return Ok(());
}



fn preflight<D: Sink>(
    validated: &Validated,
    dev: &D,
    read: ReadFn<D>,
    scratch: &mut [u8],
) -> Result<(), PayloadError> {
    let pu = &validated.manifest().partitions[0];
    let want = match &pu.old_partition_info {
        None => return Ok(()),
        Some(old) => match &old.hash {
            None => return Ok(()),
            Some(h) => h,
        },
    };
    if want.is_empty() {
        return Ok(());
    }
    let size = pu
        .old_partition_info
        .as_ref()
        .and_then(|old| old.size)
        .unwrap_or(dev.capacity());
    let got = match hash_device(dev, read, scratch, size) {
        Err(e) => return Err(e),
        Ok(v) => v,
    };
    if !hash_eq(got, want) {
        return Err(err(
            Code::hash_mismatch,
            "device does not match old partition hash",
        ));
    }
    return Ok(());
}





fn postflight<D: Sink>(
    validated: &Validated,
    dev: &D,
    read: ReadFn<D>,
    scratch: &mut [u8],
) -> Result<bool, PayloadError> {
    let pu = &validated.manifest().partitions[0];
    let want = match &pu.new_partition_info {
        None => return Ok(false),
        Some(new) => match &new.hash {
            None => return Ok(false),
            Some(h) => h,
        },
    };
    if want.is_empty() {
        return Ok(false);
    }
    let size = pu
        .new_partition_info
        .as_ref()
        .and_then(|new| new.size)
        .unwrap_or(dev.capacity());
    let got = match hash_device(dev, read, scratch, size) {
        Err(e) => return Err(e),
        Ok(v) => v,
    };
    if !hash_eq(got, want) {
        return Err(err(
            Code::hash_mismatch,
            "device does not match new partition hash",
        ));
    }
    return Ok(true);
}


fn hash_device<D: Sink>(
    dev: &D,
    read: ReadFn<D>,
    scratch: &mut [u8],
    cap: u64,
) -> Result<[u8; sha256::SHA256_SIZE], PayloadError> {
    if cap > dev.capacity() {
        return Err(err(
            Code::partition_mismatch,
            "storage cannot hold the image being hashed",
        ));
    }
    let mut h = sha256::new_hash();
    let mut at: u64 = 0;
    while at < cap {
        let n = core::cmp::min(scratch.len() as u64, cap - at) as usize;
        match read(dev, at, &mut scratch[..n]) {
            Err(e) => return Err(e),
            Ok(()) => {}
        };
        sha256::update(&mut h, &scratch[..n]);
        at += n as u64;
    }
    return Ok(sha256::finish(h));
}






struct Buffers {
    chunk: Vec<u8>,
    frame: Vec<u8>,
    codec: Option<BlockDecoder>,
}

impl Buffers {
    fn new(validated: &Validated) -> Result<Buffers, PayloadError> {
        let chunk = match alloc_buffer(validated.geometry().chunk_size as usize) {
            Err(e) => return Err(e),
            Ok(v) => v,
        };
        return Ok(Buffers {
            chunk: chunk,
            frame: Vec::new(),
            codec: None,
        });
    }




    fn stage_frames(&mut self, validated: &Validated, next: usize) -> Result<(), PayloadError> {
        let has_frames = validated.manifest().partitions[0]
            .operations
            .iter()
            .skip(next)
            .any(|op| op.r#type == TYPE_REPLACE_FRAMES || op.r#type == TYPE_REPLACE_SPARSE);
        if has_frames {
            self.frame = match alloc_buffer(validated.geometry().chunk_size as usize) {
                Err(e) => return Err(e),
                Ok(v) => v,
            };
        }
        return Ok(());
    }
}





fn execute<S: Source, D: Sink, I: OldImage>(
    op: &Operation,
    chunk: u64,
    src: &mut S,
    whole: &mut sha256::Sha256,
    dev: &mut D,
    reads: Option<ReadFn<D>>,
    image: Option<&I>,
    bufs: &mut Buffers,
) -> Result<(), PayloadError> {
    let total = total_bytes(&op.dst_extents);
    let count = match chunk_count(total, chunk) {
        None => return Err(err(Code::overflow, "operation chunk count overflows")),
        Some(n) => n,
    };
    let mut dst = Walk::new(&op.dst_extents);
    let mut blob = sha256::new_hash();
    let mut carries_blob = false;
    match op.r#type {
        TYPE_ZERO => {
            let mut i: u64 = 0;
            while i < count {
                let len = chunk_len(total, chunk, i) as usize;
                bufs.chunk[..len].fill(0);
                match scatter(dev, &mut dst, i * chunk, &bufs.chunk[..len]) {
                    Err(e) => return Err(e),
                    Ok(()) => {}
                };
                i += 1;
            }
        }
        TYPE_REPLACE => {
            carries_blob = true;
            let mut i: u64 = 0;
            while i < count {
                let len = chunk_len(total, chunk, i) as usize;
                match read_blob(src, &mut bufs.chunk[..len], whole, &mut blob) {
                    Err(e) => return Err(e),
                    Ok(()) => {}
                };
                match scatter(dev, &mut dst, i * chunk, &bufs.chunk[..len]) {
                    Err(e) => return Err(e),
                    Ok(()) => {}
                };
                i += 1;
            }
        }
        TYPE_REPLACE_FRAMES => {
            carries_blob = true;
            let table: &[u8] = match &op.frame_table {
                None => &[],
                Some(t) => &t[..],
            };
            let mut entries = Varints::new(table);
            let mut i: u64 = 0;
            while i < count {
                let len = chunk_len(total, chunk, i) as usize;
                let entry = match entries.next_entry() {
                    Err(e) => return Err(e),
                    Ok(v) => v,
                };
                let r = if entry == 0 {
                    read_blob(src, &mut bufs.chunk[..len], whole, &mut blob)
                } else {
                    read_frame(
                        src,
                        &mut bufs.frame[..(entry as usize)],
                        &mut bufs.chunk[..len],
                        whole,
                        &mut blob,
                        &mut bufs.codec,
                    )
                };
                match r {
                    Err(e) => return Err(e),
                    Ok(()) => {}
                };
                match scatter(dev, &mut dst, i * chunk, &bufs.chunk[..len]) {
                    Err(e) => return Err(e),
                    Ok(()) => {}
                };
                i += 1;
            }
        }
        TYPE_REPLACE_SPARSE => {
            carries_blob = true;
            let packed: &[u8] = match &op.frame_table {
                None => &[],
                Some(t) => &t[..],
            };
            let (map, table) = match split_sparse(packed, count) {
                Err(e) => return Err(e),
                Ok(v) => v,
            };
            let mut entries = Varints::new(table);
            let mut i: u64 = 0;
            while i < count {
                let len = chunk_len(total, chunk, i) as usize;
                let mode = match mode_at(map, i) {
                    None => return Err(map_does_not_cover()),
                    Some(m) => m,
                };
                match mode {

                    Mode::Keep => {
                        i += 1;
                        continue;
                    }
                    Mode::Zero => bufs.chunk[..len].fill(0),
                    Mode::Raw => {
                        match read_blob(src, &mut bufs.chunk[..len], whole, &mut blob) {
                            Err(e) => return Err(e),
                            Ok(()) => {}
                        };
                    }
                    Mode::Zlib => {
                        let n = match entries.next_entry() {
                            Err(e) => return Err(e),
                            Ok(v) => v as usize,
                        };
                        match read_frame(
                            src,
                            &mut bufs.frame[..n],
                            &mut bufs.chunk[..len],
                            whole,
                            &mut blob,
                            &mut bufs.codec,
                        ) {
                            Err(e) => return Err(e),
                            Ok(()) => {}
                        };
                    }
                }
                match scatter(dev, &mut dst, i * chunk, &bufs.chunk[..len]) {
                    Err(e) => return Err(e),
                    Ok(()) => {}
                };
                i += 1;
            }
        }
        TYPE_SOURCECOPY => {
            let img = match image {
                None => return Err(missing_image()),
                Some(i) => i,
            };
            let mut from = Walk::new(&op.src_extents);
            let mut i: u64 = 0;
            while i < count {
                let len = chunk_len(total, chunk, i) as usize;
                match gather(&mut from, i * chunk, &mut bufs.chunk[..len], |off, out| {
                    match check_range(off, out.len(), img.capacity()) {
                        Err(e) => return Err(e),
                        Ok(()) => {}
                    };
                    return img.read_at(off, out);
                }) {
                    Err(e) => return Err(e),
                    Ok(()) => {}
                };
                match scatter(dev, &mut dst, i * chunk, &bufs.chunk[..len]) {
                    Err(e) => return Err(e),
                    Ok(()) => {}
                };
                i += 1;
            }
        }
        TYPE_TARGET_COPY => {
            let read = match reads {
                None => {
                    return Err(err(
                        Code::target_not_readable,
                        "an in-target copy needs a readable target",
                    ));
                }
                Some(r) => r,
            };
            match verify_target_source(op, chunk, total, dev, read, &mut bufs.chunk) {
                Err(e) => return Err(e),
                Ok(()) => {}
            };


            let mut from = Walk::new(&op.src_extents);
            let mut i: u64 = 0;
            while i < count {
                let len = chunk_len(total, chunk, i) as usize;
                match gather(&mut from, i * chunk, &mut bufs.chunk[..len], |off, out| {
                    return read(&*dev, off, out);
                }) {
                    Err(e) => return Err(e),
                    Ok(()) => {}
                };
                match scatter(dev, &mut dst, i * chunk, &bufs.chunk[..len]) {
                    Err(e) => return Err(e),
                    Ok(()) => {}
                };
                i += 1;
            }
        }
        _ => return Err(err(Code::unknown_operation, "operation type not supported")),
    }
    if carries_blob {
        if let Some(want) = &op.data_sha256_hash {
            if !want.is_empty() && !hash_eq(sha256::finish(blob), want) {
                return Err(err(Code::blob_mismatch, "operation data hash mismatch"));
            }
        }
    }
    return Ok(());
}



fn verify_target_source<D: Sink>(
    op: &Operation,
    chunk: u64,
    total: u64,
    dev: &D,
    read: ReadFn<D>,
    scratch: &mut [u8],
) -> Result<(), PayloadError> {
    let want = match &op.src_sha256_hash {
        None => {
            return Err(err(
                Code::source_mismatch,
                "in-target copy has no source hash",
            ));
        }
        Some(h) => h,
    };
    let mut h = sha256::new_hash();
    let mut from = Walk::new(&op.src_extents);
    let mut at: u64 = 0;
    while at < total {
        let n = core::cmp::min(chunk, total - at) as usize;
        match gather(&mut from, at, &mut scratch[..n], |off, out| {
            read(dev, off, out)
        }) {
            Err(e) => return Err(e),
            Ok(()) => {}
        };
        sha256::update(&mut h, &scratch[..n]);
        at += n as u64;
    }
    if !hash_eq(sha256::finish(h), want) {
        return Err(err(
            Code::source_mismatch,
            "in-target copy does not match its source hash",
        ));
    }
    return Ok(());
}


fn read_blob<S: Source>(
    src: &mut S,
    out: &mut [u8],
    whole: &mut sha256::Sha256,
    blob: &mut sha256::Sha256,
) -> Result<(), PayloadError> {
    match read_full(src, out) {
        Err(e) => return Err(e),
        Ok(()) => {}
    };
    sha256::update(whole, out);
    sha256::update(blob, out);
    return Ok(());
}



fn read_frame<S: Source>(
    src: &mut S,
    frame: &mut [u8],
    chunk: &mut [u8],
    whole: &mut sha256::Sha256,
    blob: &mut sha256::Sha256,
    codec: &mut Option<BlockDecoder>,
) -> Result<(), PayloadError> {
    match read_full(src, frame) {
        Err(e) => return Err(e),
        Ok(()) => {}
    };
    sha256::update(whole, frame);
    sha256::update(blob, frame);
    let dec = codec.get_or_insert_with(BlockDecoder::new);
    return dec.decode(frame, chunk);
}



struct Walk<'o> {
    extents: &'o [Extent],
    index: usize,
    base: u64,
}

impl<'o> Walk<'o> {
    fn new(extents: &'o [Extent]) -> Walk<'o> {
        return Walk {
            extents: extents,
            index: 0,
            base: 0,
        };
    }




    fn each<E>(
        &mut self,
        start: u64,
        len: usize,
        mut f: impl FnMut(u64, usize, usize) -> Result<(), E>,
    ) -> Result<(), E> {
        while self.index < self.extents.len() {
            let (_, n) = extent_of(&self.extents[self.index]);
            if self.base + n > start {
                break;
            }
            self.base += n;
            self.index += 1;
        }
        let end = start + len as u64;
        let mut i = self.index;
        let mut b = self.base;
        while i < self.extents.len() && b < end {
            let (s, n) = extent_of(&self.extents[i]);
            let e = b + n;
            if e > start {
                let lo = if start > b { start } else { b };
                let hi = if end < e { end } else { e };
                match f(s + (lo - b), (lo - start) as usize, (hi - lo) as usize) {
                    Err(err) => return Err(err),
                    Ok(()) => {}
                };
            }
            b = e;
            i += 1;
        }
        return Ok(());
    }
}


fn scatter<D: Sink>(
    dev: &mut D,
    dst: &mut Walk,
    start: u64,
    buf: &[u8],
) -> Result<(), PayloadError> {
    return dst.each(start, buf.len(), |off, at, n| {
        return dev.write_at(off, &buf[at..(at + n)]);
    });
}


fn gather<F>(from: &mut Walk, start: u64, buf: &mut [u8], mut read: F) -> Result<(), PayloadError>
where
    F: FnMut(u64, &mut [u8]) -> Result<(), PayloadError>,
{
    let len = buf.len();
    return from.each(start, len, |off, at, n| {
        return read(off, &mut buf[at..(at + n)]);
    });
}


fn extent_of(ext: &Extent) -> (u64, u64) {
    let start = match ext.start_byte {
        None => 0,
        Some(v) => v,
    };
    let n = match ext.num_bytes {
        None => 0,
        Some(v) => v,
    };
    return (start, n);
}


fn total_bytes(extents: &[Extent]) -> u64 {
    let mut total: u64 = 0;
    for ext in extents {
        total = total.saturating_add(extent_of(ext).1);
    }
    return total;
}


fn furthest(extents: &[Extent]) -> u64 {
    let mut end: u64 = 0;
    for ext in extents {
        let (s, n) = extent_of(ext);
        end = end.max(s.saturating_add(n));
    }
    return end;
}


fn op_offset(op: &Operation) -> u64 {
    return match op.data_offset {
        None => 0,
        Some(v) => v,
    };
}


fn op_length(op: &Operation) -> u64 {
    return match op.data_length {
        None => 0,
        Some(v) => v,
    };
}



pub(crate) fn hash_eq(got: [u8; sha256::SHA256_SIZE], want: &[u8]) -> bool {
    let mut i = 0;
    while i < sha256::SHA256_SIZE {
        if got[i] != want[i] {
            return false;
        }
        i += 1;
    }
    return true;
}



pub(crate) fn alloc_buffer(len: usize) -> Result<Vec<u8>, PayloadError> {
    let mut v: Vec<u8> = Vec::new();
    match v.try_reserve_exact(len) {
        Err(_) => return Err(err(Code::resource_limit, "buffer allocation failed")),
        Ok(()) => {}
    };
    v.resize(len, 0);
    return Ok(v);
}
