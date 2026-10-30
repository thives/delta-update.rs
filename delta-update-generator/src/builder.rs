//! Hand-shaped payloads: an operation list the planner never emits.
//!
//! [`build_payload`](crate::build_payload) diffs two images and emits what its
//! planner chooses. The format allows more: byte ranges at any offset, destinations
//! in any order and in any overlap, in-target copies, and partial final chunks.
//! This builder encodes such an operation list the way the generator encodes its
//! own operations, so tests can state a delta directly and check the client
//! against an independent model of it.
//!
//! The builder does not check what it produces. A payload meant to be refused by
//! the client's policy is built the same way as one that is meant to apply, so the
//! refusal is the client's, not the builder's.

use crate::seal;
use delta_update_payload::encoding::map_len;
use delta_update_payload::encoding::push_varint;
use delta_update_payload::error::Code;
use delta_update_payload::error::PayloadError;
use delta_update_payload::error::err;
use delta_update_payload::layout;
use delta_update_payload::manifest::Extent;
use delta_update_payload::manifest::Manifest;
use delta_update_payload::manifest::Operation;
use delta_update_payload::manifest::PartInfo;
use delta_update_payload::manifest::PartitionUpdate;
use delta_update_payload::manifest::TYPE_REPLACE;
use delta_update_payload::manifest::TYPE_REPLACE_FRAMES;
use delta_update_payload::manifest::TYPE_REPLACE_SPARSE;
use delta_update_payload::manifest::TYPE_SOURCECOPY;
use delta_update_payload::manifest::TYPE_TARGET_COPY;
use delta_update_payload::manifest::TYPE_ZERO;
use delta_update_payload::rsa;
use delta_update_payload::sha256;
use delta_update_payload::sign;
use miniz_oxide::deflate::compress_to_vec_zlib;
use prost::Message;

/// What an operation does with its destination.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Kind {
    /// Raw bytes: the destination's logical stream is `data`.
    Replace,
    /// Zeros over the destination.
    Zero,
    /// A copy from the old image, read through the `OldImage` capability.
    SourceCopy,
    /// A copy within the target. Its source must not intersect its destination.
    TargetCopy,
    /// Compressed chunks, one frame or raw chunk each.
    Frames,
    /// Compressed chunks with keep, zero and raw modes, chosen per chunk.
    Sparse,
}

/// One operation of a hand-shaped payload.
#[derive(Clone, Debug)]
pub struct Op {
    pub kind: Kind,
    /// Destination byte ranges `(start, length)`, in declared order. Overlap and
    /// order are the caller's to choose.
    pub dst: Vec<(u64, u64)>,
    /// Copy source byte ranges, in declared order.
    pub src: Vec<(u64, u64)>,
    /// The destination's logical bytes for `Replace`, `Frames` and `Sparse`. Its
    /// length must equal the destination's total length.
    pub data: Vec<u8>,
    /// `Sparse` only: which chunks are kept. A kept chunk is not written, so the
    /// destination keeps whatever it holds there. An empty list keeps nothing.
    pub keep: Vec<bool>,
    /// Copies only: the SHA-256 the copy's source must match, as the copy checks
    /// it before its first write.
    pub src_hash: [u8; sha256::SHA256_SIZE],
}

impl Op {
    /// An operation with no source, data or keep flags.
    pub fn new(kind: Kind, dst: Vec<(u64, u64)>) -> Op {
        return Op {
            kind: kind,
            dst: dst,
            src: Vec::new(),
            data: Vec::new(),
            keep: Vec::new(),
            src_hash: [0; sha256::SHA256_SIZE],
        };
    }

    fn total(&self) -> Result<u64, PayloadError> {
        let mut sum: u64 = 0;
        for (_, n) in &self.dst {
            sum = match sum.checked_add(*n) {
                None => return Err(err(Code::overflow, "operation destination total overflows")),
                Some(v) => v,
            };
        }
        return Ok(sum);
    }
}

