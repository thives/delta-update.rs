//! Sizing and building one-entry `Signatures` messages on top of the
//! `prost-build` generated protobuf types (see `build.rs` and `lib.rs`).
//!
//! Mirrors `ConvertSignaturesToProtobuf` in the golden reference: the
//! deprecated `version` field is omitted, signature data is right-padded with
//! zeros to the reserved key size, and the original length is recorded as
//! `unpadded_signature_size` (a `fixed32`). The encoded size therefore
//! depends only on the reserved key size and is known *before* signing, which
//! is what lets the payload header and manifest reserve space for the
//! signature messages ahead of time.

use alloc::vec::Vec;

use crate::error::{err, Code, PayloadError};
use crate::schema::signatures::Signature;
use crate::schema::Signatures;
use prost::Message;

fn varint_size(v: usize) -> usize {
    let mut n = 1;
    let mut x = v >> 7;
    while x != 0 {
        n += 1;
        x >>= 7;
    }
    n
}

/// Encoded size of a `Signature` entry with `key_size` padded data bytes.
/// The deprecated `version` field is never written; `data` and
/// `unpadded_signature_size` are always present.
/// `key_size` is in bytes; the entry must fit `usize`.
pub fn sig_entry_size(key_size: usize) -> usize {
    (1 + varint_size(key_size) + key_size) + (1 + 4)
}

/// Encoded size of a `Signatures` message containing one padded entry.
/// The encoded size must fit `usize`; it does not depend on the unpadded
/// length, which is the whole point of the padding scheme.
pub fn signature_message_size(key_size: usize) -> usize {
    let entry = sig_entry_size(key_size);
    1 + varint_size(entry) + entry
}

/// [`signature_message_size`] with every addition checked. A key size whose entry or message does
/// not fit `usize` is an `overflow` error, so no reservation is sized by a wrapped number. The
/// signer and the profile's preflight both size their reservations with this, before anything is
/// allocated.
pub fn checked_message_size(key_size: usize) -> Result<usize, PayloadError> {
    let entry = key_size
        .checked_add(1 + varint_size(key_size) + 5)
        .ok_or_else(|| err(Code::overflow, "signature entry size overflow"))?;
    entry
        .checked_add(1 + varint_size(entry))
        .ok_or_else(|| err(Code::overflow, "signature message size overflow"))
}

