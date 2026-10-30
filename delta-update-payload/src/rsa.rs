//! RSA key decoding and prehashed PKCS#1 v1.5 SHA-256 signatures.
//!
//! Key parsing, padding, and arithmetic are delegated to `rsa`. Unlike the
//! payload's borrowed data structures, RSA operations require an allocator.

use crate::error::{err, Code, PayloadError};
use ::rsa::pkcs1::{DecodeRsaPrivateKey, DecodeRsaPublicKey};
use ::rsa::pkcs8::{DecodePrivateKey, DecodePublicKey};
use ::rsa::sha2::Sha256;
use ::rsa::traits::PublicKeyParts;
use ::rsa::{Pkcs1v15Sign, RsaPrivateKey, RsaPublicKey};

/// Maximum supported signature size in bytes (RSA-4096).
pub const KEY_SIZE_BIG: usize = 512;

/// A validated RSA public key and its modulus size in bytes.
pub struct PubKey {
    key: RsaPublicKey,
    pub size: usize,
}

/// A validated RSA private key and its modulus size in bytes.
pub struct PrivKey {
    key: RsaPrivateKey,
    pub size: usize,
}

fn key_size(key: &impl PublicKeyParts) -> Result<usize, PayloadError> {
    match key.n().bits() {
        2048 | 4096 => Ok(key.size()),
        bits if bits < 2048 => Err(err(
            Code::key_too_small,
            "RSA key must be at least 2048 bits",
        )),
        _ => Err(err(
            Code::key_unsupported,
            "only 2048-bit and 4096-bit RSA keys are supported",
        )),
    }
}

/// Decode a SubjectPublicKeyInfo (`PUBLIC KEY`) or PKCS#1 (`RSA PUBLIC KEY`) PEM.
pub fn parse_pubkey_pem(text: &[u8]) -> Result<PubKey, PayloadError> {
    let text =
        core::str::from_utf8(text).map_err(|_| err(Code::parse, "public key PEM is not UTF-8"))?;
    let key = RsaPublicKey::from_public_key_pem(text)
        .or_else(|_| RsaPublicKey::from_pkcs1_pem(text))
        .map_err(|_| {
            err(
                Code::key_unsupported,
                "invalid or unsupported RSA public key PEM",
            )
        })?;
    let size = key_size(&key)?;
    Ok(PubKey { key, size })
}

/// Decode an unencrypted PKCS#8 (`PRIVATE KEY`) or PKCS#1 (`RSA PRIVATE KEY`) PEM.
pub fn parse_privkey_pem(text: &[u8]) -> Result<PrivKey, PayloadError> {
    let text =
        core::str::from_utf8(text).map_err(|_| err(Code::parse, "private key PEM is not UTF-8"))?;
    let key = RsaPrivateKey::from_pkcs8_pem(text)
        .or_else(|_| RsaPrivateKey::from_pkcs1_pem(text))
        .map_err(|_| {
            err(
                Code::key_unsupported,
                "invalid or unsupported RSA private key PEM",
            )
        })?;
    let size = key_size(&key)?;
    Ok(PrivKey { key, size })
}

/// A modulus-sized signature in `bytes[..len]`; any unused bytes are zero.
pub struct Signature {
    pub bytes: [u8; KEY_SIZE_BIG],
    pub len: usize,
}

/// Sign an already-computed, 32-byte SHA-256 digest, without hashing it again.
///
/// This RNG-free API uses the dependency's unblinded private-key operation.
/// Do not expose it as a signing service to callers able to observe timing.
/// The public half of a private key. Used to verify generated payloads with
/// the same verifier the client runs.
pub fn public_key_of(key: &PrivKey) -> PubKey {
    return PubKey {
        key: key.key.to_public_key(),
        size: key.size,
    };
}