/// A payload: its chunk size, the partition size it records, and its operations.
pub struct Spec<'a> {
    /// The codec chunk size, which is the manifest's `block_size`.
    pub chunk: usize,
    /// The partition size recorded as both the old and the new size.
    pub partition: u64,
    /// The SHA-256 of the old partition, recorded as its hash when set. A fresh
    /// device is checked against it before its first write.
    pub old_hash: Option<[u8; sha256::SHA256_SIZE]>,
    pub ops: Vec<Op>,
    /// The fixture's private key, when the payload is to be signed.
    pub sign: Option<&'a [u8]>,
}

/// Encode a payload from `spec`. The output is framed as the generator frames its
/// own, and is signed when `spec.sign` is set.
pub fn build(spec: &Spec) -> Result<Vec<u8>, PayloadError> {
    if spec.chunk == 0 || spec.chunk > u32::MAX as usize {
        return Err(err(
            Code::unsupported_block_size,
            "chunk size is out of range",
        ));
    }
    let mut blobs: Vec<u8> = Vec::new();
    let mut operations: Vec<Operation> = Vec::new();
    for op in &spec.ops {
        let encoded = match encode(op, spec.chunk) {
            Err(e) => return Err(e),
            Ok(v) => v,
        };
        operations.push(Operation {
            r#type: encoded.kind,
            data_offset: Some(blobs.len() as u64),
            data_length: Some(encoded.blob.len() as u64),
            src_extents: extents(&op.src),
            dst_extents: extents(&op.dst),
            data_sha256_hash: encoded.hash,
            src_sha256_hash: encoded.src_hash,
            frame_table: encoded.table,
            ..Default::default()
        });
        blobs.extend_from_slice(&encoded.blob);
    }

    let mut manifest = Manifest {
        block_size: Some(spec.chunk as u32),
        minor_version: Some(layout::PROJECT_MINOR),
        partitions: vec![PartitionUpdate {
            partition_name: "A/B".into(),
            operations: operations,
            old_partition_info: Some(PartInfo {
                size: Some(spec.partition),
                hash: spec.old_hash.map(|h| h.to_vec()),
            }),
            new_partition_info: Some(PartInfo {
                size: Some(spec.partition),
                hash: None,
            }),
            ..Default::default()
        }],
        ..Default::default()
    };

    return match spec.sign {
        None => Ok(unsigned(&manifest.encode_to_vec(), &blobs)),
        Some(pem) => {
            let key = match rsa::parse_privkey_pem(pem) {
                Err(e) => return Err(e),
                Ok(k) => k,
            };
            let sig_msg_size = sign::signature_message_size(key.size);
            manifest.signatures_offset = Some(blobs.len() as u64);
            manifest.signatures_size = Some(sig_msg_size as u64);
            seal(&manifest.encode_to_vec(), &blobs, &key, sig_msg_size)
        }
    };
}

fn unsigned(manifest: &[u8], blobs: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&layout::MAGIC);
    out.extend_from_slice(&layout::MAJOR_VERSION.to_be_bytes());
    out.extend_from_slice(&(manifest.len() as u64).to_be_bytes());
    out.extend_from_slice(&0u32.to_be_bytes());
    out.extend_from_slice(manifest);
    out.extend_from_slice(blobs);
    return out;
}

fn extents(list: &[(u64, u64)]) -> Vec<Extent> {
    return list
        .iter()
        .map(|(start, n)| Extent {
            start_byte: Some(*start),
            num_bytes: Some(*n),
        })
        .collect();
}

struct Encoded {
    kind: i32,
    blob: Vec<u8>,
    table: Option<Vec<u8>>,
    hash: Option<Vec<u8>>,
    src_hash: Option<Vec<u8>>,
}

