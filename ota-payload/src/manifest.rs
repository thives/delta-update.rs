//! Manifest messages are generated from the golden schema by `prost-build`
//! (see `build.rs` and the `schema` module), preserving proto2 field
//! numbers, `required` fields and field defaults so the encoding is byte-
//! compatible with upstream update_engine. Unknown fields are tolerated
//! and skipped, as protobuf does.
//!
//! Decoding is two steps. [`prescan`](crate::prescan) first walks the wire
//! bytes against the [`ResourceProfile`] and rejects anything oversized,
//! unsupported, malformed, or missing a required field, all without
//! allocating. Only then does prost build the owned messages. Semantic policy
//! (operation ordering, extents, blob layout) is a separate pass in
//! [`plan`](crate::plan), run on the decoded value.

use crate::error::Code;
use crate::error::PayloadError;
use crate::error::err;
use crate::prescan;
use crate::profile::EXTENT_COST;
use crate::profile::MANIFEST_COST;
use crate::profile::OP_COST;
use crate::profile::PARTITION_COST;
use crate::profile::ResourceProfile;
use crate::profile::SIGNATURE_COST;
use crate::profile::SIGNATURES_COST;
use prost::Message;

pub const TYPE_REPLACE: i32 = 0;
pub const TYPE_REPLACEXZ: i32 = 8;
pub const TYPE_ZERO: i32 = 6;
pub const TYPE_DISCARD: i32 = 7;
pub const TYPE_SOURCECOPY: i32 = 4;
pub const TYPE_SOURCEBSDIFF: i32 = 5;
pub const TYPE_REPLACE_FRAMES: i32 = 256;
pub const TYPE_REPLACE_SPARSE: i32 = 257;
/// Copy within the target itself (docs/format.md §5.1). The one operation whose
/// source is the device being written.
pub const TYPE_TARGET_COPY: i32 = 258;

pub use crate::schema::signatures::Signature;
pub use crate::schema::{
    DeltaArchiveManifest as Manifest, Extent, InstallOperation as Operation,
    PartitionInfo as PartInfo, PartitionUpdate, Signatures,
};

const _: () = assert!(core::mem::size_of::<Operation>() <= OP_COST);
const _: () = assert!(core::mem::size_of::<Extent>() <= EXTENT_COST);
const _: () = assert!(core::mem::size_of::<PartitionUpdate>() <= PARTITION_COST);
const _: () = assert!(core::mem::size_of::<Manifest>() <= MANIFEST_COST);
const _: () = assert!(core::mem::size_of::<Signature>() <= SIGNATURE_COST);
const _: () = assert!(core::mem::size_of::<Signatures>() <= SIGNATURES_COST);

/// Decode a complete `DeltaArchiveManifest` under `profile`. The bytes are
/// scanned before prost allocates anything, so every allocation is bounded.
///
/// Callers must have authenticated `data` first when the payload is verified:
/// this function checks shape and limits, not provenance.
pub fn decode_manifest(data: &[u8], profile: &ResourceProfile) -> Result<Manifest, PayloadError> {
    match prescan::scan_manifest(data, profile) {
        Err(e) => return Err(e),
        Ok(_) => {}
    };
    return match Manifest::decode(data) {
        Err(_) => Err(err(Code::bad_protobuf, "manifest wire parse error")),
        Ok(m) => Ok(m),
    };
}

/// Decode a `Signatures` message (metadata or payload signature block) under
/// `profile`, with the same scan-before-decode rule as [`decode_manifest`].
pub fn decode_signatures(data: &[u8], profile: &ResourceProfile) -> Result<Signatures, PayloadError> {
    match prescan::scan_signatures(data, profile) {
        Err(e) => return Err(e),
        Ok(_) => {}
    };
    return match Signatures::decode(data) {
        Err(_) => Err(err(Code::bad_protobuf, "signatures wire parse error")),
        Ok(s) => Ok(s),
    };
}

#[cfg(test)]
mod tests {
    extern crate std;

    use super::*;
    use crate::profile::EMBEDDED;
    use alloc::string::ToString;
    use alloc::vec;
    use alloc::vec::Vec;

    fn extent(start: u64, num: u64) -> Extent {
        Extent {
            start_byte: Some(start),
            num_bytes: Some(num),
        }
    }

    fn op(typ: i32, hash: Option<Vec<u8>>) -> Operation {
        Operation {
            r#type: typ,
            dst_extents: vec![extent(0, 1)],
            data_sha256_hash: hash,
            ..Default::default()
        }
    }

    fn encode(ops: Vec<Operation>) -> Vec<u8> {
        let part = PartitionUpdate {
            partition_name: "A/B".to_string(),
            operations: ops,
            ..Default::default()
        };
        return Manifest {
            block_size: Some(4096),
            partitions: vec![part],
            ..Default::default()
        }
        .encode_to_vec();
    }

    #[test]
    fn decodes_a_well_formed_manifest() {
        let bytes = encode(vec![op(TYPE_REPLACE, Some(vec![1; 32]))]);
        let m = decode_manifest(&bytes, &EMBEDDED).unwrap();
        assert_eq!(m.partitions[0].operations.len(), 1);
    }

    #[test]
    fn hashes_must_be_empty_or_exactly_sha256_length() {
        for len in [1usize, 31, 33, 64] {
            let bytes = encode(vec![op(TYPE_REPLACE, Some(vec![0; len]))]);
            assert!(decode_manifest(&bytes, &EMBEDDED).is_err(), "{}-byte hash", len);
        }
        for hash in [None, Some(Vec::new()), Some(vec![0; 32])] {
            let bytes = encode(vec![op(TYPE_REPLACE, hash)]);
            assert!(decode_manifest(&bytes, &EMBEDDED).is_ok());
        }
    }

    #[test]
    fn operation_count_is_bounded_before_prost_allocates() {
        let ops: Vec<Operation> = (0..EMBEDDED.max_operations + 1)
            .map(|_| op(TYPE_REPLACE, None))
            .collect();
        let bytes = encode(ops);
        match decode_manifest(&bytes, &EMBEDDED) {
            Ok(_) => panic!("operation ceiling not enforced"),
            Err(e) => assert_eq!(e.code, Code::resource_limit),
        }
    }

    #[test]
    fn missing_operation_type_is_rejected_and_explicit_zero_is_replace() {
        let missing = [
            0x6a, 0x07,
            0x0a, 0x03, 0x41, 0x2f, 0x42,
            0x42, 0x00,
        ];
        match decode_manifest(&missing, &EMBEDDED) {
            Ok(_) => panic!("missing type decoded"),
            Err(e) => assert_eq!(e.code, Code::required_field_missing),
        }
        let explicit = [
            0x6a, 0x09,
            0x0a, 0x03, 0x41, 0x2f, 0x42,
            0x42, 0x02, 0x08, 0x00,
        ];
        assert!(decode_manifest(&explicit, &EMBEDDED).is_ok());
    }
}
