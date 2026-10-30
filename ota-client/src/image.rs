//! The old image: the read-only source of `SOURCE_COPY`.
//!
//! This is a capability separate from the payload [`Source`](crate::source::Source)
//! and from the [`Sink`](crate::sink::Sink) being written. A copy reads bytes of
//! the image that the update does not write to, so the destination never feeds
//! its own source through this path. An in-target copy is the other kind, and it
//! reads the target instead (`docs/format.md` §5.1).
//!
//! The image must be unchanged for the whole update, including every resumed
//! attempt. A generic trait cannot enforce that, so it is a platform contract
//! (`docs/format.md` §8). The client checks what it can before the first write:
//! the capacity, the whole-image hash, and each copy's source hash.

use ota_payload::error::Code;
use ota_payload::error::PayloadError;
use ota_payload::error::err;

/// Read-only, byte-addressed access to the old image.
pub trait OldImage {
    /// Capacity in bytes. It must equal the recorded old partition size.
    fn capacity(self: &Self) -> u64;

    /// Fill `out` from byte `offset`. The range must lie inside the capacity.
    fn read_at(self: &Self, offset: u64, out: &mut [u8]) -> Result<(), PayloadError>;
}

/// The error for a copy that has no old image. The plan's `needs_source` decides
/// whether an image is needed, and this is what a missing one reports.
pub fn missing_image() -> PayloadError {
    return err(Code::source_missing, "SOURCE_COPY needs the old image");
}

/// The image of an update that copies nothing. The plan decides whether an image
/// is needed, so this is only ever passed where none is required, or where its
/// absence is refused before any write.
pub struct NoImage {}

impl OldImage for NoImage {
    fn capacity(self: &NoImage) -> u64 {
        return 0;
    }

    fn read_at(self: &NoImage, offset: u64, out: &mut [u8]) -> Result<(), PayloadError> {
        let _ = (offset, out);
        return Err(missing_image());
    }
}
