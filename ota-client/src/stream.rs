//! The streamed, resumable apply path: payload bytes *pulled* through a
//! [`Source`](source.rs) instead of supplied as a complete buffer.
//!
//! Two phases, as in the buffered pipeline, and the boundary is a type:
//!
//! ```text
//!   prepare:  read header (bounded) -> read manifest + metadata signature
//!             -> verify metadata -> decode -> validate plan
//!   apply:    resume from the checkpoint -> the host opens the payload at the
//!             offset it asks for -> replay operations -> verify payload signature
//!             -> end of stream -> flush -> post-flight hash -> Complete
//! ```
//!
//! [`prepare`] reads only the metadata region, which is the only part whose
//! size must be known up front, and it authenticates that region before the
//! manifest is decoded. It does not consume the payload beyond that region, so
//! the host decides where the payload stream begins: at the start of the blobs
//! for a fresh update, or at the offset a checkpoint names for a resumed one.
//! Operations already committed are skipped without being read.
//!
//! Each operation is committed by a checkpoint before the next begins. An
//! interruption therefore loses at most the operation that was running, and a
//! resumed run executes that operation again from its first chunk
//! (`docs/stream.md`).

use crate::checkpoint::CheckpointStore;
use crate::checkpoint::TargetId;
use crate::engine;
use crate::engine::alloc_buffer;
use crate::engine::authenticate;
use crate::engine::Trust;
use crate::engine::Progress;
use crate::image::NoImage;
use crate::image::OldImage;
use crate::policy::Outcome;
use crate::policy::Policy;
use crate::sink::Readable;
use crate::source::Source;
use crate::source::read_full;
use ota_payload::error::PayloadError;
use ota_payload::layout;
use ota_payload::profile::ResourceProfile;

/// A streamed payload whose metadata is authenticated and validated. Nothing has
/// been written to storage, and no payload bytes beyond the metadata region have
/// been read.
pub type Prepared<'a> = engine::Prepared<'a>;

/// Read and authenticate the metadata region of a streamed payload, and validate
/// it. Reads at most the header, the manifest and the metadata signature, each
/// bounded by `profile`. The source is consumed only as far as that region.
pub fn prepare<'a, S: Source>(
    mut src: S,
    trust: Trust<'a>,
    profile: &ResourceProfile,
) -> Result<Prepared<'a>, PayloadError> {


    let mut head: [u8; layout::HEADER_SIZE] = [0; layout::HEADER_SIZE];
    match read_full(&mut src, &mut head) {
        Err(e) => return Err(e),
        Ok(()) => {}
    };
    let h = match layout::parse_header(&head, profile) {
        Err(e) => return Err(e),
        Ok(v) => v,
    };
    let mut manifest_bytes = match alloc_buffer(h.manifest_size as usize) {
        Err(e) => return Err(e),
        Ok(v) => v,
    };
    match read_full(&mut src, &mut manifest_bytes) {
        Err(e) => return Err(e),
        Ok(()) => {}
    };
    let mut meta_sig = match alloc_buffer(h.meta_sig_size as usize) {
        Err(e) => return Err(e),
        Ok(v) => v,
    };
    match read_full(&mut src, &mut meta_sig) {
        Err(e) => return Err(e),
        Ok(()) => {}
    };
    return authenticate(&head, &manifest_bytes, &meta_sig, trust, profile);
}

impl<'a> Prepared<'a> {
    /// Replay the rest of the stream onto `dev` under `policy`, resuming any
    /// interrupted update.
    ///
    /// `open` is called once, with the payload offset this run must start from:
    /// the blobs' start for a fresh update, or the offset a checkpoint records. It
    /// must return a source positioned there. A plan that copies from the old image
    /// is refused, with no write, because no image was supplied: use
    /// [`apply_with_image`](Self::apply_with_image).
    ///
    /// A streamed target is staged until this returns `Ok`: the host must not boot or
    /// publish it earlier (`docs/stream.md` §5.1). Its `Outcome::Verified` is reached only
    /// after the final signature, the end of the stream, the flush and the read-back.
    pub fn apply<S: Source, D: Readable, C: CheckpointStore, P: Progress>(
        &self,
        open: impl FnOnce(u64) -> S,
        dev: &mut D,
        store: &mut C,
        target: &TargetId,
        policy: Policy,
        prog: &P,
    ) -> Result<Outcome, PayloadError> {
        return engine::run(
            self,
            target,
            open,
            dev,
            Some(<D as Readable>::read_at),
            None::<&NoImage>,
            store,
            policy,
            prog,
        );
    }

