//! Verify the embedded metadata and payload signatures of a complete payload.
//!
//! Authentication order is fixed: frame the payload (no decoding), verify the
//! metadata signature over the raw header and manifest bytes, then decode the
//! manifest, then verify the payload signature. The manifest is untrusted
//! until its own signature has verified, so nothing it says about layout is
//! used before that point.
//!
//! Hash ranges match `CalculateHashFromPayload` in the golden reference:
//! neither signature message is part of the payload's signed hash.

use crate::error::{Code, PayloadError, err};
use crate::layout::Authenticated;
use crate::layout::Parsed;
use crate::layout::frame;
use crate::layout::locate;
use crate::manifest::decode_manifest;
use crate::manifest::decode_signatures;
use crate::profile::ResourceProfile;
use crate::rsa::{PubKey, rsa_verify};
use crate::sha256;

/// Parse a complete payload and require both embedded signatures to verify.
///
/// Each signature message must match at least one trusted key; the metadata
/// and payload signatures may match different keys during key rotation.
/// Unsigned payloads and bytes after the declared payload signature are
/// rejected. The returned value is decoded but not yet validated against the
/// device; pass it to [`plan::validate`](crate::plan::validate) before use.
pub fn verify_payload(
    payload: &[u8],
    keys: &[PubKey],
    profile: &ResourceProfile,
) -> Result<Authenticated, PayloadError> {
    let f = match frame(payload, profile) {
        Err(e) => return Err(e),
        Ok(v) => v,
    };
    if f.header.meta_sig_size == 0 {
        return Err(err(
            Code::signature_missing,
            "both embedded signatures are required",
        ));
    }

    let metadata_hash = sha256::hash_bytes(f.metadata);
    match verify_message(f.meta_sig, &metadata_hash, keys, profile) {
        Err(e) => return Err(e),
        Ok(()) => {}
    };

    let manifest = match decode_manifest(f.manifest, profile) {
        Err(e) => return Err(e),
        Ok(v) => v,
    };
    let declares_signature = match (manifest.signatures_offset, manifest.signatures_size) {
        (Some(_), Some(size)) => size != 0,
        _ => false,
    };
    if !declares_signature {
        return Err(err(
            Code::signature_missing,
            "both embedded signatures are required",
        ));
    }
    let info = match locate(&f, &manifest) {
        Err(e) => return Err(e),
        Ok(v) => v,
    };

    let blobs_end = info.sig_offset_blobs as usize;
    let blobs = &f.rest[..blobs_end];
    let signature = &f.rest[blobs_end..];
    let mut hash = sha256::new_hash();
    sha256::update(&mut hash, f.metadata);
    sha256::update(&mut hash, blobs);
    let payload_hash = sha256::finish(hash);
    match verify_message(signature, &payload_hash, keys, profile) {
        Err(e) => return Err(e),
        Ok(()) => {}
    };

    return Ok(Authenticated::new(Parsed {
        info: info,
        manifest: manifest,
    }));
}

/// Verify one `Signatures` message against the trusted keys: decode the
/// whole message first, then follow the reference's ordered search (empty
/// list rejected, `unpadded_signature_size` prefix honoured, any entry matching
/// any key succeeds). Public because the streaming applier in `ota-client`
/// verifies each signature message as it is read, rather than over a
/// complete payload buffer.
pub fn verify_message(
    data: &[u8],
    hash: &[u8],
    keys: &[PubKey],
    profile: &ResourceProfile,
) -> Result<(), PayloadError> {
    let signatures = match decode_signatures(data, profile) {
        Err(e) => return Err(e),
        Ok(v) => v,
    };
    if signatures.signatures.len() == 0 {
        return Err(err(Code::signature_missing, "empty signature message"));
    }
    for signature in &signatures.signatures {
        match &signature.data {
            None => {
                continue;
            }
            Some(data_bytes) => {
                let size = match signature.unpadded_signature_size {
                    None => data_bytes.len(),
                    Some(n) => n as usize,
                };
                if size > data_bytes.len() {
                    return Err(err(
                        Code::signature_mismatch,
                        "unpadded signature size exceeds data",
                    ));
                }
                for key in keys {
                    if rsa_verify(key, &data_bytes[..size], hash).is_ok() {
                        return Ok(());
                    }
                }
            }
        }
    }
    Err(err(
        Code::signature_mismatch,
        "no signature matches a trusted key",
    ))
}

