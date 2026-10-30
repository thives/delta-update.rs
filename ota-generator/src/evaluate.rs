//! Fitness: the exact payload size of a program, or nothing when the program is not
//! admissible.
//!
//! A program is admissible when it is complete (replay produces the new image),
//! structurally valid, and its manifest passes the client's own pre-decode scan and
//! metadata policy under the profile. Only admissible programs receive a size, and
//! the size is the length of the payload `build_payload` will emit: header, manifest,
//! two signature messages, and blobs. The manifest is measured with the same encoder
//! and placeholder hashes of the final length, so the number is exact.
//!
//! An interrupted evaluation is not an inadmissible one. It returns [`Halt::Deadline`],
//! so the search can tell "this program does not fit" from "the search ran out of time".

use ota_payload::error::Code;
use ota_payload::error::PayloadError;
use ota_payload::error::err;
use ota_payload::layout::HEADER_SIZE;
use ota_payload::manifest::Manifest;
use ota_payload::manifest::Operation;
use ota_payload::manifest::PartInfo;
use ota_payload::plan;
use ota_payload::profile::ResourceProfile;
use ota_payload::prescan::scan_manifest;
use ota_payload::prescan::scan_signatures;
use ota_payload::sign;
use prost::Message;

use crate::control::Control;
use crate::control::Halt;
use crate::control::Step;
use crate::manifest_of;
use crate::mapper::Mapping;
use crate::mapper::placeholder;
use crate::memory::Images;
use crate::program::Kind;
use crate::program::Op;
use crate::program::Program;

/// Everything about one partition update that does not change during a search.
pub struct Problem<'a> {
    /// The images and codec chunk size, with the frames compressed for them.
    pub mapping: Mapping<'a>,
    pub name: &'a str,
    pub profile: &'a ResourceProfile,
    /// The padded size of one signature message under the key.
    pub sig_msg_size: usize,
    /// The signed release version the manifest records (`docs/platform.md` §3). Zero unless the
    /// caller sets it.
    pub release_version: u64,
}

impl<'a> Problem<'a> {
    /// A problem for `old` to `new` with codec chunks of `chunk` bytes, signed by a key whose
    /// signature data is `key_size` bytes. It is refused unless a one-entry signature message of
    /// that key passes the profile's signature scanner, so every search, and every caller of the
    /// search, starts from a problem the client would not refuse on its signatures.
    pub fn new(
        old: &'a [u8],
        new: &'a [u8],
        chunk: usize,
        name: &'a str,
        profile: &'a ResourceProfile,
        key_size: usize,
    ) -> Result<Problem<'a>, PayloadError> {
        let sig_msg_size = match signature_message(key_size, profile) {
            Err(e) => return Err(e),
            Ok(s) => s,
        };
        return Ok(Problem {
            mapping: Mapping::new(Images { old: old, new: new }, chunk),
            name: name,
            profile: profile,
            sig_msg_size: sig_msg_size,
            release_version: 0,
        });
    }
}

pub(crate) fn signature_message(key_size: usize, profile: &ResourceProfile) -> Result<usize, PayloadError> {
    if key_size == 0 {
        return Err(err(Code::resource_limit, "the signing key has no signature data"));
    }
    if key_size > profile.max_signature_data {
        return Err(err(Code::resource_limit, "the signing key's signature data exceeds the profile"));
    }
    let size = match sign::checked_message_size(key_size) {
        Err(e) => return Err(e),
        Ok(s) => s,
    };
    if size > profile.max_signature_message {
        return Err(err(Code::resource_limit, "the signing key's signature message exceeds the profile"));
    }
    let message = match sign::build_signature_message(key_size, &[]) {
        Err(e) => return Err(e),
        Ok(m) => m,
    };
    match scan_signatures(&message, profile) {
        Err(e) => return Err(e),
        Ok(_) => {}
    };
    return Ok(message.len());
}

pub(crate) fn within_count_limits(profile: &ResourceProfile, program: &[Op]) -> bool {
    let extents: usize = program.iter().map(|op| op.dst.len() + op.src.len()).sum();
    return program.len() <= profile.max_operations && extents <= profile.max_total_extents;
}

/// An admissible program and its exact payload size.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Evaluated {
    /// The program the size belongs to. For a repaired program this includes the
    /// repair, so the program and its size always describe the same payload.
    pub program: Program,
    pub size: u64,
}

/// The manifest an admissible program would ship. The hashes are placeholders, and
/// the sizes they occupy do not depend on their values.
pub fn manifest_for(problem: &Problem, ops: Vec<Operation>, blob_len: u64) -> Manifest {
    let images = problem.mapping.images();
    return manifest_of(
        problem.name,
        ops,
        Some(placeholder_info(images.old.len())),
        placeholder_info(images.new.len()),
        problem.mapping.chunk(),
        blob_len,
        problem.sig_msg_size,
        problem.release_version,
    );
}

fn placeholder_info(len: usize) -> PartInfo {
    return PartInfo {
        size: Some(len as u64),
        hash: Some(placeholder()),
    };
}

