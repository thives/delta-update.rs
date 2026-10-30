extern crate std;

use std::cell::Cell;
use std::vec::Vec;

use crate::image::OldImage;
use crate::sink::Readable;
use crate::sink::Sink;
use crate::sink::check_range;
use crate::sink::io_error;
use crate::source::Source;
use delta_update_generator::SearchConfig;
use delta_update_generator::build_payload_with;
use delta_update_generator::builder::Kind;
use delta_update_generator::builder::Op;
use delta_update_generator::builder::Spec;
use delta_update_generator::builder::build;
use delta_update_payload::error::PayloadError;
use delta_update_payload::layout;
use delta_update_payload::manifest::Extent;
use delta_update_payload::manifest::Manifest;
use delta_update_payload::manifest::Operation;
use delta_update_payload::manifest::PartitionUpdate;
use delta_update_payload::profile::EMBEDDED;
use delta_update_payload::rsa::PubKey;
use delta_update_payload::rsa::parse_pubkey_pem;
use delta_update_payload::sha256;
use prost::Message;

pub const BS: usize = 512;
pub const BLOCKS: usize = 8;

const PRIVATE: &[u8] = include_bytes!("../../delta-update-payload/tests/fixtures/private.pem");
const PUBLIC: &[u8] = include_bytes!("../../delta-update-payload/tests/fixtures/public.pem");

pub struct Dev {
    pub data: Vec<u8>,
    pub writes: usize,
    pub reads: Cell<usize>,
    pub fail_flush: bool,
    pub drop_writes: bool,

    pub fail_write_at: Option<usize>,
}

impl Dev {
    pub fn new(data: Vec<u8>) -> Dev {
        return Dev {
            data: data,
            writes: 0,
            reads: Cell::new(0),
            fail_flush: false,
            drop_writes: false,
            fail_write_at: None,
        };
    }
}

impl Sink for Dev {
    fn capacity(&self) -> u64 {
        return self.data.len() as u64;
    }

    fn write_at(&mut self, offset: u64, data: &[u8]) -> Result<(), PayloadError> {
        match check_range(offset, data.len(), self.capacity()) {
            Err(e) => return Err(e),
            Ok(()) => {}
        };
        if self.fail_write_at == Some(self.writes) {
            return Err(io_error());
        }
        self.writes += 1;
        if !self.drop_writes {
            let off = offset as usize;
            self.data[off..(off + data.len())].copy_from_slice(data);
        }
        return Ok(());
    }

    fn flush(&mut self) -> Result<(), PayloadError> {
        if self.fail_flush {
            return Err(io_error());
        }
        return Ok(());
    }
}

impl Readable for Dev {
    fn read_at(&self, offset: u64, out: &mut [u8]) -> Result<(), PayloadError> {
        match check_range(offset, out.len(), self.capacity()) {
            Err(e) => return Err(e),
            Ok(()) => {}
        };
        self.reads.set(self.reads.get() + 1);
        let off = offset as usize;
        out.copy_from_slice(&self.data[off..(off + out.len())]);
        return Ok(());
    }
}

pub struct Chunked {
    pub data: Vec<u8>,
    pub pos: usize,
    pub chunk: usize,
}

impl Chunked {
    pub fn new(data: Vec<u8>, chunk: usize) -> Chunked {
        return Chunked {
            data: data,
            pos: 0,
            chunk: chunk,
        };
    }

    pub fn at(data: &[u8], pos: usize, chunk: usize) -> Chunked {
        return Chunked {
            data: data.to_vec(),
            pos: pos,
            chunk: chunk,
        };
    }
}

impl Source for Chunked {
    fn read(&mut self, out: &mut [u8]) -> Result<usize, PayloadError> {
        let remaining = self.data.len() - self.pos;
        let n = out.len().min(self.chunk).min(remaining);
        out[..n].copy_from_slice(&self.data[self.pos..(self.pos + n)]);
        self.pos += n;
        return Ok(n);
    }
}

fn block_val(b: usize, i: usize) -> u8 {
    return (((b * 37 + i * 11) % 251) + 1) as u8;
}

fn block(b: usize, variant: usize) -> Vec<u8> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < BS {
        out.push(block_val(b + variant * 100, i));
        i += 1;
    }
    return out;
}