pub fn rsa_sign(key: &PrivKey, hash: &[u8]) -> Result<Signature, PayloadError> {
    let bytes = key
        .key
        .sign(Pkcs1v15Sign::new::<Sha256>(), hash)
        .map_err(|_| err(Code::signature_mismatch, "RSA SHA-256 signing failed"))?;
    let mut signature = Signature {
        bytes: [0; KEY_SIZE_BIG],
        len: bytes.len(),
    };
    signature.bytes[..bytes.len()].copy_from_slice(&bytes);
    Ok(signature)
}

/// Verify a modulus-sized PKCS#1 v1.5 signature over a SHA-256 digest.
pub fn rsa_verify(key: &PubKey, sig_data: &[u8], hash: &[u8]) -> Result<(), PayloadError> {
    key.key
        .verify(Pkcs1v15Sign::new::<Sha256>(), hash, sig_data)
        .map_err(|_| err(Code::signature_mismatch, "RSA SHA-256 signature mismatch"))
}

#[cfg(test)]
mod tests {
    extern crate std;

    use super::*;
    use ::rsa::pkcs1::{EncodeRsaPrivateKey, EncodeRsaPublicKey};
    use ::rsa::pkcs8::{EncodePrivateKey, EncodePublicKey, LineEnding};
    use ::rsa::sha2::Digest;
    use ::rsa::BigUint;

    const PRIVATE: &[u8] = include_bytes!("../tests/fixtures/private.pem");
    const PUBLIC: &[u8] = include_bytes!("../tests/fixtures/public.pem");
    const PRIVATE_4096: &[u8] = include_bytes!("../tests/fixtures/private4096.pem");
    const MESSAGE: &[u8] = include_bytes!("../tests/fixtures/msg.bin");
    const OPENSSL_SIGNATURE: &[u8] = include_bytes!("../tests/fixtures/sig.bin");
    const OPENSSL_SIGNATURE_4096: &[u8] = include_bytes!("../tests/fixtures/sig4096.bin");

    fn success<T>(result: Result<T, PayloadError>) -> T {
        result.unwrap_or_else(|error| panic!("{}", error.msg))
    }

    fn has_code<T>(result: Result<T, PayloadError>, code: Code) {
        match result {
            Ok(_) => panic!("expected an error"),
            Err(error) => assert!(error.code == code, "{}", error.msg),
        }
    }

    #[test]
    fn parses_both_public_key_formats() {
        let expected = success(parse_pubkey_pem(PUBLIC));
        for pem in [
            expected.key.to_public_key_pem(LineEnding::LF).unwrap(),
            expected.key.to_pkcs1_pem(LineEnding::CRLF).unwrap(),
        ] {
            let parsed = success(parse_pubkey_pem(pem.as_bytes()));
            assert_eq!(parsed.key, expected.key);
            assert_eq!(parsed.size, 256);
        }
    }

    #[test]
    fn parses_both_private_key_formats() {
        let expected = success(parse_privkey_pem(PRIVATE));
        for pem in [
            expected.key.to_pkcs8_pem(LineEnding::LF).unwrap(),
            expected.key.to_pkcs1_pem(LineEnding::CRLF).unwrap(),
        ] {
            let parsed = success(parse_privkey_pem(pem.as_bytes()));
            assert_eq!(parsed.key, expected.key);
            assert_eq!(parsed.size, 256);
        }
    }

    #[test]
    fn matches_openssl_sha256_signatures_for_both_key_sizes() {
        let hash = Sha256::digest(MESSAGE);
        for (pem, expected, size) in [
            (PRIVATE, OPENSSL_SIGNATURE, 256),
            (PRIVATE_4096, OPENSSL_SIGNATURE_4096, 512),
        ] {
            let private = success(parse_privkey_pem(pem));
            let public_pem = private
                .key
                .to_public_key()
                .to_public_key_pem(LineEnding::LF)
                .unwrap();
            let public = success(parse_pubkey_pem(public_pem.as_bytes()));
            assert_eq!(private.size, size);
            assert_eq!(public.size, size);
            let signature = success(rsa_sign(&private, &hash));
            assert_eq!(signature.len, size);
            assert_eq!(&signature.bytes[..signature.len], expected);
            assert!(signature.bytes[signature.len..].iter().all(|&b| b == 0));
            success(rsa_verify(&public, expected, &hash));
        }
    }

