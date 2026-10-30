//! Full-image generation: a signed payload that writes a whole partition image from its own bytes.
//!
//! The delta generator needs the old image, and its search needs a Vulkan device. A full-image payload
//! needs neither. It records only the new image, so it updates the new-image prefix of any sufficiently
//! large target whatever it holds: zeros, erased flash, or unrelated data. Trailing bytes stay unchanged.
//!
//! The construction is simple and deterministic, and it is not a search. Two candidates are built from
//! the image, and the smaller admissible one is signed:
//!
//! - **raw**: one `REPLACE` over the whole image;
//! - **framed**: the image's chunks in order. Each maximal run of all-zero chunks is one `ZERO`, and
//!   every other run is `REPLACE_FRAMES`, with each chunk stored as a zlib frame when that is smaller and
//!   raw otherwise. A framed run is cut into several operations wherever its frame table would pass the
//!   profile's limit.
//!
//! Each candidate is admitted under the profile by the client's own metadata policy, and its size is the
//! complete encoded payload: header, manifest, both signatures and blobs. A candidate the profile refuses
//! is dropped, and when neither fits the build fails with a resource-limit error. It never retries under a
//! different profile. The signed bytes are checked by [`check::check_full_payload`] before they are returned.

use ota_payload::encoding::push_varint;
use ota_payload::error::Code;
use ota_payload::error::PayloadError;
use ota_payload::error::err;
use ota_payload::manifest::Operation;
use ota_payload::manifest::PartInfo;
use ota_payload::manifest::TYPE_REPLACE_FRAMES;
use ota_payload::manifest::TYPE_ZERO;
use ota_payload::profile::ResourceProfile;
use ota_payload::rsa;
use ota_payload::sha256;
use prost::Message;

use crate::check;
use crate::checked_block_size;
use crate::control::Control;
use crate::control::Halt;
use crate::evaluate;
use crate::manifest_of;
use crate::mapper::raw_replace_wire;
use crate::mapper::stored_frame;
use crate::mapper::wire;
use crate::part_info;
use crate::program::Span;
use crate::seal;

const LEVEL: u8 = 6;

struct Candidate {
    ops: Vec<Operation>,
    blobs: Vec<u8>,
}

/// Build the signed full-image payload for `new`, named `name`, under `key`.
///
/// The caller has checked the partition name. The block size is checked here, before any candidate is cut
/// into chunks of it, so a direct call with a size the builder refuses returns an error and never reaches
/// the chunking, which would panic at zero. The key's signature messages are checked against the profile
/// here, before any candidate is built, as the delta search checks them: a key the profile refuses cannot
/// sign a payload it would accept.
pub fn build(
    new: &[u8],
    name: &str,
    key: &rsa::PrivKey,
    block_size: usize,
    profile: &ResourceProfile,
    release_version: u64,
) -> Result<Vec<u8>, PayloadError> {
    match checked_block_size(block_size, profile) {
        Err(e) => return Err(e),
        Ok(()) => {}
    };
    let sig_msg_size = match evaluate::signature_message(key.size, profile) {
        Err(e) => return Err(e),
        Ok(s) => s,
    };
    let info = part_info(new);
    let cand = match choose(new, &info, name, block_size, sig_msg_size, profile, release_version) {
        Err(e) => return Err(e),
        Ok(c) => c,
    };
    let blob_len = cand.blobs.len();
    let manifest = manifest_of(
        name,
        cand.ops,
        None,
        info,
        block_size,
        blob_len as u64,
        sig_msg_size,
        release_version,
    )
    .encode_to_vec();
    let out = match seal(&manifest, &cand.blobs, key, sig_msg_size) {
        Err(e) => return Err(e),
        Ok(v) => v,
    };
    match check::check_full_payload(&out, rsa::public_key_of(key), new, profile) {
        Err(e) => return Err(e),
        Ok(()) => {}
    };
    return Ok(out);
}

