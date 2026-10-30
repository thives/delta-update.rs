//! The storage boundary of the middleware.
//!
//! All I/O is byte-addressed: a write names a byte offset and carries the bytes
//! for that range, and a read names a byte offset and fills a caller-owned
//! buffer. This crate never allocates on behalf of storage and never assumes a
//! physical alignment or erase unit. An adapter that sits on block hardware owns
//! that translation: it reads and rewrites whole blocks around a byte range,
//! erases what it must, and keeps the write inside the range it was given.
//!
//! `Sink` is the whole coupling surface of the write path: capacity, byte-range
//! writes and a persistence barrier. Whole-image integrity hashing and
//! in-target copies *read* storage back, so reads live behind the separate
//! `Readable` capability. A host that can read its storage opts in; one that
//! cannot still gets every write-only check.
//!
//! Capacity is part of the contract, not an assumption: the middleware refuses
//! to write unless every range the metadata names lies inside the capacity the
//! sink reports, and the recorded new-image size must fit. Capacity is never resized. See
//! `docs/client.md` §2.

use ota_payload::error::Code;
use ota_payload::error::PayloadError;
use ota_payload::error::err;

/// The minimum the apply middleware needs from storage: its capacity, byte-range
/// writes and a persistence barrier.
pub trait Sink {
    /// Capacity in bytes. Fixed for the storage's lifetime.
    fn capacity(self: &Self) -> u64;

    /// Write `data` at byte `offset`. Acceptance by the storage layer is not
    /// persistence; that is the job of [`flush`](Sink::flush).
    ///
    /// A failed write may leave part of the requested range modified. It must not
    /// modify any byte outside that range. The middleware never relies on a
    /// failed write being atomic, and a rerun repairs the range.
    fn write_at(self: &mut Self, offset: u64, data: &[u8]) -> Result<(), PayloadError>;

    /// Persistence barrier. On success, every write made before this call must be
    /// on the host's stable storage, meaning it survives power loss and is not
    /// merely visible through a cache. A sink that cannot make that guarantee must
    /// not report success here.
    fn flush(self: &mut Self) -> Result<(), PayloadError>;
}

/// Capability layer for storage that can be read back. In-target copies read the
/// target, and the image-prefix pre-flight and post-flight hashes read it too.
pub trait Readable: Sink {
    /// Fill `out` from byte `offset`. The range must lie inside the capacity.
    fn read_at(self: &Self, offset: u64, out: &mut [u8]) -> Result<(), PayloadError>;
}

/// Validate a byte range against a storage capacity. Every bundled sink calls this
/// before touching storage, so the bounds rule has exactly one definition.
pub fn check_range(offset: u64, len: usize, capacity: u64) -> Result<(), PayloadError> {
    let end = match offset.checked_add(len as u64) {
        None => return Err(err(Code::extent_out_of_range, "byte range overflows")),
        Some(e) => e,
    };
    if end > capacity {
        return Err(err(
            Code::extent_out_of_range,
            "byte range is outside the storage",
        ));
    }
    return Ok(());
}

/// Convenience helper shared by sink implementors for turning short
/// reads and platform failures into the payload error vocabulary.
pub fn io_error() -> PayloadError {
    err(Code::device_io, "storage io error")
}