#[cfg(test)]
mod tests {
    extern crate std;

    use super::*;
    use crate::layout;
    use crate::manifest::Manifest;
    use crate::manifest::PartitionUpdate;
    use crate::manifest::Signature;
    use crate::manifest::Signatures;
    use crate::profile::EMBEDDED;
    use crate::rsa::{PrivKey, parse_privkey_pem, parse_pubkey_pem, rsa_sign};
    use crate::sign::build_signature_message;
    use crate::sign::signature_message_size;
    use ::rsa::RsaPrivateKey;
    use ::rsa::pkcs1::DecodeRsaPrivateKey;
    use ::rsa::pkcs8::{DecodePrivateKey, EncodePublicKey, LineEnding};
    use alloc::string::ToString;
    use alloc::vec::Vec;
    use prost::Message;

    const PRIVATE: &[u8] = include_bytes!("../tests/fixtures/private.pem");
    const PRIVATE_4096: &[u8] = include_bytes!("../tests/fixtures/private4096.pem");

    #[allow(deprecated)]
    fn entry(data: &[u8], unpadded: Option<u32>) -> Signature {
        Signature {
            version: Some(42),
            data: Some(data.to_vec()),
            unpadded_signature_size: unpadded,
        }
    }

    fn message(entries: &[Signature]) -> Vec<u8> {
        let mut msg = Signatures {
            signatures: Vec::new(),
        };
        let mut i = 0;
        while i < entries.len() {
            msg.signatures.push(entries[i].clone());
            i += 1;
        }
        msg.encode_to_vec()
    }

    fn manifest_with(offset: Option<u64>, size: Option<u64>) -> Manifest {
        let mut parts = Vec::new();
        parts.push(one_partition());
        Manifest {
            block_size: Some(4096),
            signatures_offset: offset,
            signatures_size: size,
            minor_version: Some(0),
            partitions: parts,
            max_timestamp: None,
            dynamic_partition_metadata: None,
            partial_update: None,
            apex_info: Vec::new(),
            security_patch_level: None,
            release_version: None,
        }
    }

    #[allow(deprecated)]
    fn one_partition() -> PartitionUpdate {
        PartitionUpdate {
            partition_name: "A/B".to_string(),
            run_postinstall: None,
            postinstall_path: None,
            filesystem_type: None,
            new_partition_signature: Vec::new(),
            old_partition_info: None,
            new_partition_info: None,
            operations: Vec::new(),
            postinstall_optional: None,
            hash_tree_data_extent: None,
            hash_tree_extent: None,
            hash_tree_algorithm: None,
            hash_tree_salt: None,
            fec_data_extent: None,
            fec_extent: None,
            fec_roots: None,
            version: None,
            merge_operations: Vec::new(),
            estimate_cow_size: None,
            estimate_op_count_max: None,
        }
    }

    fn manifest(offset: u64, size: u64) -> Vec<u8> {
        manifest_with(Some(offset), Some(size)).encode_to_vec()
    }

    fn success<T>(result: Result<T, PayloadError>) -> T {
        result.unwrap_or_else(|error| panic!("{}", error.msg))
    }

    fn has_code<T>(result: Result<T, PayloadError>, code: Code) {
        match result {
            Ok(_) => panic!("expected an error"),
            Err(error) => assert!(error.code == code, "{}", error.msg),
        }
    }