fn choose(
    new: &[u8],
    info: &PartInfo,
    name: &str,
    block_size: usize,
    sig_msg_size: usize,
    profile: &ResourceProfile,
    release_version: u64,
) -> Result<Candidate, PayloadError> {
    let framed = match framed(new, runs(new, block_size), block_size, profile) {
        Err(e) => return Err(e),
        Ok(c) => c,
    };
    let mut best: Option<(u64, Candidate)> = None;
    for cand in [Some(raw(new)), framed].into_iter().flatten() {
        let size = match admitted(&cand, info, name, block_size, sig_msg_size, profile, release_version) {
            Err(_) => continue,
            Ok(s) => s,
        };
        let smaller = match &best {
            None => true,
            Some((kept, _)) => size < *kept,
        };
        if smaller {
            best = Some((size, cand));
        }
    }
    return match best {
        None => Err(err(
            Code::resource_limit,
            "no full-image candidate fits the resource profile",
        )),
        Some((_, cand)) => Ok(cand),
    };
}

fn admitted(
    cand: &Candidate,
    info: &PartInfo,
    name: &str,
    block_size: usize,
    sig_msg_size: usize,
    profile: &ResourceProfile,
    release_version: u64,
) -> Result<u64, PayloadError> {
    let blob_len = cand.blobs.len() as u64;
    let manifest = manifest_of(
        name,
        cand.ops.clone(),
        None,
        info.clone(),
        block_size,
        blob_len,
        sig_msg_size,
        release_version,
    );
    return evaluate::admitted_size(manifest, profile, sig_msg_size, blob_len);
}

fn raw(new: &[u8]) -> Candidate {
    if new.is_empty() {
        return Candidate {
            ops: Vec::new(),
            blobs: Vec::new(),
        };
    }
    let len = new.len() as u64;
    return Candidate {
        ops: vec![raw_replace_wire(
            &[Span { start: 0, len: len }],
            0,
            len,
            sha256::hash_bytes(new).to_vec(),
        )],
        blobs: new.to_vec(),
    };
}

fn framed(
    new: &[u8],
    mut runs: impl Iterator<Item = (usize, usize, bool)>,
    chunk: usize,
    profile: &ResourceProfile,
) -> Result<Option<Candidate>, PayloadError> {
    let mut cand = Candidate {
        ops: Vec::new(),
        blobs: Vec::new(),
    };
    let mut at = 0usize;
    loop {
        if at < new.len() && !room_for_op(cand.ops.len(), profile) {
            return Ok(None);
        }
        let Some((start, end, zero)) = runs.next() else {
            return Ok(Some(cand));
        };
        at = end;
        if zero {
            let cursor = cand.blobs.len() as u64;
            let dst = [span(start, end)];
            cand.ops
                .push(wire(TYPE_ZERO, &dst, &[], cursor, 0, None, None, None));
        } else {
            match frames(&mut cand, new, start, end, chunk, profile) {
                Err(e) => return Err(e),
                Ok(false) => return Ok(None),
                Ok(true) => {}
            };
        }
    }
}

fn room_for_op(ops: usize, profile: &ResourceProfile) -> bool {
    let next = ops + 1;
    return next <= profile.max_operations
        && next <= profile.max_total_extents
        && profile.max_extents_per_op >= 1;
}

fn runs(new: &[u8], chunk: usize) -> impl Iterator<Item = (usize, usize, bool)> + '_ {
    let mut pieces = new.chunks(chunk).peekable();
    let mut start = 0usize;
    return std::iter::from_fn(move || {
        let zero = is_zero(pieces.peek()?);
        let mut end = start;
        while let Some(piece) = pieces.next_if(|p| is_zero(p) == zero) {
            end += piece.len();
        }
        let run = (start, end, zero);
        start = end;
        return Some(run);
    });
}

fn is_zero(piece: &[u8]) -> bool {
    return piece.iter().all(|&b| b == 0);
}