pub struct SliceImage<'a> {
    pub data: &'a [u8],
}

impl<'a> OldImage for SliceImage<'a> {
    fn capacity(self: &Self) -> u64 {
        return self.data.len() as u64;
    }

    fn read_at(self: &Self, offset: u64, out: &mut [u8]) -> Result<(), PayloadError> {
        let start = offset as usize;
        let end = start + out.len();
        if end > self.data.len() {
            return Err(PayloadError {
                code: delta_update_payload::error::Code::source_missing,
                msg: "test image read past its end",
            });
        }
        out.copy_from_slice(&self.data[start..end]);
        return Ok(());
    }
}

pub fn old_image() -> Vec<u8> {
    let mut out = Vec::new();
    for b in 0..BLOCKS {
        out.extend(block(b, 0));
    }
    return out;
}

pub fn new_image() -> Vec<u8> {
    let mut out = Vec::new();
    for b in 0..BLOCKS {
        let chunk = match b {
            0 | 5 => block(b, 0),
            2 | 6 => std::vec![0u8; BS],
            _ => block(b, 1),
        };
        out.extend(chunk);
    }
    return out;
}

pub fn keys() -> Vec<PubKey> {
    let mut v = Vec::new();
    v.push(parse_pubkey_pem(PUBLIC).ok().unwrap());
    return v;
}

pub fn signed_payload() -> Vec<u8> {
    return match build_payload_with(
        &old_image(),
        &new_image(),
        b"A/B",
        PRIVATE,
        BS,
        &EMBEDDED,
        &SearchConfig::quick(),
    ) {
        Err(e) => panic!("build_payload failed: {}", e.msg),
        Ok(v) => v,
    };
}

pub fn write_only_payload() -> Vec<u8> {
    return match delta_update_generator::build_full_payload_with_release(
        &new_image(),
        b"A/B",
        PRIVATE,
        BS,
        &EMBEDDED,
        0,
    ) {
        Err(e) => panic!("build_payload failed: {}", e.msg),
        Ok(v) => v,
    };
}

pub fn two_op_payload() -> Vec<u8> {
    let old = old_image();
    let new = new_image();
    let half = (old.len() / 2) as u64;
    let mut first = Op::new(Kind::Replace, std::vec![(0, half)]);
    first.data = new[..half as usize].to_vec();
    let mut second = Op::new(Kind::Replace, std::vec![(half, half)]);
    second.data = new[half as usize..].to_vec();
    let spec = Spec {
        chunk: BS,
        partition: old.len() as u64,
        old_hash: Some(delta_update_payload::sha256::hash_bytes(&old)),
        ops: std::vec![first, second],
        sign: Some(PRIVATE),
    };
    return match build(&spec) {
        Err(e) => panic!("builder failed: {}", e.msg),
        Ok(v) => v,
    };
}

pub fn unsigned_payload(typ: i32) -> Vec<u8> {
    let blob = block(0, 1);
    let mut h = sha256::new_hash();
    sha256::update(&mut h, &blob);
    let digest = sha256::finish(h);
    let op = Operation {
        r#type: typ,
        data_offset: Some(0),
        data_length: Some(BS as u64),
        dst_extents: std::vec![Extent {
            start_byte: Some(0),
            num_bytes: Some(BS as u64),
        }],
        data_sha256_hash: Some(digest.to_vec()),
        ..Default::default()
    };
    let part = PartitionUpdate {
        partition_name: "A/B".into(),
        operations: std::vec![op],
        ..Default::default()
    };
    let manifest = Manifest {
        block_size: Some(BS as u32),
        minor_version: Some(layout::PROJECT_MINOR),
        partitions: std::vec![part],
        ..Default::default()
    }
    .encode_to_vec();
    let mut payload = Vec::new();
    payload.extend_from_slice(b"CrAU");
    payload.extend_from_slice(&2u64.to_be_bytes());
    payload.extend_from_slice(&(manifest.len() as u64).to_be_bytes());
    payload.extend_from_slice(&0u32.to_be_bytes());
    payload.extend_from_slice(&manifest);
    payload.extend_from_slice(&blob);
    return payload;
}
