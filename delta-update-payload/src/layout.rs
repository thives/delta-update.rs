//! Payload header and framing, mirroring `PayloadMetadata::ParsePayloadHeader`
//! from the golden reference. The layout (major version 2) is:
//!
//!   "CrAU" | u64 major | u64 manifest_size | u32 metadata_sig_size
//!   | manifest proto | metadata Signatures message
//!   | blobs (blob offsets recorded in the manifest)
//!   | payload Signatures message (size recorded in the manifest)
//!
//! Framing and decoding are separate steps. [`parse_header`] and [`frame`]
//! slice the raw bytes and check every declared size against the resource
//! profile, without decoding anything. The verifier then authenticates the
//! metadata range before [`locate`] and the manifest decoder are allowed to
//! interpret it.

use crate::error::{Code, PayloadError, err};
use crate::manifest::Manifest;
use crate::manifest::decode_manifest;
use crate::profile::ResourceProfile;

pub const MAGIC: [u8; 4] = [0x43, 0x72, 0x41, 0x55];
pub const MAJOR_VERSION: u64 = 2;

pub const HEADER_SIZE: usize = 24;

/// Capability revision of the byte-range format (`docs/format.md` §2). It sets
/// the meaning of every extent, the replay order, and the resume contract. The
/// reference's maximum is 9, so a reference client refuses this minor before
/// writing anything. A project client before this revision refuses it too, and
/// so does this one for any other value: there is one revision in use, so a
/// payload is always read under the semantics it was generated for.
pub const PROJECT_MINOR: u32 = 17;

/// Maximum minor version this implementation can apply.
pub const MAX_SUPPORTED_MINOR: u32 = PROJECT_MINOR;

pub struct PayloadInfo {
    pub major: u64,
    pub manifest_size: u64,
    pub metadata_size: u64,
    pub meta_sig_size: u32,
    pub blobs_start: u64,
    pub has_sig: bool,
    pub sig_offset_blobs: u64,
    pub sig_size: u64,
    pub sig_msg_offset: u64,
    pub payload_size: u64,
}

/// A decoded manifest together with the layout it was found in.
pub struct Parsed {
    pub info: PayloadInfo,
    pub manifest: Manifest,
}

/// A payload whose metadata signature has been verified. Only the verifier
/// constructs this type, so holding one is proof of authentication.
pub struct Authenticated {
    parsed: Parsed,
}

impl Authenticated {
    pub(crate) fn new(parsed: Parsed) -> Authenticated {
        return Authenticated { parsed: parsed };
    }

    pub fn parsed(&self) -> &Parsed {
        return &self.parsed;
    }

    pub fn into_parsed(self) -> Parsed {
        return self.parsed;
    }
}

/// A payload decoded without signature checks. The caller has chosen to trust
/// nothing about its metadata beyond the structural and policy checks.
pub struct Unauthenticated {
    parsed: Parsed,
}

impl Unauthenticated {
    pub fn parsed(&self) -> &Parsed {
        return &self.parsed;
    }

    pub fn into_parsed(self) -> Parsed {
        return self.parsed;
    }
}

fn be64(data: &[u8], off: usize) -> u64 {
    let mut v = 0;
    for i in 0..8 {
        v = (v << 8) | (data[off + i] as u64);
    }
    v
}

fn be32(data: &[u8], off: usize) -> u32 {
    let mut v = 0;
    for i in 0..4 {
        v = (v << 8) | (data[off + i] as u32);
    }
    v
}

/// The fixed header fields, decoded on their own so the whole-payload parser
/// and the streaming reader (`delta_update_client::stream`) share exactly one
/// definition of "where the manifest starts".
pub struct HeaderInfo {
    pub major: u64,
    pub manifest_size: u64,
    pub meta_sig_size: u32,
}

/// Decode just the 24-byte header under `profile`. `data` may be longer: only
/// the first `HEADER_SIZE` bytes are read. Both declared sizes are checked
/// here, so no caller sizes a buffer from an unchecked header value.
pub fn parse_header(data: &[u8], profile: &ResourceProfile) -> Result<HeaderInfo, PayloadError> {
    if data.len() < HEADER_SIZE {
        return Err(err(Code::parse, "payload shorter than header"));
    }
    for i in 0..4 {
        if data[i] != MAGIC[i] {
            return Err(err(Code::bad_magic, "invalid delta magic"));
        }
    }
    let major = be64(data, 4);
    if major != MAJOR_VERSION {
        return Err(err(
            Code::unsupported_major_version,
            "unsupported payload major version",
        ));
    }
    let manifest_size = be64(data, 12);
    if manifest_size > (profile.max_encoded_manifest as u64) {
        return Err(err(Code::manifest_too_big, "manifest too big"));
    }
    let meta_sig_size = be32(data, 20);
    if (meta_sig_size as u64) > (profile.max_signature_message as u64) {
        return Err(err(
            Code::resource_limit,
            "metadata signature exceeds the profile",
        ));
    }
    return Ok(HeaderInfo {
        major: major,
        manifest_size: manifest_size,
        meta_sig_size: meta_sig_size,
    });
}