fn frames(
    cand: &mut Candidate,
    new: &[u8],
    start: usize,
    end: usize,
    chunk: usize,
    profile: &ResourceProfile,
) -> Result<bool, PayloadError> {
    let ctl = Control::unbounded();
    let mut op_start = start;
    let mut table: Vec<u8> = Vec::new();
    let mut body: Vec<u8> = Vec::new();
    let mut at = start;
    while at < end {
        let stop = if end - at < chunk { end } else { at + chunk };
        let raw = &new[at..stop];
        let frame = match stored_frame(raw, LEVEL, &ctl) {
            Err(Halt::Failed(e)) => return Err(e),
            Err(Halt::Deadline) => {
                return Err(err(Code::resource_limit, "compression was interrupted"));
            }
            Ok(f) => f,
        };
        let mut entry: Vec<u8> = Vec::new();
        push_varint(&mut entry, frame.as_ref().map_or(0, |f| f.len() as u64));
        if at > op_start && table.len() + entry.len() > profile.max_frame_table_bytes {
            push_frames(cand, op_start, at, &mut table, &mut body);
            if !room_for_op(cand.ops.len(), profile) {
                return Ok(false);
            }
            op_start = at;
        }
        table.extend(entry);
        match frame {
            None => body.extend_from_slice(raw),
            Some(f) => body.extend(f),
        };
        at = stop;
    }
    push_frames(cand, op_start, end, &mut table, &mut body);
    return Ok(true);
}

fn push_frames(
    cand: &mut Candidate,
    start: usize,
    end: usize,
    table: &mut Vec<u8>,
    body: &mut Vec<u8>,
) {
    let cursor = cand.blobs.len() as u64;
    let hash = sha256::hash_bytes(body).to_vec();
    let dst = [span(start, end)];
    cand.ops.push(wire(
        TYPE_REPLACE_FRAMES,
        &dst,
        &[],
        cursor,
        body.len() as u64,
        Some(hash),
        None,
        Some(std::mem::take(table)),
    ));
    cand.blobs.append(body);
}

fn span(start: usize, end: usize) -> Span {
    return Span {
        start: start as u64,
        len: (end - start) as u64,
    };
}

#[cfg(test)]
mod tests {
    use super::*;
    use ota_payload::profile::EMBEDDED;

    fn with_ops(ops: usize) -> ResourceProfile {
        return ResourceProfile {
            max_operations: ops,
            ..EMBEDDED
        };
    }

    fn alternating(n: usize) -> Vec<u8> {
        return (0..n).map(|k| if k % 2 == 0 { 0 } else { 1 }).collect();
    }

    #[test]
    fn runs_join_chunks_of_one_kind_and_keep_the_partial_last_chunk() {
        let mut new = vec![0u8; 16];
        new[8..12].fill(7);
        new.extend_from_slice(&[1, 2]);
        assert_eq!(
            runs(&new, 4).collect::<Vec<_>>(),
            vec![
                (0, 8, true),
                (8, 12, false),
                (12, 16, true),
                (16, 18, false)
            ]
        );
    }

    #[test]
    fn each_count_limit_admits_its_own_value_and_no_more() {
        let ops = ResourceProfile {
            max_operations: 3,
            ..EMBEDDED
        };
        assert!(room_for_op(2, &ops));
        assert!(!room_for_op(3, &ops));
        let extents = ResourceProfile {
            max_total_extents: 3,
            ..EMBEDDED
        };
        assert!(room_for_op(2, &extents));
        assert!(!room_for_op(3, &extents));
        let per_op = ResourceProfile {
            max_extents_per_op: 0,
            ..EMBEDDED
        };
        assert!(!room_for_op(0, &per_op));
    }