    fn public(pem: &[u8]) -> PubKey {
        let pem = core::str::from_utf8(pem).unwrap();
        let private = RsaPrivateKey::from_pkcs8_pem(pem)
            .or_else(|_| RsaPrivateKey::from_pkcs1_pem(pem))
            .unwrap();
        let pem = private
            .to_public_key()
            .to_public_key_pem(LineEnding::LF)
            .unwrap();
        success(parse_pubkey_pem(pem.as_bytes()))
    }

    fn signed_message(key: &PrivKey, hash: &[u8]) -> Vec<u8> {
        let signature = success(rsa_sign(key, hash));
        message(&[entry(
            &signature.bytes[..signature.len],
            Some(signature.len as u32),
        )])
    }

    fn framed_payload(
        manifest: &[u8],
        metadata_signature: &[u8],
        blobs: &[u8],
        signature: &[u8],
    ) -> Vec<u8> {
        let mut payload = Vec::new();
        payload.extend_from_slice(&layout::MAGIC);
        payload.extend_from_slice(&(layout::MAJOR_VERSION as u64).to_be_bytes());
        payload.extend_from_slice(&(manifest.len() as u64).to_be_bytes());
        payload.extend_from_slice(&(metadata_signature.len() as u32).to_be_bytes());
        payload.extend_from_slice(manifest);
        payload.extend_from_slice(metadata_signature);
        payload.extend_from_slice(blobs);
        payload.extend_from_slice(signature);
        payload
    }

    fn payload_signed_for(key: &PrivKey, manifest: &[u8], blobs: &[u8], signature: &[u8]) -> Vec<u8> {
        let msg_len = signature_message_size(key.size);
        let mut head = Vec::new();
        head.extend_from_slice(&layout::MAGIC);
        head.extend_from_slice(&(layout::MAJOR_VERSION as u64).to_be_bytes());
        head.extend_from_slice(&(manifest.len() as u64).to_be_bytes());
        head.extend_from_slice(&(msg_len as u32).to_be_bytes());
        head.extend_from_slice(manifest);
        let sig = success(rsa_sign(key, &sha256::hash_bytes(&head)));
        let metadata_signature = success(build_signature_message(key.size, &sig.bytes[..sig.len]));
        return framed_payload(manifest, &metadata_signature, blobs, signature);
    }

    fn signed_payload(metadata_key: &[u8], payload_key: &[u8], blobs: &[u8]) -> Vec<u8> {
        let metadata_key = success(parse_privkey_pem(metadata_key));
        let payload_key = success(parse_privkey_pem(payload_key));
        let placeholder = [0; 512];
        let metadata_signature = message(&[entry(
            &placeholder[..metadata_key.size],
            Some(metadata_key.size as u32),
        )]);
        let signature = message(&[entry(
            &placeholder[..payload_key.size],
            Some(payload_key.size as u32),
        )]);
        let manifest = manifest(blobs.len() as u64, signature.len() as u64);
        let mut payload = framed_payload(&manifest, &metadata_signature, blobs, &signature);
        let metadata_end = 24 + manifest.len();
        let blobs_start = metadata_end + metadata_signature.len();
        let signature_start = blobs_start + blobs.len();
        let metadata_signature =
            signed_message(&metadata_key, &sha256::hash_bytes(&payload[..metadata_end]));
        let mut signed_bytes = payload[..metadata_end].to_vec();
        signed_bytes.extend_from_slice(blobs);
        let signature = signed_message(&payload_key, &sha256::hash_bytes(&signed_bytes));
        payload[metadata_end..blobs_start].copy_from_slice(&metadata_signature);
        payload[signature_start..].copy_from_slice(&signature);
        payload
    }

    fn verify(payload: &[u8], keys: &[PubKey]) -> Result<Authenticated, PayloadError> {
        return verify_payload(payload, keys, &EMBEDDED);
    }

