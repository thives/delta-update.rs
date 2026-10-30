//! Durable progress of one update: the record that lets an interrupted apply
//! resume at its first uncommitted operation.
//!
//! A checkpoint says three things, and each is checked against the authenticated
//! metadata before it is used: which payload it belongs to (the digest of the
//! signed header and manifest), which target it was written to (a host-supplied
//! identity), and how far the update has got (the next operation, the absolute
//! payload offset of that operation's blob, and the payload-signature hash state
//! over everything before it). `docs/checkpoint.md` states the record layout and
//! the contract the host must meet.
//!
//! The record is written only through [`CheckpointStore::replace`], which the
//! host must make atomic and durable. The record carries a SHA-256 over its
//! bytes. That detects a torn or damaged record and nothing more: checkpoint
//! storage is trusted execution state, not an authenticated input.

use ota_payload::error::Code;
use ota_payload::error::PayloadError;
use ota_payload::error::err;
use ota_payload::sha256;

/// Identity of the target a checkpoint was written to. The host chooses it, and
/// the same target must always receive the same value. Two different devices
/// must not share one.
pub type TargetId = [u8; sha256::SHA256_SIZE];

/// Identity of the authenticated metadata: the digest of the signed header and
/// manifest. A checkpoint is valid only for the payload it was written from.
pub type MetadataDigest = [u8; sha256::SHA256_SIZE];

const MAGIC: [u8; 4] = *b"OTAC";
/// Layout version this crate writes. Version 2 adds the `Mismatch` phase (code 4).
/// The field layout is unchanged from version 1, so version-1 records still decode:
/// their phases are a subset of version 2's (`docs/checkpoint.md` §9).
pub const VERSION: u8 = 2;


const READABLE_VERSIONS: [u8; 2] = [1, VERSION];

/// Bytes of a record: its fields, then a SHA-256 over them.
pub const RECORD_SIZE: usize = BODY_SIZE + sha256::SHA256_SIZE;


const BODY_SIZE: usize = 4 + 1 + 1 + 2 + 4 + 8 + 32 + 32 + sha256::SNAPSHOT_SIZE;
const PHASE_AT: usize = 5;
const NEXT_OP_AT: usize = 8;
const OFFSET_AT: usize = 12;
const METADATA_AT: usize = 20;
const TARGET_AT: usize = 52;
const HASH_AT: usize = 84;

/// Where an update stands. The order is the order of progress.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Phase {
    /// Recorded before the first write. Operation zero has not been committed.
    Initialized,
    /// Some operations are committed, and at least one remains.
    Applying,
    /// Every operation is committed. The payload signature and the post-flight
    /// hash are still to be checked.
    AwaitingFinal,
    /// The update is verified. Re-applying this payload to this target writes
    /// nothing; it re-checks the payload's stream and, on a readable target, the
    /// new-image hash.
    Complete,
    /// Every operation is committed, but the target does not match the authenticated
    /// new image. Nothing is written or completed until the host restores the target
    /// and resets the checkpoint (`docs/checkpoint.md` §8). Written only after a
    /// failed final hash, and only by a version-2 writer.
    Mismatch,
}

impl Phase {
    fn code(self) -> u8 {
        return match self {
            Phase::Initialized => 0,
            Phase::Applying => 1,
            Phase::AwaitingFinal => 2,
            Phase::Complete => 3,
            Phase::Mismatch => 4,
        };
    }

    fn from_code(code: u8) -> Option<Phase> {
        return match code {
            0 => Some(Phase::Initialized),
            1 => Some(Phase::Applying),
            2 => Some(Phase::AwaitingFinal),
            3 => Some(Phase::Complete),
            4 => Some(Phase::Mismatch),
            _ => None,
        };
    }
}

/// The decoded fields of a checkpoint.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Record {
    pub phase: Phase,
    /// Index of the first operation not yet committed.
    pub next_op: u32,
    /// Absolute payload offset of that operation's blob: the offset of the first
    /// byte the stream must supply to resume. Equal to the end of the metadata
    /// signature when `next_op` is zero, and to the payload signature's start when
    /// every operation is committed.
    pub payload_offset: u64,
    pub metadata: MetadataDigest,
    pub target: TargetId,
    /// The payload-signature hash state over the bytes before `payload_offset`,
    /// as [`sha256::snapshot`] records it.
    pub hash: [u8; sha256::SNAPSHOT_SIZE],
}

