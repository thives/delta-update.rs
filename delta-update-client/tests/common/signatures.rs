use super::Window;
use delta_update_payload::manifest::Signature;
use delta_update_payload::manifest::Signatures;
use delta_update_payload::rsa::PrivKey;
use delta_update_payload::rsa::PubKey;
use delta_update_payload::rsa::rsa_sign;
use delta_update_payload::rsa::rsa_verify;
use prost::Message;

#[derive(Clone, Copy)]
pub enum Entry {
    #[allow(dead_code)]
    Empty,

    Valid,

    Junk(usize),
}

pub fn encode(entries: &[Entry], valid: &[u8]) -> Vec<u8> {
    return encode_message(entries, valid, true);
}

pub fn encode_without_sizes(entries: &[Entry], valid: &[u8]) -> Vec<u8> {
    return encode_message(entries, valid, false);
}

#[allow(deprecated)]
fn encode_message(entries: &[Entry], valid: &[u8], include_sizes: bool) -> Vec<u8> {
    let mut signatures = Vec::new();
    for entry in entries {
        signatures.push(match *entry {
            Entry::Empty => Signature {
                version: None,
                data: None,
                unpadded_signature_size: None,
            },
            Entry::Valid => Signature {
                version: None,
                data: Some(valid.to_vec()),
                unpadded_signature_size: include_sizes.then_some(valid.len() as u32),
            },
            Entry::Junk(n) => Signature {
                version: None,
                data: Some(junk(n)),
                unpadded_signature_size: include_sizes.then_some(n as u32),
            },
        });
    }
    return Signatures {
        signatures: signatures,
    }
    .encode_to_vec();
}

pub fn junk(n: usize) -> Vec<u8> {
    return vec![0x5a; n];
}

pub fn sign(key: &PrivKey, hash: &[u8]) -> Vec<u8> {
    let signature = match rsa_sign(key, hash) {
        Err(e) => panic!("signing failed: {}", e.msg),
        Ok(s) => s,
    };
    return signature.bytes[..signature.len].to_vec();
}

pub fn pad_to(msg: &mut Vec<u8>, total: usize, field: u64) {
    let tag = (field << 3) | 2;
    let room = total - msg.len();

    let mut len = room;
    while len > 0 && varint_len(tag) + varint_len(len as u64) + len != room {
        len -= 1;
    }
    put_varint(msg, tag);
    put_varint(msg, len as u64);
    msg.resize(msg.len() + len, 0);
    assert_eq!(msg.len(), total, "padding did not fill the message exactly");
}

fn put_varint(out: &mut Vec<u8>, mut v: u64) {
    loop {
        let b = (v & 0x7f) as u8;
        v >>= 7;
        if v == 0 {
            out.push(b);
            return;
        }
        out.push(b | 0x80);
    }
}

fn varint_len(mut v: u64) -> usize {
    let mut n = 1;
    while v >= 0x80 {
        v >>= 7;
        n += 1;
    }
    return n;
}

pub fn rsa_allowance(key: &PubKey, valid: &[u8], hash: &[u8]) -> usize {
    let junk = junk(valid.len());
    let (verified, valid_peak) = Window::new().measure(|| rsa_verify(key, valid, hash));
    assert!(verified.is_ok(), "the valid signature did not verify");
    let (refused, junk_peak) = Window::new().measure(|| rsa_verify(key, &junk, hash));
    assert!(refused.is_err(), "junk bytes verified");
    return valid_peak.max(junk_peak);
}
