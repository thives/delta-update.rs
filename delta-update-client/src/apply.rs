//! The buffered apply pipeline: a complete payload in, storage writes out.
//!
//! This crate is the middleware between payload *transport* (deliberately
//! out of project scope: hosts supply complete payload bytes) and *device
//! integration* (deferred: hosts implement the traits in `sink.rs`).
//!
//! The pipeline has two phases, and the boundary between them is a type:
//!
//! ```text
//!   prepare:  frame -> verify metadata and payload -> decode -> validate plan
//!             -> verify every operation's data hash                 (no writes)
//!   apply:    policy checks -> bind capacity -> validate old image -> resume
//!             from checkpoint -> [fresh, base-dependent only: pre-flight, persist
//!             Initialized] -> operations, each committed by a checkpoint -> finish
//! ```
//!
//! [`prepare`] is the only way to obtain a [`Prepared`] value. It runs every
//! check that can be decided from the payload alone, so the apply methods can only
//! replay a plan whose blobs are already known to be authentic. Blob hashes are
//! checked up front here, so this path refuses a corrupt blob before any write. The
//! streamed path cannot do that (`docs/stream.md`).
//!
//! Device applies take a [`Policy`]. Hosts and the CLI use `Policy::Strict`, and
//! `Policy::Development` is for tests and development. The write-only sink methods are
//! an explicit weaker capability: they always return [`Outcome::Applied`], because
//! nothing can be read back to verify.

use crate::checkpoint::CheckpointStore;
use crate::checkpoint::TargetId;
use crate::engine;
use crate::engine::Prepared as Engine;
use crate::image::NoImage;
use crate::image::OldImage;
use crate::sink::Readable;
use crate::sink::Sink;
use crate::source::SliceSource;
use delta_update_payload::error::Code;
use delta_update_payload::error::PayloadError;
use delta_update_payload::error::err;
use delta_update_payload::layout;
use delta_update_payload::plan;
use delta_update_payload::plan::Geometry;
use delta_update_payload::profile::ResourceProfile;
use delta_update_payload::sha256;
use delta_update_payload::verify;

pub use crate::engine::Cleared;
pub use crate::engine::NoProgress;
pub use crate::engine::Progress;
pub use crate::engine::Trust;
pub use crate::policy::Outcome;
pub use crate::policy::Policy;

/// A payload that has been authenticated (per `trust`), validated against
/// `profile`, and whose blobs are verified, but not yet applied. Nothing has been
/// written to storage.
pub struct Prepared<'a> {
    payload: &'a [u8],
    inner: Engine<'a>,
}

/// Authenticate, decode and validate a complete payload, and check every blob
/// against its operation's data hash. The only failures are metadata or payload
/// errors, and none of them touch storage.
pub fn prepare<'a>(
    payload: &'a [u8],
    trust: Trust<'a>,
    profile: &ResourceProfile,
) -> Result<Prepared<'a>, PayloadError> {
    let (info, manifest, verified, keys) = match trust {
        Trust::Verified(keys) => match verify::verify_payload(payload, keys, profile) {
            Err(e) => return Err(e),
            Ok(a) => {
                let layout::Parsed { info, manifest } = a.into_parsed();
                (info, manifest, true, keys)
            }
        },
        Trust::Unverified => match layout::parse_unverified(payload, profile) {
            Err(e) => return Err(e),
            Ok(u) => {
                let layout::Parsed { info, manifest } = u.into_parsed();
                (info, manifest, false, &[][..])
            }
        },
    };
    let validated = match plan::validate(manifest, profile) {
        Err(e) => return Err(e),
        Ok(v) => v,
    };

    let blobs_len = if info.has_sig {
        info.sig_offset_blobs
    } else {
        info.payload_size - info.blobs_start
    };
    if validated.geometry().blob_total != blobs_len {
        return Err(err(
            Code::parse,
            "blob region length disagrees with the manifest",
        ));
    }
    let blobs_start = info.blobs_start;
    match verify_blobs(payload, blobs_start, &validated) {
        Err(e) => return Err(e),
        Ok(()) => {}
    };
    let head = &payload[..layout::HEADER_SIZE];
    let manifest_end = info.metadata_size as usize;
    let manifest_bytes = &payload[layout::HEADER_SIZE..manifest_end];
    let inner = engine::assemble(
        head,
        manifest_bytes,
        info.meta_sig_size as usize,
        validated,
        keys,
        verified,
        false,
        profile,
    );
    return Ok(Prepared {
        payload: payload,
        inner: inner,
    });
}