impl Record {
    /// The record's bytes: fields in fixed positions, then their SHA-256.
    pub fn encode(&self) -> [u8; RECORD_SIZE] {
        let mut out = [0u8; RECORD_SIZE];
        out[..4].copy_from_slice(&MAGIC);
        out[4] = VERSION;
        out[PHASE_AT] = self.phase.code();
        out[NEXT_OP_AT..OFFSET_AT].copy_from_slice(&self.next_op.to_be_bytes());
        out[OFFSET_AT..METADATA_AT].copy_from_slice(&self.payload_offset.to_be_bytes());
        out[METADATA_AT..TARGET_AT].copy_from_slice(&self.metadata);
        out[TARGET_AT..HASH_AT].copy_from_slice(&self.target);
        out[HASH_AT..BODY_SIZE].copy_from_slice(&self.hash);
        let digest = sha256::hash_bytes(&out[..BODY_SIZE]);
        out[BODY_SIZE..].copy_from_slice(&digest);
        return out;
    }

    /// Decode and check a record. A record of the wrong length, magic, version,
    /// phase, or digest is `checkpoint_invalid`, and it is never partly trusted.
    pub fn decode(bytes: &[u8]) -> Result<Record, PayloadError> {
        if bytes.len() != RECORD_SIZE {
            return Err(invalid("checkpoint record has the wrong length"));
        }
        if bytes[..4] != MAGIC || !READABLE_VERSIONS.contains(&bytes[4]) {
            return Err(invalid("checkpoint record is not this version"));
        }
        let digest = sha256::hash_bytes(&bytes[..BODY_SIZE]);
        if digest[..] != bytes[BODY_SIZE..] {
            return Err(invalid("checkpoint record fails its integrity check"));
        }
        let phase = match Phase::from_code(bytes[PHASE_AT]) {
            None => return Err(invalid("checkpoint phase is unknown")),
            Some(p) => p,
        };
        let mut next = [0u8; 4];
        next.copy_from_slice(&bytes[NEXT_OP_AT..OFFSET_AT]);
        let mut offset = [0u8; 8];
        offset.copy_from_slice(&bytes[OFFSET_AT..METADATA_AT]);
        let mut metadata = [0u8; 32];
        metadata.copy_from_slice(&bytes[METADATA_AT..TARGET_AT]);
        let mut target = [0u8; 32];
        target.copy_from_slice(&bytes[TARGET_AT..HASH_AT]);
        let mut hash = [0u8; sha256::SNAPSHOT_SIZE];
        hash.copy_from_slice(&bytes[HASH_AT..BODY_SIZE]);
        return Ok(Record {
            phase: phase,
            next_op: u32::from_be_bytes(next),
            payload_offset: u64::from_be_bytes(offset),
            metadata: metadata,
            target: target,
            hash: hash,
        });
    }
}

fn invalid(msg: &'static str) -> PayloadError {
    return err(Code::checkpoint_invalid, msg);
}

/// Durable storage for one update's checkpoint record. The host provides it.
pub trait CheckpointStore {
    /// Read the stored record into `out`. Returns `Ok(false)` when no record has
    /// been stored, which is a fresh update. What it returns is the record that is
    /// visible, which may not be durable yet: a resume must call
    /// [`confirm_stored`](CheckpointStore::confirm_stored) before it trusts the record.
    /// A reset does not, since removing the record syncs the same directory.
    fn load(self: &Self, out: &mut [u8; RECORD_SIZE]) -> Result<bool, PayloadError>;

    /// Replace the stored record with `record`. The replacement must be atomic:
    /// after an interruption, a load returns either the old record or the new one,
    /// never a mixture. It must also be durable: once this returns `Ok`, the new
    /// record survives power loss. A store that cannot promise that must not
    /// return `Ok`. A store that fails after the new record has become visible
    /// (a failed directory sync after the rename) returns the error, and leaves the
    /// record unconfirmed until [`confirm_stored`](CheckpointStore::confirm_stored)
    /// succeeds.
    fn replace(self: &mut Self, record: &[u8; RECORD_SIZE]) -> Result<(), PayloadError>;

