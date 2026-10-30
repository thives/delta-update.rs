//! Incremental and one-shot SHA-256 hashing backed by `sha2`.

use sha2::Digest;
use sha2::Sha256 as Hasher;
use sha2::digest::common::array::Array;
use sha2::digest::common::hazmat::SerializableState;
use sha2::digest::common::hazmat::SerializedState;
use sha2::digest::common::typenum::Unsigned;

/// Size of a SHA-256 digest in bytes.
pub const SHA256_SIZE: usize = 32;

/// An in-progress SHA-256 hash. Finalization consumes the state.
#[derive(Clone)]
pub struct Sha256 {
    inner: Hasher,
}

/// Starts a new SHA-256 hash.
pub fn new_hash() -> Sha256 {
    Sha256 {
        inner: Hasher::new(),
    }
}

/// Appends bytes to the message. Empty slices leave the hash unchanged.
pub fn update(h: &mut Sha256, data: &[u8]) {
    h.inner.update(data);
}

/// Consumes the hash state and returns the raw digest bytes.
pub fn finish(h: Sha256) -> [u8; SHA256_SIZE] {
    h.inner.finalize().into()
}

/// Hashes a complete message and returns the raw digest bytes.
pub fn hash_bytes(data: &[u8]) -> [u8; SHA256_SIZE] {
    let mut h = new_hash();
    update(&mut h, data);
    finish(h)
}

/// Layout version of a [`snapshot`]. A restore rejects any other value, so a
/// future layout cannot be read as this one.
pub const SNAPSHOT_VERSION: u8 = 1;

/// Bytes of the algorithm's serialized state, as the `digest` crate defines it:
/// eight words of chaining value, the buffered partial block, and the length.
pub const STATE_SIZE: usize =
    <<Hasher as SerializableState>::SerializedStateSize as Unsigned>::USIZE;

/// Bytes of a [`snapshot`]: the version, then the serialized state.
pub const SNAPSHOT_SIZE: usize = 1 + STATE_SIZE;

/// Snapshots an in-progress hash so it can be restored later, possibly by another
/// process. The bytes are the `digest` crate's supported state serialization
/// behind a version byte. Raw memory of the hash object is never copied.
pub fn snapshot(h: &Sha256) -> [u8; SNAPSHOT_SIZE] {
    let state = h.inner.serialize();
    let mut out = [SNAPSHOT_VERSION; SNAPSHOT_SIZE];
    out[1..].copy_from_slice(state.as_slice());
    return out;
}

/// Restores a hash from a [`snapshot`]. `None` when the bytes are not a snapshot
/// of this version and length, or when the algorithm refuses the state. A
/// restored hash continues exactly as the one that was snapshotted.
pub fn restore(bytes: &[u8]) -> Option<Sha256> {
    if bytes.len() != SNAPSHOT_SIZE || bytes[0] != SNAPSHOT_VERSION {
        return None;
    }
    let state: SerializedState<Hasher> = match Array::try_from(&bytes[1..]) {
        Err(_) => return None,
        Ok(s) => s,
    };
    return match Hasher::deserialize(&state) {
        Err(_) => None,
        Ok(inner) => Some(Sha256 { inner: inner }),
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    fn digest_from_hex(hex: &str) -> [u8; SHA256_SIZE] {
        assert_eq!(hex.len(), SHA256_SIZE * 2);
        core::array::from_fn(|i| u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).unwrap())
    }

    #[test]
    fn matches_known_answer_vectors() {
        for (message, expected) in [
            (
                &b""[..],
                "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
            ),
            (
                &b"abc"[..],
                "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad",
            ),
            (
                &b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"[..],
                "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1",
            ),
            (
                &b"abcdefghbcdefghicdefghijdefghijkefghijklfghijklmghijklmnhijklmno\
                   ijklmnopjklmnopqklmnopqrlmnopqrsmnopqrstnopqrstu"[..],
                "cf5b16a778af8380036ce59e7b0492370b249b11e8f07a51afac45037afee9d1",
            ),
        ] {
            assert_eq!(hash_bytes(message), digest_from_hex(expected));
        }
    }

    #[test]
    fn matches_golden_reference() {
        let expected =
            digest_from_hex("8f434346648f6b96df89dda901c5176b10a6d83961dd3c1ac88b59b2dc327aa4");
        assert_eq!(hash_bytes(b"hi"), expected);

        let mut h = new_hash();
        update(&mut h, b"h");
        update(&mut h, b"i");
        assert_eq!(finish(h), expected);
    }

    #[test]
    fn empty_updates_leave_hash_unchanged() {
        assert_eq!(finish(new_hash()), hash_bytes(b""));
        let mut h = new_hash();
        update(&mut h, b"");
        update(&mut h, b"abc");
        update(&mut h, b"");
        assert_eq!(finish(h), hash_bytes(b"abc"));
    }

    #[test]
    fn chunking_preserves_digest_at_block_and_padding_boundaries() {
        let data: [u8; 1024] = core::array::from_fn(|i| i as u8);
        for len in [0, 1, 55, 56, 63, 64, 65, 111, 112, 127, 128, 129, 1024] {
            let message = &data[..len];
            let expected = hash_bytes(message);
            for chunk_size in [1, 7, 55, 56, 63, 64, 65, 128] {
                let mut h = new_hash();
                for chunk in message.chunks(chunk_size) {
                    update(&mut h, chunk);
                }
                assert_eq!(finish(h), expected, "len={len}, chunk_size={chunk_size}");
            }
        }
    }

    #[test]
    fn snapshot_restores_to_the_same_digest_at_every_split_point() {
        let data: [u8; 300] = core::array::from_fn(|i| (i * 7) as u8);
        let expected = hash_bytes(&data);
        for split in [0, 1, 55, 56, 63, 64, 65, 127, 128, 129, 299, 300] {
            let mut h = new_hash();
            update(&mut h, &data[..split]);
            let snap = snapshot(&h);
            let mut restored = restore(&snap).unwrap();
            update(&mut restored, &data[split..]);
            assert_eq!(finish(restored), expected, "split={split}");
        }
    }

    #[test]
    fn snapshot_is_bounded_and_versioned() {
        let mut h = new_hash();
        let empty = snapshot(&h);
        update(&mut h, &[0u8; 10_000]);
        let full = snapshot(&h);
        assert_eq!(empty.len(), SNAPSHOT_SIZE);
        assert_eq!(full.len(), SNAPSHOT_SIZE);
        assert!(SNAPSHOT_SIZE <= 128, "snapshot grew to {SNAPSHOT_SIZE} bytes");
        assert_eq!(empty[0], SNAPSHOT_VERSION);

        let mut other = full;
        other[0] = SNAPSHOT_VERSION + 1;
        assert!(restore(&other).is_none());
        assert!(restore(&full[..SNAPSHOT_SIZE - 1]).is_none());
        let mut longer = full.to_vec();
        longer.push(0);
        assert!(restore(&longer).is_none());
        assert!(restore(&[]).is_none());
    }

    #[test]
    fn matches_million_a_vector() {
        let mut h = new_hash();
        for _ in 0..1000 {
            update(&mut h, &[b'a'; 1000]);
        }
        assert_eq!(
            finish(h),
            digest_from_hex("cdc76e5c9914fb9281a1c7e284d73e67f1809a48a497200e046d39ccc7112cd0")
        );
    }
}