    #[test]
    fn a_zero_run_past_the_operation_limit_rejects_the_candidate() {
        let zero_fifth = alternating(5);
        let refused = framed(&zero_fifth, runs(&zero_fifth, 1), 1, &with_ops(4))
            .unwrap_or_else(|e| panic!("{}", e.msg));
        assert!(refused.is_none());
    }

    #[test]
    fn a_data_run_past_the_operation_limit_rejects_the_candidate() {
        let data_fifth = alternating(6)[1..].to_vec();
        let refused = framed(&data_fifth, runs(&data_fifth, 1), 1, &with_ops(4))
            .unwrap_or_else(|e| panic!("{}", e.msg));
        assert!(refused.is_none());
    }

    #[test]
    fn a_frame_table_split_past_the_operation_limit_rejects_the_candidate() {
        let new: Vec<u8> = (0..32u32)
            .map(|k| (k.wrapping_mul(2654435761) >> 24) as u8 | 1)
            .collect();
        let two = ResourceProfile {
            max_frame_table_bytes: 3,
            ..with_ops(2)
        };
        let refused = framed(&new, runs(&new, 4), 4, &two).unwrap_or_else(|e| panic!("{}", e.msg));
        assert!(refused.is_none(), "the split would be the third operation");
        let three = ResourceProfile {
            max_operations: 3,
            ..two
        };
        let fits = framed(&new, runs(&new, 4), 4, &three).unwrap_or_else(|e| panic!("{}", e.msg));
        assert_eq!(fits.map(|c| c.ops.len()), Some(3));
    }

    #[test]
    fn a_refused_candidate_reads_no_run_from_the_one_it_refuses() {
        let new = alternating(1000);
        let mut drawn = 0;
        let refused = framed(&new, runs(&new, 1).inspect(|_| drawn += 1), 1, &with_ops(3))
            .unwrap_or_else(|e| panic!("{}", e.msg));
        assert!(refused.is_none());
        assert_eq!(
            drawn, 3,
            "three runs fit, and the fourth is refused before it is read"
        );
    }

    #[test]
    fn a_candidate_that_covers_the_image_at_the_limit_reads_no_further_run() {
        let new = alternating(4);
        let mut drawn = 0;
        let cand = framed(&new, runs(&new, 1).inspect(|_| drawn += 1), 1, &with_ops(4))
            .unwrap_or_else(|e| panic!("{}", e.msg));
        assert_eq!(cand.map(|c| c.ops.len()), Some(4));
        assert_eq!(drawn, 4);
    }

    #[test]
    fn a_frame_table_that_reaches_the_budget_closes_its_operation() {
        let new: Vec<u8> = (0..32u32)
            .map(|k| (k.wrapping_mul(2654435761) >> 24) as u8 | 1)
            .collect();
        let profile = ResourceProfile {
            max_frame_table_bytes: 3,
            ..EMBEDDED
        };
        let mut cand = Candidate {
            ops: Vec::new(),
            blobs: Vec::new(),
        };
        match frames(&mut cand, &new, 0, 32, 4, &profile) {
            Err(e) => panic!("{}", e.msg),
            Ok(fits) => assert!(fits),
        };
        let sizes: Vec<usize> = cand
            .ops
            .iter()
            .map(|op| op.frame_table.clone().unwrap_or_default().len())
            .collect();
        assert_eq!(sizes, vec![3, 3, 2]);
        assert_eq!(cand.blobs, new);
    }

    #[test]
    fn the_direct_builder_refuses_a_block_size_it_cannot_cut_by() {
        const PRIV: &[u8] = include_bytes!("../../ota-payload/tests/fixtures/private.pem");
        let key = rsa::parse_privkey_pem(PRIV).ok().unwrap();
        let new = vec![7u8; 64];
        for block_size in [0, 8192] {
            let refused = build(&new, "A/B", &key, block_size, &EMBEDDED, 0);
            assert_eq!(
                refused.err().map(|e| e.code),
                Some(Code::unsupported_block_size),
                "block size {block_size}"
            );
        }
    }
}