/// Encode a one-entry `Signatures` message for an already-computed
/// signature, zero-padding the data to the reserved `key_size` bytes.
///
/// `key_size` is the reserved data length in bytes, not a cryptographic key
/// validation policy. Empty/short signatures are permitted, as in the
/// reference. Invalid lengths are rejected before any allocation, and a
/// reservation the allocator cannot make is an error rather than an abort.
#[allow(deprecated)]
pub fn build_signature_message(key_size: usize, sig_data: &[u8]) -> Result<Vec<u8>, PayloadError> {
    if sig_data.len() > key_size {
        return Err(err(
            Code::signature_mismatch,
            "signature exceeds padded size",
        ));
    }
    let unpadded = u32::try_from(sig_data.len())
        .map_err(|_| err(Code::overflow, "unpadded signature size exceeds fixed32"))?;
    checked_message_size(key_size)?;

    let mut padded = Vec::new();
    padded
        .try_reserve_exact(key_size)
        .map_err(|_| err(Code::resource_limit, "signature reservation cannot be allocated"))?;
    padded.resize(key_size, 0);
    let mut i = 0;
    while i < sig_data.len() {
        padded[i] = sig_data[i];
        i += 1;
    }

    let entry_val = Signature {
        version: None,
        data: Some(padded),
        unpadded_signature_size: Some(unpadded),
    };
    let mut entries = Vec::new();
    entries.push(entry_val);
    let message = Signatures {
        signatures: entries,
    };
    Ok(message.encode_to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::manifest::decode_signatures;
    use crate::profile::EMBEDDED;
    use crate::profile::HOST;
    use crate::rsa::{parse_pubkey_pem, rsa_verify};
    use crate::sha256;

    fn success<T>(result: Result<T, PayloadError>) -> T {
        result.unwrap_or_else(|error| panic!("{}", error.msg))
    }

    #[test]
    fn matches_reference_wire_format() {
        let encoded = success(build_signature_message(3, &[0xaa, 0xbb]));
        assert_eq!(
            encoded,
            [0x0a, 0x0a, 0x12, 0x03, 0xaa, 0xbb, 0x00, 0x1d, 0x02, 0x00, 0x00, 0x00,]
        );
        let empty = success(build_signature_message(0, &[]));
        assert_eq!(
            empty,
            [0x0a, 0x07, 0x12, 0x00, 0x1d, 0x00, 0x00, 0x00, 0x00]
        );
    }

    #[test]
    fn sizes_match_known_rsa_reservations() {
        for (key_size, entry_size, message_size) in [(256, 264, 267), (512, 520, 523)] {
            assert_eq!(sig_entry_size(key_size), entry_size);
            assert_eq!(signature_message_size(key_size), message_size);
        }
    }

    #[test]
    fn checked_sizes_agree_with_the_wire_sizes_and_refuse_wrapping_ones() {
        for key_size in [0, 1, 127, 128, 16383, 16384, 1 << 20] {
            assert_eq!(checked_message_size(key_size).ok(), Some(signature_message_size(key_size)));
        }
        for key_size in [usize::MAX, usize::MAX - 1, usize::MAX - 16] {
            assert_eq!(checked_message_size(key_size).err().map(|e| e.code), Some(Code::overflow), "key {}", key_size);
        }
    }

    #[test]
    fn size_helpers_agree_with_generated_encoder() {
        for key_size in [
            0, 1, 3, 120, 121, 127, 128, 255, 256, 512, 16375, 16376, 16383, 16384,
        ] {
            for unpadded in [0, key_size / 2, key_size] {
                let mut signature = Vec::new();
                let mut k = 0;
                while k < unpadded {
                    signature.push(0xab);
                    k += 1;
                }
                let encoded = success(build_signature_message(key_size, signature.as_slice()));
                assert_eq!(encoded.len(), signature_message_size(key_size));
                let decoded = success(decode_signatures(encoded.as_slice(), &HOST));
                assert_eq!(decoded.signatures.len(), 1);
                let entry = &decoded.signatures[0];
                let data = entry.data.as_ref().unwrap();
                assert_eq!(entry.unpadded_signature_size.unwrap() as usize, unpadded);
                assert_eq!(data.len(), key_size);
                let mut i = 0;
                while i < unpadded {
                    assert_eq!(data[i], 0xab);
                    i += 1;
                }
                while i < key_size {
                    assert_eq!(data[i], 0);
                    i += 1;
                }
            }
        }
    }

    #[test]
    fn rejects_invalid_lengths_before_allocating() {
        for (key_size, signature, code) in [
            (0, &[0x78][..], Code::signature_mismatch),
            (2, &[0x61, 0x62, 0x63][..], Code::signature_mismatch),
            (usize::MAX, &[0x0][..0], Code::overflow),
            (usize::MAX - 16, &[0x0][..0], Code::overflow),
        ] {
            match build_signature_message(key_size, signature) {
                Ok(_) => panic!("expected an error"),
                Err(error) => assert!(error.code == code, "{}", error.msg),
            }
        }
    }

    #[test]
    fn encoded_openssl_signature_verifies_after_unpadding() {
        let signature = include_bytes!("../tests/fixtures/sig.bin");
        let key = success(parse_pubkey_pem(include_bytes!(
            "../tests/fixtures/public.pem"
        )));
        let hash = sha256::hash_bytes(include_bytes!("../tests/fixtures/msg.bin"));
        for key_size in [256, 512] {
            let encoded = success(build_signature_message(key_size, signature));
            let decoded = success(decode_signatures(encoded.as_slice(), &EMBEDDED));
            let entry = &decoded.signatures[0];
            let data = entry.data.as_ref().unwrap();
            let size = entry.unpadded_signature_size.unwrap() as usize;
            success(rsa_verify(&key, &data[..size], &hash));
        }
    }
}