fn verify_blobs(
    payload: &[u8],
    blobs_start: u64,
    validated: &plan::Validated,
) -> Result<(), PayloadError> {
    for op in &validated.manifest().partitions[0].operations {
        let want = match &op.data_sha256_hash {
            None => continue,
            Some(h) => h,
        };
        if want.is_empty() {
            continue;
        }
        let off = match op.data_offset {
            None => 0,
            Some(v) => v,
        };
        let len = match op.data_length {
            None => 0,
            Some(v) => v,
        };
        let start = (blobs_start + off) as usize;
        let end = start + (len as usize);
        let blob = match payload.get(start..end) {
            None => return Err(err(Code::parse, "operation blob outside the payload")),
            Some(b) => b,
        };
        let got = sha256::hash_bytes(blob);
        if !engine::hash_eq(got, want) {
            return Err(err(Code::blob_mismatch, "operation data hash mismatch"));
        }
    }
    return Ok(());
}

impl<'a> Prepared<'a> {
    /// The logical partition label for adapter routing, not a path or slot selector.
    /// It is authenticated only when prepared with [`Trust::Verified`].
    pub fn partition_name(&self) -> &str {
        return self.inner.partition_name();
    }

    /// The validated geometry: new-image size, chunk size, operations and reach.
    /// Use it to size storage before applying.
    pub fn geometry(&self) -> &Geometry {
        return self.inner.geometry();
    }

    /// Apply to a write-only sink: an explicit, weaker capability. A plan that copies
    /// from the old image, or within the target, is refused with no write, as is a
    /// base-dependent plan: some of its bytes are the destination's own, and a write-only
    /// destination cannot supply them. Only a destination-independent plan applies. The
    /// result is [`Outcome::Applied`] and never `Verified`, since nothing can be read
    /// back. The strict policy refuses every such update.
    pub fn apply_to_sink<D: Sink, C: CheckpointStore, P: Progress>(
        self,
        sink: &mut D,
        store: &mut C,
        target: &TargetId,
        prog: &P,
    ) -> Result<Outcome, PayloadError> {
        return self.run(
            sink,
            None,
            None::<&NoImage>,
            store,
            Policy::Development,
            target,
            prog,
        );
    }

    /// Apply to a write-only sink, with the old image that `SOURCE_COPY` reads. The
    /// image does not make a base-dependent plan applicable: its bytes are the
    /// destination's own, so that plan is refused as for
    /// [`apply_to_sink`](Self::apply_to_sink). The result is [`Outcome::Applied`].
    pub fn apply_to_sink_with_image<D: Sink, I: OldImage, C: CheckpointStore, P: Progress>(
        self,
        sink: &mut D,
        image: &I,
        store: &mut C,
        target: &TargetId,
        prog: &P,
    ) -> Result<Outcome, PayloadError> {
        return self.run(
            sink,
            None,
            Some(image),
            store,
            Policy::Development,
            target,
            prog,
        );
    }

    /// Apply to a readable device under `policy`. A base-dependent update is checked
    /// against the signed old hash before its first write, and every update is checked
    /// against the signed new hash after its flush. An interrupted update resumes from
    /// its checkpoint, so the device is not hashed against the old image again.
    ///
    /// Under [`Policy::Strict`] every requirement of that policy is checked before the
    /// first write, and a success is [`Outcome::Verified`]. A checkpoint that records a
    /// failed final check is refused with `recovery_required` until
    /// [`reset_checkpoint`](Self::reset_checkpoint) clears it.
    pub fn apply_to_device<D: Readable, C: CheckpointStore, P: Progress>(
        self,
        dev: &mut D,
        store: &mut C,
        target: &TargetId,
        policy: Policy,
        prog: &P,
    ) -> Result<Outcome, PayloadError> {
        return self.run(
            dev,
            Some(<D as Readable>::read_at),
            None::<&NoImage>,
            store,
            policy,
            target,
            prog,
        );
    }