    /// Replay the rest of the stream onto `dev` under `policy`, with the old image that
    /// `SOURCE_COPY` reads. The image must stay unchanged across resumed runs.
    /// Its old hash is checked independently of the target. Untouched and sparse-kept
    /// target bytes are not initialized from this image; they must already be correct.
    pub fn apply_with_image<S: Source, D: Readable, I: OldImage, C: CheckpointStore, P: Progress>(
        &self,
        open: impl FnOnce(u64) -> S,
        dev: &mut D,
        image: &I,
        store: &mut C,
        target: &TargetId,
        policy: Policy,
        prog: &P,
    ) -> Result<Outcome, PayloadError> {
        return engine::run(
            self,
            target,
            open,
            dev,
            Some(<D as Readable>::read_at),
            Some(image),
            store,
            policy,
            prog,
        );
    }

    /// Clear a checkpoint that needs recovery, and return what was removed. See
    /// [`engine::reset`].
    pub fn reset_checkpoint<D: Readable, C: CheckpointStore>(
        &self,
        dev: &D,
        store: &mut C,
        target: &TargetId,
    ) -> Result<Option<engine::Cleared>, PayloadError> {
        return engine::reset(self, target, dev, Some(<D as Readable>::read_at), store);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::checkpoint::MemoryStore;
    use crate::engine::NoProgress;
    use crate::apply::prepare as prepare_buffered;
    use crate::testing::BLOCKS;
    use crate::testing::BS;
    use crate::testing::Chunked;
    use crate::testing::Dev;
    use crate::testing::keys;
    use crate::testing::SliceImage;
    use crate::testing::two_op_payload;
    use crate::testing::new_image;
    use crate::testing::old_image;
    use crate::testing::signed_payload;
    use crate::testing::unsigned_payload;
    use alloc::vec;
    use alloc::vec::Vec;
    use ota_payload::error::Code;
    use ota_payload::manifest::TYPE_REPLACE;
    use ota_payload::profile::EMBEDDED;

    const TARGET: TargetId = [4; 32];



    fn run(payload: &[u8], trust: Trust, chunk: usize, dev: &mut Dev, store: &mut MemoryStore) -> Result<(), PayloadError> {
        let src = Chunked::new(payload.to_vec(), chunk);
        let prepared = match prepare(src, trust, &EMBEDDED) {
            Err(e) => return Err(e),
            Ok(p) => p,
        };
        let open = |offset: u64| Chunked::at(payload, offset as usize, chunk);
        let image = SliceImage { data: &old_image() };
        return prepared.apply_with_image(open, dev, &image, store, &TARGET, Policy::Development, &NoProgress {}).map(|_| ());
    }

    fn code<T>(result: Result<T, PayloadError>) -> Code {
        match result {
            Ok(_) => panic!("expected an error"),
            Err(e) => return e.code,
        }
    }

    #[test]
    fn streamed_apply_matches_the_buffered_pipeline() {
        let payload = signed_payload();
        let keys = keys();
        let mut streamed = Dev::new(old_image());

        run(&payload, Trust::Verified(&keys), 7, &mut streamed, &mut MemoryStore::new()).unwrap();
        let mut buffered = Dev::new(old_image());
        let prepared = prepare_buffered(&payload, Trust::Verified(&keys), &EMBEDDED).unwrap();
        prepared
            .apply_to_device_with_image(&mut buffered, &SliceImage { data: &old_image() }, &mut MemoryStore::new(), &TARGET, Policy::Development, &NoProgress {})
            .unwrap();
        assert_eq!(streamed.data, new_image());
        assert_eq!(streamed.data, buffered.data);
    }

    #[test]
    fn resume_converges_after_an_interrupted_stream() {
        let payload = two_op_payload();
        let keys = keys();
        let mut store = MemoryStore::new();
        let mut dev = Dev::new(old_image());
        dev.fail_write_at = Some(5);
        assert!(run(&payload, Trust::Verified(&keys), 64, &mut dev, &mut store).is_err());
        dev.fail_write_at = None;
        let before = dev.writes;
        run(&payload, Trust::Verified(&keys), 64, &mut dev, &mut store).unwrap();
        assert_eq!(dev.data, new_image());


        let mut fresh = Dev::new(old_image());
        run(&payload, Trust::Verified(&keys), 64, &mut fresh, &mut MemoryStore::new()).unwrap();
        assert!(dev.writes - before < fresh.writes);
        assert!(fresh.writes <= BLOCKS * 2);
    }

    #[test]
    fn reapplying_a_completed_update_writes_nothing() {
        let payload = signed_payload();
        let keys = keys();
        let mut store = MemoryStore::new();
        let mut dev = Dev::new(old_image());
        run(&payload, Trust::Verified(&keys), 64, &mut dev, &mut store).unwrap();
        let writes = dev.writes;
        run(&payload, Trust::Verified(&keys), 64, &mut dev, &mut store).unwrap();
        assert_eq!(dev.writes, writes);
        assert_eq!(dev.data, new_image());
    }

    #[test]
    fn truncated_and_trailing_streams_are_rejected() {
        let payload = signed_payload();
        let keys = keys();
        let mut dev = Dev::new(old_image());
        let truncated = &payload[..payload.len() - 1];
        assert!(run(truncated, Trust::Verified(&keys), 64, &mut dev, &mut MemoryStore::new()).is_err());
        let mut trailing = payload.clone();
        trailing.push(0);
        let mut dev = Dev::new(old_image());
        assert_eq!(
            code(run(&trailing, Trust::Verified(&keys), 64, &mut dev, &mut MemoryStore::new())),
            Code::parse
        );
    }

    #[test]
    fn unverified_tampering_is_detected_after_the_write() {
        let mut payload = signed_payload();
        let info = ota_payload::layout::parse_unverified(&payload, &EMBEDDED).ok().unwrap();


        let parsed = info.parsed();
        let last = parsed.manifest.partitions[0]
            .operations
            .iter()
            .rev()
            .find(|op| op.data_length.unwrap_or(0) > 0)
            .unwrap();
        let at = parsed.info.blobs_start as usize + last.data_offset.unwrap_or(0) as usize + 1;
        payload[at] ^= 1;
        let mut dev = Dev::new(old_image());
        let got = code(run(&payload, Trust::Unverified, 64, &mut dev, &mut MemoryStore::new()));
        assert!(got == Code::frame_malformed || got == Code::blob_mismatch);


        assert!(dev.writes > 0);
    }

    #[test]
    fn verified_stream_requires_both_signature_messages() {
        let payload = unsigned_payload(TYPE_REPLACE);
        let keys = keys();
        let mut dev = Dev::new(old_image());
        assert_eq!(
            code(run(&payload, Trust::Verified(&keys), 64, &mut dev, &mut MemoryStore::new())),
            Code::signature_missing
        );
        assert_eq!(dev.writes, 0);
    }

    #[test]
    fn profile_limits_are_enforced_before_the_manifest_is_read() {
        let payload = signed_payload();
        let keys = keys();
        let tiny = ResourceProfile {
            max_encoded_manifest: 8,
            ..EMBEDDED
        };
        let mut src = Chunked::new(payload, 1024);
        let result = prepare(&mut src, Trust::Verified(&keys), &tiny);
        assert_eq!(
            match result {
                Ok(_) => panic!("oversized manifest accepted"),
                Err(e) => e.code,
            },
            Code::manifest_too_big
        );

        assert_eq!(src.pos, layout::HEADER_SIZE);
    }

    #[test]
    fn unsupported_operation_is_refused_by_prepare() {

        let payload = unsigned_payload(8);
        let src = Chunked::new(payload, 64);
        assert_eq!(
            match prepare(src, Trust::Unverified, &EMBEDDED) {
                Ok(_) => panic!("unsupported operation accepted"),
                Err(e) => e.code,
            },
            Code::unknown_operation
        );
    }

    #[test]
    fn device_capacity_is_bound_before_any_read_or_write() {
        let payload = signed_payload();
        let keys = keys();
        let mut dev = Dev::new(vec_of(BS / 2, BLOCKS));
        assert_eq!(
            code(run(&payload, Trust::Verified(&keys), 64, &mut dev, &mut MemoryStore::new())),
            Code::partition_mismatch
        );
        assert_eq!(dev.reads.get(), 0);
        assert_eq!(dev.writes, 0);
    }

    #[test]
    fn flush_failure_is_reported_by_the_stream() {
        let payload = signed_payload();
        let keys = keys();
        let mut dev = Dev::new(old_image());
        dev.fail_flush = true;
        assert!(run(&payload, Trust::Verified(&keys), 64, &mut dev, &mut MemoryStore::new()).is_err());
    }

    fn vec_of(len: usize, blocks: usize) -> Vec<u8> {
        return vec![0u8; len * blocks];
    }
}
