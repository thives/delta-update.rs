//! The payload-input boundary of the streaming applier.
//!
//! Symmetric to [`Sink`]: where `Sink` abstracts *storage writes*, `Source`
//! abstracts *payload bytes in*. A host implements it however it can produce
//! bytes: a positional file read, a RAM buffer, a decompressed chunk chain,
//! or the tail of a download it is still receiving. This crate only ever
//! *pulls* bytes, so the payload never has to be resident, and no transport
//! (download, storage of the package) is implemented here — that stays out
//! of project scope.
//!
//! The whole design rests on one property of the payload format: every region
//! is consumed strictly in physical order — header, manifest, metadata
//! signature, blobs, payload signature — so a sequential pull is sufficient.
//! No seeking, skipping or random access is ever needed.

use delta_update_payload::error::Code;
use delta_update_payload::error::PayloadError;
use delta_update_payload::error::err;

pub trait Source {
    /// Pull up to `out.len()` bytes into `out`, returning how many were read.
    /// A short read is legal mid-message (chunked hosts); returning `0` means
    /// the stream has ended. Takes a mutable receiver: a source is a cursor.
    fn read(self: &mut Self, out: &mut [u8]) -> Result<usize, PayloadError>;
}

impl<S: Source> Source for &mut S {
    fn read(self: &mut Self, out: &mut [u8]) -> Result<usize, PayloadError> {
        return (**self).read(out);
    }
}

/// A source over bytes that are already in memory, starting at `pos`. The buffered
/// pipeline resumes through one of these, so a checkpoint's offset is an index.
pub struct SliceSource<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> SliceSource<'a> {
    pub fn new(data: &'a [u8], pos: usize) -> SliceSource<'a> {
        return SliceSource {
            data: data,
            pos: pos,
        };
    }
}

impl<'a> Source for SliceSource<'a> {
    fn read(self: &mut Self, out: &mut [u8]) -> Result<usize, PayloadError> {
        let rest = match self.data.get(self.pos..) {
            None => &[][..],
            Some(r) => r,
        };
        let n = if out.len() < rest.len() {
            out.len()
        } else {
            rest.len()
        };
        out[..n].copy_from_slice(&rest[..n]);
        self.pos += n;
        return Ok(n);
    }
}

/// Fill `out` completely. A source that ends early is a truncated payload.
pub fn read_full<S: Source>(src: &mut S, out: &mut [u8]) -> Result<(), PayloadError> {
    let mut filled = 0;
    while filled < out.len() {
        let want = out.len() - filled;
        let n = match src.read(&mut out[filled..(filled + want)]) {
            Err(e) => return Err(e),
            Ok(v) => v,
        };
        if n > want {
            return Err(err(
                Code::device_io,
                "payload source returned more bytes than requested",
            ));
        }
        if n == 0 {
            return Err(err(Code::parse, "payload truncated: source ended early"));
        }
        filled += n;
    }
    return Ok(());
}

/// Require the stream to have ended: any further byte is an unauthenticated
/// trailer, matching the whole-buffer verifier's "bytes after payload
/// signature" rejection.
pub fn expect_end<S: Source>(src: &mut S) -> Result<(), PayloadError> {
    let mut probe: [u8; 1] = [0];
    let n = match src.read(&mut probe) {
        Err(e) => return Err(e),
        Ok(v) => v,
    };
    if n > 0 {
        return Err(err(Code::parse, "bytes after the payload signature"));
    }
    return Ok(());
}