    /// Confirm that the record a load returned is durable, before a resume trusts it
    /// for any write. A store syncs whatever it must to make the record survive a
    /// power loss, and returns `Ok` only when that has succeeded. A failure is
    /// `checkpoint_unconfirmed`: the record is kept, and the run writes nothing. A
    /// store that cannot tell from a fresh process whether an earlier replacement was
    /// confirmed, such as a file store, syncs on every call. A store that tracks its
    /// own history may return `Ok` without any I/O when nothing is pending.
    fn confirm_stored(self: &mut Self) -> Result<(), PayloadError>;

    /// Remove the stored record, so the next load reports a fresh update. Only
    /// [`engine::reset`](crate::engine::reset) calls it, after it has established that
    /// a restart is safe. Like `replace`, it must be atomic and durable.
    ///
    /// A failure before the record is removed leaves it as it was. A failure after the
    /// removal, while making it durable, is `checkpoint_removal_uncertain`: the record is
    /// gone for now, but it may reappear after a crash, so the host must inspect the
    /// record and the target before it retries (`docs/checkpoint.md` §8.1).
    fn clear(self: &mut Self) -> Result<(), PayloadError>;

    /// Confirm that no record is stored, and that its absence is durable. A reset calls it
    /// when [`load`](CheckpointStore::load) finds no record, so a reset retried after an
    /// uncertain removal does not report success until the absence is on disk. It removes
    /// nothing. A stored record is `checkpoint_present`, and it is left in place. A store that
    /// cannot confirm the absence reports `checkpoint_removal_uncertain`, and must not return `Ok`.
    fn confirm_absent(self: &mut Self) -> Result<(), PayloadError>;
}

/// A checkpoint store that lives in memory. It models the two states a host store has: the record a
/// process sees (`record`), and the record a power cut leaves (`durable`). A replacement or a removal
/// that reports success makes its result durable. One that fails after it took effect leaves the visible
/// state changed and the durable state unchanged, which is the crash state a failed directory sync
/// leaves. [`crash`](MemoryStore::crash) discards the visible state, as a power cut does. The store is for
/// tests and dry runs: it claims no durability beyond this model.
pub struct MemoryStore {

    record: Option<[u8; RECORD_SIZE]>,

    durable: Option<[u8; RECORD_SIZE]>,


    pending: bool,
    /// How many replacements have been made, for tests that count them.
    pub replacements: usize,
    /// When set to `n`, the replacement that would be number `n` (counting from zero)
    /// fails before it stores anything, and the stored record stays as it was.
    pub fail_at: Option<usize>,
    /// When set to `n`, the replacement that would be number `n` stores its record and then fails
    /// to make it durable, as a directory sync after the rename does. The record is then visible
    /// but unconfirmed.
    pub fail_sync_at: Option<usize>,
    /// When set, `confirm_stored` fails, so a visible record that is not durable stays unconfirmed.
    pub fail_confirm: bool,
    /// When set, `clear` fails before it removes anything.
    pub fail_clear: bool,
    /// When set, `clear` removes the record and then fails to make the removal durable,
    /// as a failed directory sync does. It reports `checkpoint_removal_uncertain`, and so does
    /// `confirm_absent`, since the same sync is what would confirm the absence.
    pub fail_clear_sync: bool,
}

impl MemoryStore {
    pub fn new() -> MemoryStore {
        return MemoryStore {
            record: None,
            durable: None,
            pending: false,
            replacements: 0,
            fail_at: None,
            fail_sync_at: None,
            fail_confirm: false,
            fail_clear: false,
            fail_clear_sync: false,
        };
    }

    /// The stored record, decoded, for inspection.
    pub fn stored(&self) -> Option<Record> {
        return match &self.record {
            None => None,
            Some(r) => Record::decode(r).ok(),
        };
    }

    /// Replace the stored bytes directly, to model a damaged record in tests. The bytes are already on
    /// the device, so they are durable.
    pub fn overwrite(&mut self, bytes: &[u8]) {
        let mut r = [0u8; RECORD_SIZE];
        r.copy_from_slice(&bytes[..RECORD_SIZE]);
        self.record = Some(r);
        self.durable = Some(r);
        self.pending = false;
    }

    /// A power cut: the visible state becomes the durable one, so an unconfirmed record is lost, and an
    /// unconfirmed removal comes back.
    pub fn crash(&mut self) {
        self.record = self.durable;
        self.pending = false;
    }
}

impl CheckpointStore for MemoryStore {
    fn load(self: &Self, out: &mut [u8; RECORD_SIZE]) -> Result<bool, PayloadError> {
        return match &self.record {
            None => Ok(false),
            Some(r) => {
                out.copy_from_slice(r);
                Ok(true)
            }
        };
    }