    /// Apply to a readable device, with the old image that `SOURCE_COPY` reads, under
    /// `policy`. The image is validated before the device is read or written, and its
    /// old hash is checked independently of the destination. A base-dependent update
    /// still requires the destination to match the old image before its first write,
    /// as [`apply_to_device`](Self::apply_to_device) does. Untouched and sparse-kept
    /// bytes must already hold their required contents.
    pub fn apply_to_device_with_image<D: Readable, I: OldImage, C: CheckpointStore, P: Progress>(
        self,
        dev: &mut D,
        image: &I,
        store: &mut C,
        target: &TargetId,
        policy: Policy,
        prog: &P,
    ) -> Result<Outcome, PayloadError> {
        return self.run(
            dev,
            Some(<D as Readable>::read_at),
            Some(image),
            store,
            policy,
            target,
            prog,
        );
    }

    /// Clear a checkpoint that needs recovery, so the next apply starts this update
    /// from its first operation. Returns what was removed, or `None` if there was no
    /// record. See [`engine::reset`] for the rule: a base-dependent update resets only
    /// if the destination is the old image again, and this method checks that before it
    /// changes anything.
    pub fn reset_checkpoint<D: Readable, C: CheckpointStore>(
        &self,
        dev: &D,
        store: &mut C,
        target: &TargetId,
    ) -> Result<Option<Cleared>, PayloadError> {
        return engine::reset(
            &self.inner,
            target,
            dev,
            Some(<D as Readable>::read_at),
            store,
        );
    }

    /// What the destination must hold before the first write: the old image for a
    /// base-dependent payload, nothing for a destination-independent one.
    pub fn contract(&self) -> delta_update_payload::plan::Contract {
        return self.inner.contract();
    }

    /// The signed release version, when the payload carries one. Authenticated only
    /// when prepared with [`Trust::Verified`].
    pub fn release_version(&self) -> Option<u64> {
        return self.inner.release_version();
    }