fn encode(op: &Op, chunk: usize) -> Result<Encoded, PayloadError> {
    let total = op.total()?;
    let (kind, blob, table) = match op.kind {
        Kind::Replace => {
            check_data(op, total)?;
            (TYPE_REPLACE, op.data.clone(), None)
        }
        Kind::Zero => (TYPE_ZERO, Vec::new(), None),
        Kind::SourceCopy => (TYPE_SOURCECOPY, Vec::new(), None),
        Kind::TargetCopy => (TYPE_TARGET_COPY, Vec::new(), None),
        Kind::Frames => {
            check_data(op, total)?;
            let (blob, table) = frames(&op.data, chunk);
            (TYPE_REPLACE_FRAMES, blob, Some(table))
        }
        Kind::Sparse => {
            check_data(op, total)?;
            let (blob, packed) = sparse(&op.data, &op.keep, chunk);
            (TYPE_REPLACE_SPARSE, blob, Some(packed))
        }
    };
    let copy = op.kind == Kind::SourceCopy || op.kind == Kind::TargetCopy;
    let hash = match op.kind {
        Kind::Zero | Kind::SourceCopy | Kind::TargetCopy => None,
        _ => Some(sha256::hash_bytes(&blob).to_vec()),
    };
    let src_hash = if copy {
        Some(op.src_hash.to_vec())
    } else {
        None
    };
    return Ok(Encoded {
        kind: kind,
        blob: blob,
        table: table,
        hash: hash,
        src_hash: src_hash,
    });
}

fn check_data(op: &Op, total: u64) -> Result<(), PayloadError> {
    if op.data.len() as u64 != total {
        return Err(err(
            Code::blob_mismatch,
            "builder: data length does not match the destination",
        ));
    }
    return Ok(());
}

fn frames(data: &[u8], chunk: usize) -> (Vec<u8>, Vec<u8>) {
    let mut blob = Vec::new();
    let mut table = Vec::new();
    for raw in data.chunks(chunk) {
        let (stored, zlib) = encode_chunk(raw);
        push_varint(&mut table, if zlib { stored.len() as u64 } else { 0 });
        blob.extend_from_slice(&stored);
    }
    return (blob, table);
}

fn sparse(data: &[u8], keep: &[bool], chunk: usize) -> (Vec<u8>, Vec<u8>) {
    let chunks = data.len().div_ceil(chunk);
    let mut map = std::vec![0u8; map_len(chunks as u64).unwrap_or(0) as usize];
    let mut blob = Vec::new();
    let mut table = Vec::new();
    for (i, raw) in data.chunks(chunk).enumerate() {
        let mode: u8 = if keep.get(i).copied().unwrap_or(false) {
            0
        } else if raw.iter().all(|b| *b == 0) {
            1
        } else {
            let (stored, zlib) = encode_chunk(raw);
            blob.extend_from_slice(&stored);
            if zlib {
                push_varint(&mut table, stored.len() as u64);
                3
            } else {
                2
            }
        };
        map[i / 4] |= mode << (2 * (i % 4));
    }
    let mut packed = map;
    packed.extend_from_slice(&table);
    return (blob, packed);
}

fn encode_chunk(raw: &[u8]) -> (Vec<u8>, bool) {
    let frame = compress_to_vec_zlib(raw, 6);
    if frame.len() < raw.len() {
        return (frame, true);
    }
    return (raw.to_vec(), false);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_destination_total_is_the_sum_of_its_ranges() {
        let op = Op::new(Kind::Zero, vec![(0, 512), (4096, 1024)]);
        assert_eq!(op.total().ok(), Some(1536));
        let edge = Op::new(Kind::Zero, vec![(0, u64::MAX - 1), (0, 1)]);
        assert_eq!(edge.total().ok(), Some(u64::MAX));
    }

    #[test]
    fn an_overflowing_destination_total_is_refused() {
        let op = Op::new(Kind::Zero, vec![(0, u64::MAX), (0, 1)]);
        assert_eq!(op.total().err().map(|e| e.code), Some(Code::overflow));
    }

    #[test]
    fn build_refuses_an_overflowing_destination_without_allocating_its_data() {
        let spec = Spec {
            chunk: 512,
            partition: 4096,
            old_hash: None,
            ops: vec![Op::new(Kind::Zero, vec![(0, u64::MAX), (0, 1)])],
            sign: None,
        };
        assert_eq!(build(&spec).err().map(|e| e.code), Some(Code::overflow));
    }
}