/// The exact size of the payload that `manifest` and `blob_len` would produce, or an
/// error when the manifest is outside the profile or breaks the metadata policy.
pub fn payload_size(problem: &Problem, manifest: Manifest, blob_len: u64) -> Result<u64, PayloadError> {
    return admitted_size(manifest, problem.profile, problem.sig_msg_size, blob_len);
}

pub(crate) fn admitted_size(
    manifest: Manifest,
    profile: &ResourceProfile,
    sig_msg_size: usize,
    blob_len: u64,
) -> Result<u64, PayloadError> {
    let bytes = manifest.encode_to_vec();
    match scan_manifest(&bytes, profile) {
        Err(e) => return Err(e),
        Ok(_) => {}
    };
    match plan::validate(manifest, profile) {
        Err(e) => return Err(e),
        Ok(_) => {}
    };
    let signatures = 2 * sig_msg_size as u64;
    return Ok(HEADER_SIZE as u64 + bytes.len() as u64 + signatures + blob_len);
}

/// Evaluate a program. A complete, admissible program is returned as it is. An
/// incomplete one is repaired by a single raw replacement over its mismatched bytes,
/// which is appended as the last operation. The repair is kept only when the repaired
/// program is admissible, so it can never exclude a plan that already fit.
///
/// `Ok(None)` means the program is not admissible. `Err(Halt::Deadline)` means the
/// evaluation was interrupted, which says nothing about the program.
pub fn evaluate(problem: &Problem, program: Program, ctl: &Control) -> Step<Option<Evaluated>> {
    if !within_count_limits(problem.profile, &program) {
        return Ok(None);
    }
    let first = match problem.mapping.map(&program, ctl, false) {
        Err(Halt::Deadline) => return Err(Halt::Deadline),
        Err(Halt::Failed(_)) => return Ok(None),
        Ok(m) => m,
    };
    let (program, mapped) = if first.mismatches.is_empty() {
        (program, first)
    } else {
        let mut repaired = program;
        repaired.push(Op::new(Kind::Replace, first.mismatches));
        let second = match problem.mapping.map(&repaired, ctl, false) {
            Err(Halt::Deadline) => return Err(Halt::Deadline),
            Err(Halt::Failed(_)) => return Ok(None),
            Ok(m) => m,
        };
        if !second.mismatches.is_empty() {
            return Ok(None);
        }
        (repaired, second)
    };
    let manifest = manifest_for(problem, mapped.ops, mapped.blob_len);
    return match payload_size(problem, manifest, mapped.blob_len) {
        Err(_) => Ok(None),
        Ok(size) => Ok(Some(Evaluated {
            program: program,
            size: size,
        })),
    };
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::control::Halt;
    use crate::control::test_clocks::Ticking;
    use crate::program::Span;
    use crate::program::raw_baseline;
    use ota_payload::profile::EMBEDDED;
    use ota_payload::rsa::parse_privkey_pem;

    const PRIV: &[u8] = include_bytes!("../../ota-payload/tests/fixtures/private.pem");

    fn s(start: u64, len: u64) -> Span {
        return Span { start: start, len: len };
    }

    fn problem_for<'a>(old: &'a [u8], new: &'a [u8], chunk: usize) -> Problem<'a> {
        return match Problem::new(old, new, chunk, "A/B", &EMBEDDED, 256) {
            Err(e) => panic!("{}", e.msg),
            Ok(p) => p,
        };
    }

    fn admit(problem: &Problem, program: Program) -> Option<Evaluated> {
        return match evaluate(problem, program, &Control::unbounded()) {
            Err(_) => panic!("an unbounded evaluation cannot time out"),
            Ok(e) => e,
        };
    }

    #[test]
    fn an_incomplete_program_is_repaired_by_a_raw_suffix() {
        let old = vec![0u8; 64];
        let mut new = old.clone();
        new[7..19].fill(3);
        let problem = problem_for(&old, &new, 4);
        let found = admit(&problem, Vec::new());
        let found = found.unwrap_or_else(|| panic!("an empty program repairs to a raw replacement"));
        assert_eq!(found.program.len(), 1);
        assert_eq!(found.program[0].kind, Kind::Replace);
        assert_eq!(found.program[0].dst, vec![s(7, 12)]);
    }

    #[test]
    fn a_complete_program_is_not_repaired() {
        let old = vec![1u8; 16];
        let new = old.clone();
        let problem = problem_for(&old, &new, 4);
        let found = admit(&problem, Vec::new());
        let found = found.unwrap_or_else(|| panic!("identical images need no operations"));
        assert!(found.program.is_empty());
    }

    #[test]
    fn an_expired_control_interrupts_an_evaluation_whose_runs_all_match() {
        let old = vec![3u8; 4096];
        let problem = problem_for(&old, &old, 64);
        let clock = Ticking::new(1.0);
        let expired = Control::new(&clock, 0.5, 1.0, 0, None);
        assert!(matches!(evaluate(&problem, Vec::new(), &expired), Err(Halt::Deadline)));
    }

    #[test]
    fn the_raw_baseline_is_admitted_at_the_length_of_the_payload_it_emits() {
        let key = match parse_privkey_pem(PRIV) {
            Err(e) => panic!("{}", e.msg),
            Ok(k) => k,
        };
        for (len, chunk) in [(1usize, 1usize), (17, 4), (4096, 64), (5000, 4096)] {
            let old = noise_bytes(len, 3);
            let mut new = old.clone();
            new[len / 2] ^= 0x5a;
            let problem = problem_for(&old, &new, chunk);
            let program = raw_baseline(len as u64);
            let found = match admit(&problem, program.clone()) {
                None => panic!("a raw replacement of {} bytes must fit the embedded profile", len),
                Some(e) => e,
            };
            assert_eq!(found.program, program);
            let emitted = match crate::assemble(&problem, &program, &key, &old, &new) {
                Err(e) => panic!("len {} chunk {}: {}", len, chunk, e.msg),
                Ok(p) => p,
            };
            assert_eq!(emitted.len() as u64, found.size, "len {} chunk {}", len, chunk);
        }
    }

    #[test]
    fn a_profile_refuses_a_signature_it_cannot_hold_before_any_program_is_considered() {
        let old = vec![0u8; 16];
        let new = old.clone();
        let no_slots = ResourceProfile {
            max_signatures: 0,
            ..EMBEDDED
        };
        assert_eq!(Problem::new(&old, &new, 4, "A/B", &no_slots, 256).err().map(|e| e.code), Some(Code::resource_limit));
        let small_data = ResourceProfile {
            max_signature_data: 255,
            ..EMBEDDED
        };
        assert_eq!(Problem::new(&old, &new, 4, "A/B", &small_data, 256).err().map(|e| e.code), Some(Code::resource_limit));
        let small_message = ResourceProfile {
            max_signature_message: sign::signature_message_size(256) - 1,
            ..EMBEDDED
        };
        assert_eq!(Problem::new(&old, &new, 4, "A/B", &small_message, 256).err().map(|e| e.code), Some(Code::resource_limit));
        let small_budget = ResourceProfile {
            max_decoded_bytes: 64,
            ..EMBEDDED
        };
        assert_eq!(Problem::new(&old, &new, 4, "A/B", &small_budget, 256).err().map(|e| e.code), Some(Code::resource_limit));
        assert!(Problem::new(&old, &new, 4, "A/B", &EMBEDDED, 0).is_err(), "a key with no signature data is refused");
    }

    #[test]
    fn a_profile_admits_a_signature_exactly_at_its_limits() {
        let old = vec![0u8; 16];
        let at_limit = ResourceProfile {
            max_signature_message: sign::signature_message_size(256),
            max_signature_data: 256,
            ..EMBEDDED
        };
        let problem = match Problem::new(&old, &old, 4, "A/B", &at_limit, 256) {
            Err(e) => panic!("{}", e.msg),
            Ok(p) => p,
        };
        assert_eq!(problem.sig_msg_size, sign::signature_message_size(256));
    }

    #[test]
    fn a_signature_past_the_profile_is_refused_before_its_message_is_built() {
        let old = vec![0u8; 16];
        for key_size in [0, EMBEDDED.max_signature_data + 1, usize::MAX] {
            assert_eq!(
                Problem::new(&old, &old, 4, "A/B", &EMBEDDED, key_size).err().map(|e| e.code),
                Some(Code::resource_limit),
                "key {}",
                key_size
            );
        }
        let message_short = ResourceProfile {
            max_signature_message: sign::signature_message_size(EMBEDDED.max_signature_data) - 1,
            ..EMBEDDED
        };
        assert_eq!(
            Problem::new(&old, &old, 4, "A/B", &message_short, EMBEDDED.max_signature_data).err().map(|e| e.code),
            Some(Code::resource_limit)
        );
        assert!(Problem::new(&old, &old, 4, "A/B", &EMBEDDED, EMBEDDED.max_signature_data).is_ok());
    }

    #[test]
    fn a_signature_reservation_that_cannot_exist_is_an_error_not_an_allocation() {
        let old = vec![0u8; 16];
        let permissive = ResourceProfile {
            max_signature_data: usize::MAX,
            max_signature_message: usize::MAX,
            max_decoded_bytes: usize::MAX,
            ..EMBEDDED
        };
        for key_size in [usize::MAX, usize::MAX - 1, usize::MAX - 16] {
            assert_eq!(
                Problem::new(&old, &old, 4, "A/B", &permissive, key_size).err().map(|e| e.code),
                Some(Code::overflow),
                "key {}",
                key_size
            );
        }
        for key_size in [isize::MAX as usize, isize::MAX as usize - 16] {
            assert_eq!(
                Problem::new(&old, &old, 4, "A/B", &permissive, key_size).err().map(|e| e.code),
                Some(Code::resource_limit),
                "key {}",
                key_size
            );
        }
    }

    fn noise_bytes(len: usize, seed: u64) -> Vec<u8> {
        let mut x = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
        let mut out = Vec::with_capacity(len);
        for _ in 0..len {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            out.push((x >> 24) as u8);
        }
        return out;
    }
}