    #[test]
    fn rejects_malformed_and_wrong_kind_pem() {
        for input in [
            &b""[..],
            &b"not a key"[..],
            &b"-----BEGIN PUBLIC KEY-----\nAAAA\n-----END PUBLIC KEY-----"[..],
            &b"-----BEGIN ENCRYPTED PRIVATE KEY-----\nAAAA\n-----END ENCRYPTED PRIVATE KEY-----"[..],
        ] {
            has_code(parse_pubkey_pem(input), Code::key_unsupported);
            has_code(parse_privkey_pem(input), Code::key_unsupported);
        }
        has_code(parse_pubkey_pem(PRIVATE), Code::key_unsupported);
        has_code(parse_privkey_pem(PUBLIC), Code::key_unsupported);
        has_code(parse_pubkey_pem(&[0xff]), Code::parse);
        has_code(parse_privkey_pem(&[0xff]), Code::parse);
    }

    #[test]
    fn rejects_unsupported_key_sizes() {
        for (bits, code) in [
            (1024, Code::key_too_small),
            (2040, Code::key_too_small),
            (2047, Code::key_too_small),
            (2056, Code::key_unsupported),
            (3072, Code::key_unsupported),
        ] {
            let modulus = (BigUint::from(1u8) << (bits - 1)) + BigUint::from(1u8);
            let key = RsaPublicKey::new(modulus, BigUint::from(65537u32)).unwrap();
            let pem = key.to_public_key_pem(LineEnding::LF).unwrap();
            has_code(parse_pubkey_pem(pem.as_bytes()), code);
        }
    }

    #[test]
    fn rejects_wrong_digest_lengths() {
        let private = success(parse_privkey_pem(PRIVATE));
        let public = success(parse_pubkey_pem(PUBLIC));
        for len in [0, 31, 33, 64] {
            let hash = [0; 64];
            has_code(rsa_sign(&private, &hash[..len]), Code::signature_mismatch);
            has_code(
                rsa_verify(&public, OPENSSL_SIGNATURE, &hash[..len]),
                Code::signature_mismatch,
            );
        }
    }

    #[test]
    fn rejects_signatures_without_sha256_digest_info() {
        let private = success(parse_privkey_pem(PRIVATE));
        let public = success(parse_pubkey_pem(PUBLIC));
        let hash = Sha256::digest(MESSAGE);
        let signature = private
            .key
            .sign(Pkcs1v15Sign::new_unprefixed(), &hash)
            .unwrap();
        has_code(
            rsa_verify(&public, &signature, &hash),
            Code::signature_mismatch,
        );
    }

    #[test]
    fn rejects_tampering_wrong_keys_and_bad_signature_lengths() {
        let public = success(parse_pubkey_pem(PUBLIC));
        let hash = Sha256::digest(MESSAGE);
        let mut wrong_hash = hash;
        wrong_hash[0] ^= 1;
        has_code(
            rsa_verify(&public, OPENSSL_SIGNATURE, &wrong_hash),
            Code::signature_mismatch,
        );
        let mut signature = OPENSSL_SIGNATURE.to_vec();
        signature[0] ^= 1;
        has_code(
            rsa_verify(&public, &signature, &hash),
            Code::signature_mismatch,
        );
        for signature in [&[][..], &OPENSSL_SIGNATURE[..255], &[0; 256], &[0; 257]] {
            has_code(
                rsa_verify(&public, signature, &hash),
                Code::signature_mismatch,
            );
        }
        let other_public = PubKey {
            size: public.size,
            key: RsaPublicKey::new(public.key.n().clone(), BigUint::from(3u8)).unwrap(),
        };
        has_code(
            rsa_verify(&other_public, OPENSSL_SIGNATURE, &hash),
            Code::signature_mismatch,
        );
    }
}