    #[test]
    fn decodes_generated_manifest_and_info() {
        for pem in [PRIVATE, PRIVATE_4096] {
            let payload = signed_payload(pem, pem, b"blob data");
            let parsed = success(verify(&payload, &[public(pem)]));
            let parsed = parsed.parsed();
            assert_eq!(parsed.info.sig_offset_blobs, 9);
            assert_eq!(parsed.info.payload_size, payload.len() as u64);
            assert!(parsed.info.has_sig);
            assert_eq!(parsed.manifest.partitions[0].partition_name, "A/B");
        }
    }

    #[test]
    fn supports_key_rotation_and_an_empty_blob_region() {
        let payload = signed_payload(PRIVATE, PRIVATE_4096, b"");
        let keys = [public(PRIVATE_4096), public(PRIVATE)];
        let parsed = success(verify(&payload, &keys));
        assert_eq!(parsed.parsed().info.blobs_start, parsed.parsed().info.sig_msg_offset);
        has_code(verify(&payload, &keys[..1]), Code::signature_mismatch);
        has_code(verify(&payload, &keys[1..]), Code::signature_mismatch);
        has_code(verify(&payload, &[]), Code::signature_mismatch);
    }

    #[test]
    fn rejects_tampering_in_each_authenticated_region() {
        let payload = signed_payload(PRIVATE, PRIVATE, b"blob data");
        let parsed = success(layout::parse_unverified(&payload, &EMBEDDED));
        let info = &parsed.parsed().info;
        let keys = [public(PRIVATE)];
        for index in [
            25,
            info.metadata_size as usize + 20,
            info.blobs_start as usize,
            info.sig_msg_offset as usize + 20,
        ] {
            let mut tampered = payload.clone();
            tampered[index] ^= 1;
            has_code(verify(&tampered, &keys), Code::signature_mismatch);
        }
    }

    #[test]
    fn rejects_missing_signatures_incomplete_fields_and_trailing_bytes() {
        let key = success(parse_privkey_pem(PRIVATE));
        let payload = signed_payload(PRIVATE, PRIVATE, b"blob data");
        let parsed = success(layout::parse_unverified(&payload, &EMBEDDED));
        let info = &parsed.parsed().info;
        let signature_start = info.sig_msg_offset as usize;
        let keys = [public(PRIVATE)];
        let signature = &payload[signature_start..];
        let manifest_variants = [
            Vec::new(),
            manifest_with(Some(0), None).encode_to_vec(),
            manifest_with(None, Some(signature.len() as u64)).encode_to_vec(),
            manifest(0, 0),
        ];
        for manifest in manifest_variants {
            let unsigned = payload_signed_for(&key, &manifest, b"", signature);
            has_code(verify(&unsigned, &keys), Code::signature_missing);
        }
        let no_metadata_signature =
            framed_payload(&manifest(0, signature.len() as u64), b"", b"", signature);
        has_code(
            verify(&no_metadata_signature, &keys),
            Code::signature_missing,
        );
        let mut trailing = payload.clone();
        trailing.push(0);
        has_code(verify(&trailing, &keys), Code::parse);
        has_code(verify(&payload[..payload.len() - 1], &keys), Code::overflow);
        has_code(verify(b"", &keys), Code::parse);
        let mut bad_magic = payload;
        bad_magic[0] ^= 1;
        has_code(verify(&bad_magic, &keys), Code::bad_magic);
    }

    #[test]
    fn rejects_signature_offset_and_size_overflow_without_panicking() {
        let key = success(parse_privkey_pem(PRIVATE));
        let keys = [public(PRIVATE)];
        for (offset, size) in [(u64::MAX, 1), (0, u64::MAX), (u64::MAX - 25, 100)] {
            let payload = payload_signed_for(&key, &manifest(offset, size), b"", b"");
            has_code(verify(&payload, &keys), Code::overflow);
        }
    }