    fn run<D: Sink, I: OldImage, C: CheckpointStore, P: Progress>(
        self,
        dev: &mut D,
        reads: Option<engine::ReadFn<D>>,
        image: Option<&I>,
        store: &mut C,
        policy: Policy,
        target: &TargetId,
        prog: &P,
    ) -> Result<Outcome, PayloadError> {
        let payload = self.payload;

        let open = |offset: u64| SliceSource::new(payload, offset as usize);
        return engine::run(
            &self.inner,
            target,
            open,
            dev,
            reads,
            image,
            store,
            policy,
            prog,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::checkpoint::MemoryStore;
    use crate::testing::BLOCKS;
    use crate::testing::BS;
    use crate::testing::Dev;
    use crate::testing::SliceImage;
    use crate::testing::keys;
    use crate::testing::new_image;
    use crate::testing::old_image;
    use crate::testing::signed_payload;
    use crate::testing::two_op_payload;
    use crate::testing::unsigned_payload;
    use crate::testing::write_only_payload;
    use alloc::vec;
    use delta_update_payload::manifest::TYPE_REPLACE;
    use delta_update_payload::profile::EMBEDDED;

    const TARGET: TargetId = [9; 32];

    fn sink_run(
        payload: &[u8],
        trust: Trust,
        dev: &mut Dev,
        store: &mut MemoryStore,
    ) -> Result<(), PayloadError> {
        let prepared = match prepare(payload, trust, &EMBEDDED) {
            Err(e) => return Err(e),
            Ok(p) => p,
        };
        let image = SliceImage { data: &old_image() };
        return prepared
            .apply_to_sink_with_image(dev, &image, store, &TARGET, &NoProgress {})
            .map(|_| ());
    }

    fn device_run(
        payload: &[u8],
        trust: Trust,
        dev: &mut Dev,
        store: &mut MemoryStore,
    ) -> Result<(), PayloadError> {
        let prepared = match prepare(payload, trust, &EMBEDDED) {
            Err(e) => return Err(e),
            Ok(p) => p,
        };
        let image = SliceImage { data: &old_image() };
        return prepared
            .apply_to_device_with_image(
                dev,
                &image,
                store,
                &TARGET,
                Policy::Development,
                &NoProgress {},
            )
            .map(|_| ());
    }

    fn code<T>(result: Result<T, PayloadError>) -> Code {
        match result {
            Ok(_) => panic!("expected an error"),
            Err(e) => return e.code,
        }
    }

    #[test]
    fn verified_payload_applies_to_a_write_only_sink() {
        let payload = write_only_payload();
        let keys = keys();
        let mut dev = Dev::new(old_image());
        sink_run(
            &payload,
            Trust::Verified(&keys),
            &mut dev,
            &mut MemoryStore::new(),
        )
        .unwrap();
        assert_eq!(dev.data, new_image());
    }

    #[test]
    fn verified_tampering_is_rejected_before_any_write() {
        let mut payload = signed_payload();
        let info = layout::parse_unverified(&payload, &EMBEDDED).ok().unwrap();
        let blob = info.parsed().info.blobs_start as usize;
        payload[blob + 1] ^= 1;
        let keys = keys();
        let mut dev = Dev::new(old_image());
        assert_eq!(
            code(sink_run(
                &payload,
                Trust::Verified(&keys),
                &mut dev,
                &mut MemoryStore::new()
            )),
            Code::signature_mismatch
        );
        assert_eq!(dev.writes, 0);
    }

    #[test]
    fn unverified_blob_tampering_is_caught_by_its_hash_before_any_write() {
        let mut payload = signed_payload();
        let info = layout::parse_unverified(&payload, &EMBEDDED).ok().unwrap();
        let blob = info.parsed().info.blobs_start as usize;
        payload[blob + 1] ^= 1;
        let mut dev = Dev::new(old_image());
        assert_eq!(
            code(sink_run(
                &payload,
                Trust::Unverified,
                &mut dev,
                &mut MemoryStore::new()
            )),
            Code::blob_mismatch
        );
        assert_eq!(dev.writes, 0);
    }

    #[test]
    fn stale_device_fails_preflight_before_any_write() {
        let payload = signed_payload();
        let keys = keys();
        let mut stale = old_image();
        stale[0] ^= 0xff;
        let mut dev = Dev::new(stale);
        let prepared = prepare(&payload, Trust::Verified(&keys), &EMBEDDED).unwrap();
        assert_eq!(
            prepared.contract(),
            delta_update_payload::plan::Contract::BaseDependent
        );
        assert_eq!(
            code(prepared.apply_to_device_with_image(
                &mut dev,
                &SliceImage { data: &old_image() },
                &mut MemoryStore::new(),
                &TARGET,
                Policy::Development,
                &NoProgress {}
            )),
            Code::hash_mismatch
        );
        assert_eq!(dev.writes, 0);
    }

    #[test]
    fn a_destination_independent_update_applies_from_unrelated_contents() {
        let payload = two_op_payload();
        let keys = keys();
        let prepared = prepare(&payload, Trust::Verified(&keys), &EMBEDDED).unwrap();
        assert_eq!(
            prepared.contract(),
            delta_update_payload::plan::Contract::DestinationIndependent
        );
        let mut unrelated = Dev::new(vec![0xa5; old_image().len()]);

        assert_eq!(
            prepared
                .apply_to_device(
                    &mut unrelated,
                    &mut MemoryStore::new(),
                    &TARGET,
                    Policy::Development,
                    &NoProgress {}
                )
                .unwrap(),
            Outcome::Applied
        );
        assert_eq!(unrelated.data, new_image());
    }

    #[test]
    fn lost_writes_are_detected_by_the_post_flight_hash() {
        let payload = signed_payload();
        let keys = keys();
        let mut dev = Dev::new(old_image());
        dev.drop_writes = true;
        assert_eq!(
            code(device_run(
                &payload,
                Trust::Verified(&keys),
                &mut dev,
                &mut MemoryStore::new()
            )),
            Code::hash_mismatch
        );
    }

    #[test]
    fn insufficient_device_capacity_is_rejected_with_zero_writes() {
        let payload = signed_payload();
        let keys = keys();
        let mut dev = Dev::new(vec![0u8; (BLOCKS - 1) * BS]);
        assert_eq!(
            code(device_run(
                &payload,
                Trust::Verified(&keys),
                &mut dev,
                &mut MemoryStore::new()
            )),
            Code::partition_mismatch
        );
        assert_eq!(dev.writes, 0);
    }

    #[test]
    fn flush_failure_is_reported_on_both_paths() {
        let payload = signed_payload();
        let keys = keys();
        let mut dev = Dev::new(old_image());
        dev.fail_flush = true;
        assert!(
            sink_run(
                &payload,
                Trust::Verified(&keys),
                &mut dev,
                &mut MemoryStore::new()
            )
            .is_err()
        );
        let mut dev = Dev::new(old_image());
        dev.fail_flush = true;
        assert!(
            device_run(
                &payload,
                Trust::Verified(&keys),
                &mut dev,
                &mut MemoryStore::new()
            )
            .is_err()
        );
    }

    #[test]
    fn unsigned_payload_is_refused_when_verification_is_required() {
        let payload = unsigned_payload(TYPE_REPLACE);
        let keys = keys();
        let mut dev = Dev::new(old_image());
        assert_eq!(
            code(sink_run(
                &payload,
                Trust::Verified(&keys),
                &mut dev,
                &mut MemoryStore::new()
            )),
            Code::signature_missing
        );
        assert_eq!(dev.writes, 0);
    }

    #[test]
    fn unverified_blob_region_must_match_the_manifest() {
        let mut payload = unsigned_payload(TYPE_REPLACE);
        payload.push(0);
        let mut dev = Dev::new(old_image());
        assert_eq!(
            code(sink_run(
                &payload,
                Trust::Unverified,
                &mut dev,
                &mut MemoryStore::new()
            )),
            Code::parse
        );
        assert_eq!(dev.writes, 0);
    }

    #[test]
    fn an_interrupted_update_resumes_from_its_checkpoint() {
        let payload = two_op_payload();
        let keys = keys();
        let mut store = MemoryStore::new();
        let mut dev = Dev::new(old_image());

        dev.fail_write_at = Some(5);
        assert!(device_run(&payload, Trust::Verified(&keys), &mut dev, &mut store).is_err());
        assert_ne!(dev.data, new_image());
        assert!(store.stored().is_some());

        dev.fail_write_at = None;
        let writes_before = dev.writes;
        device_run(&payload, Trust::Verified(&keys), &mut dev, &mut store).unwrap();
        assert_eq!(dev.data, new_image());

        let fresh = {
            let mut fresh = Dev::new(old_image());
            device_run(
                &payload,
                Trust::Verified(&keys),
                &mut fresh,
                &mut MemoryStore::new(),
            )
            .unwrap();
            fresh.writes
        };
        assert!(dev.writes - writes_before < fresh);
    }

    #[test]
    fn a_checkpoint_of_another_target_is_refused_with_no_write() {
        let payload = signed_payload();
        let keys = keys();
        let mut store = MemoryStore::new();
        let mut dev = Dev::new(old_image());
        device_run(&payload, Trust::Verified(&keys), &mut dev, &mut store).unwrap();
        let prepared = prepare(&payload, Trust::Verified(&keys), &EMBEDDED).unwrap();
        let mut other = Dev::new(new_image());
        let other_target = [1u8; 32];
        assert_eq!(
            code(prepared.apply_to_device_with_image(
                &mut other,
                &SliceImage { data: &old_image() },
                &mut store,
                &other_target,
                Policy::Development,
                &NoProgress {}
            )),
            Code::checkpoint_mismatch
        );
        assert_eq!(other.writes, 0);
    }
}