    fn replace(self: &mut Self, record: &[u8; RECORD_SIZE]) -> Result<(), PayloadError> {
        let index = self.replacements;
        if self.fail_at == Some(index) {
            return Err(crate::sink::io_error());
        }
        self.record = Some(*record);
        self.replacements += 1;
        if self.fail_sync_at == Some(index) {
            self.pending = true;
            return Err(crate::sink::io_error());
        }
        self.durable = Some(*record);
        self.pending = false;
        return Ok(());
    }

    fn confirm_stored(self: &mut Self) -> Result<(), PayloadError> {
        if !self.pending {
            return Ok(());
        }
        if self.fail_confirm {
            return Err(err(
                Code::checkpoint_unconfirmed,
                "checkpoint is stored, but its replacement is not confirmed durable; it is not used",
            ));
        }
        self.durable = self.record;
        self.pending = false;
        return Ok(());
    }

    fn clear(self: &mut Self) -> Result<(), PayloadError> {
        if self.fail_clear {
            return Err(crate::sink::io_error());
        }
        self.record = None;
        self.pending = false;
        if self.fail_clear_sync {
            return Err(err(
                Code::checkpoint_removal_uncertain,
                "checkpoint removed, but the removal is not confirmed durable",
            ));
        }
        self.durable = None;
        return Ok(());
    }