    #[test]
    fn accepts_multiple_signatures_optional_size_and_nonzero_padding() {
        let hash = sha256::hash_bytes(b"signed bytes");
        let private = success(parse_privkey_pem(PRIVATE));
        let keys = [public(PRIVATE_4096), public(PRIVATE)];
        let signature = success(rsa_sign(&private, &hash));
        let data = &signature.bytes[..signature.len];
        let without_size = entry(data, None);
        success(verify_message(
            &message(&[without_size.clone()]),
            &hash,
            &keys,
            &EMBEDDED,
        ));
        let mut padded = data.to_vec();
        padded.extend_from_slice(&[0xab; 256]);
        let padded_entry = entry(&padded, Some(data.len() as u32));
        let missing_data = entry(b"", None);
        let invalid = entry(&[0; 256], None);
        success(verify_message(
            &message(&[missing_data, invalid, padded_entry]),
            &hash,
            &keys,
            &EMBEDDED,
        ));
        has_code(
            verify_message(&message(&[entry(&padded, None)]), &hash, &keys, &EMBEDDED),
            Code::signature_mismatch,
        );
        has_code(
            verify_message(&message(&[entry(data, Some(0))]), &hash, &keys, &EMBEDDED),
            Code::signature_mismatch,
        );
        let too_long = entry(data, Some(data.len() as u32 + 1));
        has_code(
            verify_message(
                &message(&[too_long.clone(), without_size.clone()]),
                &hash,
                &keys,
                &EMBEDDED,
            ),
            Code::signature_mismatch,
        );
        success(verify_message(
            &message(&[without_size.clone(), too_long.clone()]),
            &hash,
            &keys,
            &EMBEDDED,
        ));
    }

    #[test]
    fn rejects_empty_and_malformed_signature_messages() {
        let hash = sha256::hash_bytes(b"signed bytes");
        let private = success(parse_privkey_pem(PRIVATE));
        let keys = [public(PRIVATE)];
        let valid = signed_message(&private, &hash);
        has_code(
            verify_message(b"", &hash, &keys, &EMBEDDED),
            Code::signature_missing,
        );
        has_code(
            verify_message(&message(&[entry(b"", None)]), &hash, &keys, &EMBEDDED),
            Code::signature_mismatch,
        );
        let suffixes = [
            &[0x80][..],
            &[0x00, 0x00][..],
            &[0x10, 0x80][..],
            &[0x15, 0x00][..],
            &[0x11, 0x00][..],
            &[0x12, 0x02, 0x00][..],
            &[0x17][..],
            &[0x0a, 0x01, 0x12][..],
            &[0x0a, 0x02, 0x1d, 0x00][..],
            &[0x0a, 0x02, 0x20, 0x80][..],
            &[
                0x0a, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x01,
            ][..],
            &[0x80, 0x80, 0x80, 0x80, 0x10, 0x00][..],
        ];
        for suffix in suffixes {
            let mut malformed = valid.to_vec();
            malformed.extend_from_slice(suffix);
            has_code(
                verify_message(&malformed, &hash, &keys, &EMBEDDED),
                Code::bad_protobuf,
            );
        }
        let mut entries = Vec::new();
        let mut k = 0;
        while k < EMBEDDED.max_signatures + 1 {
            entries.push(entry(b"", None));
            k += 1;
        }
        let too_many = message(entries.as_slice());
        has_code(
            verify_message(&too_many, &hash, &keys, &EMBEDDED),
            Code::resource_limit,
        );
    }

