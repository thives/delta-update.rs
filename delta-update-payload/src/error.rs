//! Error codes, mirroring the subset of `ErrorCode` in the golden
//! reference that applies to payload parsing, signing and application.

#[allow(nonstandard_style)]
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Code {
    parse,
    bad_magic,
    unsupported_major_version,
    unsupported_minor_version,
    unsupported_block_size,
    manifest_too_big,
    overflow,
    bad_protobuf,
    required_field_missing,
    unknown_operation,
    extent_too_many,
    sparse_hole_unsupported,
    resource_limit,
    hash_length,
    unsupported_field,

    signature_missing,
    signature_mismatch,
    key_unsupported,
    key_too_small,

    partition_mismatch,
    blob_mismatch,
    frame_table_invalid,
    block_map_invalid,
    frame_malformed,
    extent_overlap,
    extent_out_of_range,
    hash_mismatch,
    device_io,
    buffer_too_small,
    source_missing,
    source_geometry,
    source_mismatch,
    device_geometry,
    copy_overlap,
    target_not_readable,
    checkpoint_invalid,
    checkpoint_mismatch,
    checkpoint_removal_uncertain,
    checkpoint_present,
    checkpoint_unconfirmed,
    no_compute_device,

    rollback_refused,
    recovery_required,
}

#[derive(Debug)]
pub struct PayloadError {
    pub code: Code,
    pub msg: &'static str,
}

pub fn err(code: Code, msg: &'static str) -> PayloadError {
    PayloadError { code, msg }
}