/// A payload split into its regions, none of them decoded.
pub struct Frame<'a> {
    pub header: HeaderInfo,
    /// Header plus manifest: the range the metadata signature covers.
    pub metadata: &'a [u8],
    /// The manifest bytes alone.
    pub manifest: &'a [u8],
    /// The metadata `Signatures` message.
    pub meta_sig: &'a [u8],
    /// Blobs, followed by the payload `Signatures` message.
    pub rest: &'a [u8],
}

/// Split a complete payload into regions under `profile`. Nothing is decoded,
/// so the framing cannot be influenced by manifest contents.
pub fn frame<'a>(payload: &'a [u8], profile: &ResourceProfile) -> Result<Frame<'a>, PayloadError> {
    let header = match parse_header(payload, profile) {
        Err(e) => return Err(e),
        Ok(v) => v,
    };
    let metadata_end = match (HEADER_SIZE as u64).checked_add(header.manifest_size) {
        None => return Err(err(Code::overflow, "manifest size overflows")),
        Some(v) => v,
    };
    if (payload.len() as u64) < metadata_end {
        return Err(err(Code::overflow, "payload shorter than metadata"));
    }
    let sig_end = metadata_end + (header.meta_sig_size as u64);
    if (payload.len() as u64) < sig_end {
        return Err(err(
            Code::overflow,
            "payload shorter than metadata signature",
        ));
    }
    let metadata_end = metadata_end as usize;
    let sig_end = sig_end as usize;
    return Ok(Frame {
        header: header,
        metadata: &payload[..metadata_end],
        manifest: &payload[HEADER_SIZE..metadata_end],
        meta_sig: &payload[metadata_end..sig_end],
        rest: &payload[sig_end..],
    });
}

/// Locate the blobs and payload signature from a decoded manifest. Only call
/// this on an authenticated manifest, or on one the caller has chosen to
/// trust, because `signatures_offset` and `signatures_size` come from it.
///
/// This is where the end-of-payload invariant lives, for every trust mode: a
/// declared signature region must end exactly at the payload's end. A region
/// that runs past the end is a truncation (`overflow`); bytes after it are a
/// trailer (`parse`). An undeclared signature leaves the framing to the blob
/// length check in the callers.
pub fn locate(frame: &Frame, manifest: &Manifest) -> Result<PayloadInfo, PayloadError> {
    let metadata_size = (HEADER_SIZE as u64) + frame.header.manifest_size;
    let blobs_start = metadata_size + (frame.header.meta_sig_size as u64);
    let payload_size = blobs_start + (frame.rest.len() as u64);
    let has_sig = !(manifest.signatures_offset.is_none() || manifest.signatures_size.is_none());
    let mut sig_offset_blobs = 0;
    let mut sig_size = 0;
    let mut sig_msg_offset = payload_size;
    if has_sig {
        sig_offset_blobs = manifest.signatures_offset.unwrap_or(0);
        sig_size = manifest.signatures_size.unwrap_or(0);
        sig_msg_offset = blobs_start
            .checked_add(sig_offset_blobs)
            .ok_or_else(|| err(Code::overflow, "signature offset overflow"))?;
        let sig_end = sig_msg_offset
            .checked_add(sig_size)
            .ok_or_else(|| err(Code::overflow, "signature size overflow"))?;
        if payload_size < sig_end {
            return Err(err(Code::overflow, "signature message out of range"));
        }
        if payload_size != sig_end {
            return Err(err(Code::parse, "bytes after payload signature"));
        }
    }
    return Ok(PayloadInfo {
        major: frame.header.major,
        manifest_size: frame.header.manifest_size,
        metadata_size: metadata_size,
        meta_sig_size: frame.header.meta_sig_size,
        blobs_start: blobs_start,
        has_sig: has_sig,
        sig_offset_blobs: sig_offset_blobs,
        sig_size: sig_size,
        sig_msg_offset: sig_msg_offset,
        payload_size: payload_size,
    });
}

/// Frame and decode a payload without verifying anything. Used by the
/// unverified entry points and by tests that inspect layout. The result is
/// structurally checked but carries no authenticity guarantee.
pub fn parse_unverified(
    payload: &[u8],
    profile: &ResourceProfile,
) -> Result<Unauthenticated, PayloadError> {
    let f = match frame(payload, profile) {
        Err(e) => return Err(e),
        Ok(v) => v,
    };
    let manifest = match decode_manifest(f.manifest, profile) {
        Err(e) => return Err(e),
        Ok(v) => v,
    };
    let info = match locate(&f, &manifest) {
        Err(e) => return Err(e),
        Ok(v) => v,
    };
    return Ok(Unauthenticated {
        parsed: Parsed {
            info: info,
            manifest: manifest,
        },
    });
}