    fn confirm_absent(self: &mut Self) -> Result<(), PayloadError> {
        if self.record.is_some() {
            return Err(err(
                Code::checkpoint_present,
                "checkpoint is present, so the reset that expected none removes nothing",
            ));
        }
        if self.fail_clear_sync {
            return Err(err(
                Code::checkpoint_removal_uncertain,
                "checkpoint is absent, but its absence is not confirmed durable",
            ));
        }
        self.durable = None;
        return Ok(());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ota_payload::sha256::SNAPSHOT_SIZE;

    fn sample() -> Record {
        return Record {
            phase: Phase::Applying,
            next_op: 7,
            payload_offset: 4242,
            metadata: [1; 32],
            target: [2; 32],
            hash: [3; SNAPSHOT_SIZE],
        };
    }

    #[test]
    fn a_record_round_trips_through_its_bytes() {
        let r = sample();
        let bytes = r.encode();
        assert_eq!(Record::decode(&bytes).unwrap(), r);
        assert_eq!(RECORD_SIZE, bytes.len());
    }

    #[test]
    fn every_single_byte_corruption_is_refused() {
        let bytes = sample().encode();
        for i in 0..RECORD_SIZE {
            let mut bad = bytes;
            bad[i] ^= 0x01;
            assert!(
                Record::decode(&bad).is_err(),
                "byte {i} corruption accepted"
            );
        }
    }

    #[test]
    fn wrong_length_and_unknown_version_are_refused() {
        let bytes = sample().encode();
        assert!(Record::decode(&bytes[..RECORD_SIZE - 1]).is_err());
        let mut longer = bytes.to_vec();
        longer.push(0);
        assert!(Record::decode(&longer).is_err());


        let mut other = sample();
        other.phase = Phase::Complete;
        let mut body = other.encode();
        body[4] = VERSION + 1;
        let digest = sha256::hash_bytes(&body[..BODY_SIZE]);
        body[BODY_SIZE..].copy_from_slice(&digest);
        assert_eq!(
            Record::decode(&body).err().map(|e| e.code),
            Some(Code::checkpoint_invalid)
        );
    }

    #[test]
    fn the_memory_store_replaces_and_can_fail_once() {
        let mut store = MemoryStore::new();
        let mut out = [0u8; RECORD_SIZE];
        assert!(!store.load(&mut out).unwrap());
        store.replace(&sample().encode()).unwrap();
        assert!(store.load(&mut out).unwrap());
        assert_eq!(Record::decode(&out).unwrap(), sample());
        store.fail_at = Some(1);
        let mut next = sample();
        next.next_op = 8;
        assert!(store.replace(&next.encode()).is_err());

        assert_eq!(store.stored().unwrap().next_op, 7);
        assert_eq!(store.replacements, 1);

        store.fail_at = None;
        store.replace(&next.encode()).unwrap();
        assert_eq!(store.stored().unwrap().next_op, 8);
    }

    #[test]
    fn a_version_one_record_still_decodes_and_a_mismatch_round_trips() {
        let mut v1 = sample().encode();
        v1[4] = 1;
        let digest = sha256::hash_bytes(&v1[..BODY_SIZE]);
        v1[BODY_SIZE..].copy_from_slice(&digest);
        assert_eq!(Record::decode(&v1).unwrap(), sample());
        let mut mismatch = sample();
        mismatch.phase = Phase::Mismatch;
        assert_eq!(Record::decode(&mismatch.encode()).unwrap(), mismatch);
    }

    #[test]
    fn clear_removes_the_record_and_can_fail_without_removing_it() {
        let mut store = MemoryStore::new();
        store.replace(&sample().encode()).unwrap();
        store.fail_clear = true;
        assert!(store.clear().is_err());
        assert!(store.stored().is_some());
        store.fail_clear = false;
        store.clear().unwrap();
        assert!(store.stored().is_none());
    }

    #[test]
    fn a_removal_that_is_not_confirmed_durable_is_reported_and_the_record_is_gone() {
        let mut store = MemoryStore::new();
        store.replace(&sample().encode()).unwrap();
        store.fail_clear_sync = true;
        assert_eq!(
            store.clear().err().map(|e| e.code),
            Some(Code::checkpoint_removal_uncertain)
        );

        assert!(store.stored().is_none());
    }

    #[test]
    fn confirm_absent_refuses_a_stored_record_and_leaves_it_in_place() {
        let mut store = MemoryStore::new();
        store.replace(&sample().encode()).unwrap();
        assert_eq!(
            store.confirm_absent().err().map(|e| e.code),
            Some(Code::checkpoint_present)
        );
        assert_eq!(store.stored(), Some(sample()));
    }

    #[test]
    fn confirm_absent_of_an_absent_record_reports_uncertainty_until_the_sync_works() {
        let mut store = MemoryStore::new();
        store.fail_clear_sync = true;
        assert_eq!(
            store.confirm_absent().err().map(|e| e.code),
            Some(Code::checkpoint_removal_uncertain)
        );
        store.fail_clear_sync = false;
        assert!(store.confirm_absent().is_ok());
        assert!(store.stored().is_none());
    }

    #[test]
    fn a_replacement_whose_sync_fails_is_visible_but_lost_by_a_crash_until_confirmed() {
        let mut store = MemoryStore::new();
        store.replace(&sample().encode()).unwrap();
        let mut next = sample();
        next.next_op = 8;
        store.fail_sync_at = Some(1);
        assert!(store.replace(&next.encode()).is_err());

        assert_eq!(store.stored().unwrap().next_op, 8);
        store.crash();
        assert_eq!(store.stored().unwrap().next_op, 7);
    }

    #[test]
    fn confirm_stored_makes_a_visible_record_durable_and_a_crash_then_keeps_it() {
        let mut store = MemoryStore::new();
        store.fail_sync_at = Some(0);
        assert!(store.replace(&sample().encode()).is_err());
        store.confirm_stored().unwrap();
        store.crash();
        assert_eq!(store.stored(), Some(sample()));
    }

    #[test]
    fn a_failed_confirmation_leaves_the_record_unconfirmed_and_refuses_again() {
        let mut store = MemoryStore::new();
        store.fail_sync_at = Some(0);
        assert!(store.replace(&sample().encode()).is_err());
        store.fail_confirm = true;
        for _ in 0..2 {
            assert_eq!(
                store.confirm_stored().err().map(|e| e.code),
                Some(Code::checkpoint_unconfirmed)
            );
        }

        store.crash();
        assert!(store.stored().is_none());
    }

    #[test]
    fn a_successful_replacement_confirms_the_one_before_it() {
        let mut store = MemoryStore::new();
        store.fail_sync_at = Some(0);
        assert!(store.replace(&sample().encode()).is_err());
        let mut next = sample();
        next.next_op = 9;
        store.replace(&next.encode()).unwrap();
        store.crash();
        assert_eq!(store.stored().unwrap().next_op, 9);
    }

    #[test]
    fn confirm_stored_of_a_durable_record_writes_nothing_and_succeeds() {
        let mut store = MemoryStore::new();
        store.replace(&sample().encode()).unwrap();
        store.fail_confirm = true;
        assert!(store.confirm_stored().is_ok());
        assert_eq!(store.stored(), Some(sample()));
    }
}