    #[test]
    fn duplicate_signature_data_is_refused_before_any_entry_is_checked() {
        let hash = sha256::hash_bytes(b"signed bytes");
        let private = success(parse_privkey_pem(PRIVATE));
        let keys = [public(PRIVATE)];
        let signature = success(rsa_sign(&private, &hash));
        let data = &signature.bytes[..signature.len];
        let mut body = Vec::new();
        body.push(0x12);
        body.extend_from_slice(&varint_bytes(data.len() as u64));
        body.extend_from_slice(data);
        body.push(0x12);
        body.extend_from_slice(&varint_bytes(257));
        body.extend_from_slice(&[0xab; 257]);
        let mut msg = Vec::new();
        msg.push(0x0a);
        msg.extend_from_slice(&varint_bytes(body.len() as u64));
        msg.extend_from_slice(&body);
        has_code(
            verify_message(&msg, &hash, &keys, &EMBEDDED),
            Code::bad_protobuf,
        );
    }

    fn pad_to(msg: &mut Vec<u8>, total: usize) {
        let room = total - msg.len();
        let mut len = room;
        while len > 0 && 1 + varint_bytes(len as u64).len() + len != room {
            len -= 1;
        }
        msg.push((15 << 3) | 2);
        msg.extend_from_slice(&varint_bytes(len as u64));
        msg.resize(msg.len() + len, 0);
        assert_eq!(msg.len(), total);
    }

    #[test]
    fn signature_messages_are_bounded_at_the_shared_boundary() {
        let hash = sha256::hash_bytes(b"signed bytes");
        let private = success(parse_privkey_pem(PRIVATE));
        let keys = [public(PRIVATE)];
        let signature = success(rsa_sign(&private, &hash));
        let limit = EMBEDDED.max_signature_message;

        let mut at_limit = message(&[entry(&signature.bytes[..signature.len], None)]);
        pad_to(&mut at_limit, limit);
        success(verify_message(&at_limit, &hash, &keys, &EMBEDDED));
        success(decode_signatures(&at_limit, &EMBEDDED));

        let mut over = at_limit.clone();
        over.push(0);
        has_code(
            verify_message(&over, &hash, &keys, &EMBEDDED),
            Code::resource_limit,
        );
        has_code(decode_signatures(&over, &EMBEDDED), Code::resource_limit);

        let mut malformed = at_limit.clone();
        malformed.resize(limit + 1, 0x80);
        has_code(
            verify_message(&malformed, &hash, &keys, &EMBEDDED),
            Code::resource_limit,
        );
    }

    #[test]
    fn oversized_payload_signature_declaration_is_refused_after_authentication() {
        let key = success(parse_privkey_pem(PRIVATE));
        let keys = [public(PRIVATE)];
        let oversize = EMBEDDED.max_signature_message + 1;
        let manifest = manifest(0, oversize as u64);
        let mut oversized = Vec::new();
        oversized.resize(oversize, 0);
        let payload = payload_signed_for(&key, &manifest, b"", &oversized);
        has_code(verify(&payload, &keys), Code::resource_limit);
    }

    #[test]
    fn skips_well_formed_unknown_fields() {
        let hash = sha256::hash_bytes(b"signed bytes");
        let private = success(parse_privkey_pem(PRIVATE));
        let keys = [public(PRIVATE)];
        let signature = success(rsa_sign(&private, &hash));
        let mut entry_bytes = entry(&signature.bytes[..signature.len], None).encode_to_vec();
        let unknown = &[
            0x20, 0x01, 0x29, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x32, 0x01, 0x78,
            0x3d, 0x00, 0x00, 0x00, 0x00,
        ];
        entry_bytes.extend_from_slice(unknown);
        let mut msg = Vec::new();
        msg.push(0x0a);
        msg.extend_from_slice(&varint_bytes(entry_bytes.len() as u64));
        msg.extend_from_slice(&entry_bytes);
        msg.extend_from_slice(unknown);
        success(verify_message(&msg, &hash, &keys, &EMBEDDED));
    }

    fn varint_bytes(v: u64) -> Vec<u8> {
        let mut out = Vec::new();
        let mut x = v;
        loop {
            let b = (x & 0x7f) as u8;
            x >>= 7;
            if x != 0 {
                out.push(b | 0x80);
            } else {
                out.push(b);
                break;
            }
        }
        out
    }
}
