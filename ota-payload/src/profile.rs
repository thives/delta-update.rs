//! The resource profile: the single owner of every size and count limit that
//! bounds untrusted payload metadata.
//!
//! Two profiles exist. [`EMBEDDED`] is the documented baseline for the
//! `no_std` client on a device with about 100 KiB of RAM. [`HOST`] is a
//! deliberate, much larger override for desktop tooling and tests. The
//! generator and the client both take a profile, so a payload the generator
//! emits under a profile is one the client accepts under the same profile.
//!
//! Limits are enforced on two axes. Counts and byte sizes are checked
//! individually by the pre-decode scan in [`prescan`](crate::prescan). The
//! decoded-memory budget bounds the modeled decode peak: the heap the owned
//! prost messages hold, the largest temporary that is live alongside it, and
//! allocator overhead. The model, the allocator assumptions behind it, and the
//! measurements that check it are in `docs/memory.md`.

/// Bytes charged per heap allocation for allocator bookkeeping and alignment.
///
/// This is an assumption about the target allocator, not a measurement of it.
/// It covers an 8-byte block header plus rounding up to a 16-byte granule, which
/// adds at most 23 bytes, rounded to 24. The host tests simulate exactly that
/// allocator, so the assumption is checked there, not on the device.
pub const ALLOC_OVERHEAD: usize = 24;
/// Smallest capacity of a byte vector that holds data. prost fills byte and
/// string fields through `Vec<u8>`, whose minimum non-zero capacity is 8.
pub const MIN_BYTES_CAP: usize = 8;
/// Smallest capacity of a vector of elements up to 1 KiB, the growth floor for
/// every message vector this crate decodes.
pub const MIN_VEC_CAP: usize = 4;
/// Upper bound on one `InstallOperation` struct.
pub const OP_COST: usize = 192;
/// Upper bound on one `Extent`.
pub const EXTENT_COST: usize = 32;
/// Upper bound on one `PartitionUpdate` struct.
pub const PARTITION_COST: usize = 512;
/// Upper bound on the top-level `DeltaArchiveManifest` struct.
pub const MANIFEST_COST: usize = 256;
/// Upper bound on the top-level `Signatures` struct.
pub const SIGNATURES_COST: usize = 32;
/// Upper bound on one `Signatures.Signature` entry.
pub const SIGNATURE_COST: usize = 48;
/// Longest string field the middleware will store (names, versions, paths).
pub const MAX_STRING_BYTES: usize = 256;
/// Bytes of one interval in the plan's sorted workspaces: a start and an end, each a
/// `u64`. The contract derivation holds one per destination extent, and the copy check
/// holds one per extent of one operation. Both are bounded by the extents the scanner
/// counts (`plan::validate`, `docs/memory.md` §2.5).
pub const SPAN_COST: usize = 16;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ResourceProfile {
    /// Encoded `DeltaArchiveManifest` bytes, read before any decoding.
    pub max_encoded_manifest: usize,
    /// Encoded `Signatures` message (metadata or payload signature block).
    pub max_signature_message: usize,
    /// Budget for the modeled decode peak of one decoded message.
    pub max_decoded_bytes: usize,
    /// Operations across the manifest.
    pub max_operations: usize,
    /// Destination plus source extents in one operation.
    pub max_extents_per_op: usize,
    /// Extents across all operations.
    pub max_total_extents: usize,
    /// Partitions in the manifest.
    pub max_partitions: usize,
    /// Entries in one `Signatures` message.
    pub max_signatures: usize,
    /// Bytes of one signature's `data` field.
    pub max_signature_data: usize,
    /// Largest block size the applier accepts.
    pub max_block_size: u32,
    /// Packed frame table of one `REPLACE_FRAMES` or `REPLACE_SPARSE` operation, in bytes.
    pub max_frame_table_bytes: usize,
    /// Packed block map of one `REPLACE_SPARSE` operation, in bytes. Four blocks
    /// per byte, so this bounds the blocks one sparse operation may span.
    pub max_map_bytes: usize,
}

/// The embedded baseline, sized against a device of about 100 KiB of RAM. The
/// modeled phase peaks in `docs/memory.md` §4 (decoded manifest, signature
/// decoding, RSA workspace, blob buffers) are set against that figure, with
/// headroom for the stack. The fit is modeled and host-measured, not demonstrated
/// on a device. Every value is a limit, not a target: the generator coalesces to
/// meet it.
///
/// The count limits are not all admissible together. The budget binds first: a
/// manifest with the maximum operation count, two extents per operation and both
/// hashes costs more than 32 KiB in the model, so the generator must coalesce it.
/// The exact boundary is measured in `ota-client/tests/peak_memory.rs`.
pub const EMBEDDED: ResourceProfile = ResourceProfile {
    max_encoded_manifest: 16 * 1024,
    max_signature_message: 2 * 1024,
    max_decoded_bytes: 32 * 1024,
    max_operations: 64,
    max_extents_per_op: 256,
    max_total_extents: 128,
    max_partitions: 1,
    max_signatures: 4,
    max_signature_data: 512,
    max_block_size: 4096,
    max_frame_table_bytes: 8 * 1024,
    max_map_bytes: 1024,
};

/// Host override for desktop tooling. Matches the historical 64 MiB manifest
/// cap and accepts any block size that fits `u32`.
pub const HOST: ResourceProfile = ResourceProfile {
    max_encoded_manifest: 64 * 1024 * 1024,
    max_signature_message: 1 << 20,
    max_decoded_bytes: 1 << 30,
    max_operations: 1 << 20,
    max_extents_per_op: 1 << 20,
    max_total_extents: 1 << 24,
    max_partitions: 4,
    max_signatures: 8,
    max_signature_data: 1 << 16,
    max_block_size: u32::MAX,
    max_frame_table_bytes: 1 << 24,
    max_map_bytes: 1 << 20,
};

/// Element slots a `Vec` of `n` elements occupies once grown by `push`: the
/// capacity doubles from [`MIN_VEC_CAP`], so the allocation can be larger than
/// `n`. Saturates rather than wrapping for counts no profile can reach.
pub fn vec_slots(n: usize) -> usize {
    if n == 0 {
        return 0;
    }
    let pow2 = n.checked_next_power_of_two().unwrap_or(usize::MAX);
    return core::cmp::max(MIN_VEC_CAP, pow2);
}

/// What a manifest or signature message will hold once decoded, computed from
/// the wire bytes before anything is allocated.
///
/// The peak is `retained + transient`. Retained is what the finished message
/// holds, and it only grows during decoding. Transient is the largest single
/// temporary that is live alongside it. Decoding is sequential, so the peak
/// never has two temporaries live at once, and the largest one bounds the
/// transient part. Summing them would be valid but needlessly strict.
///
/// This relies on each singular byte, string and message field being decoded
/// once. A repeated one would make prost replace or merge into the buffer it
/// already holds, which the model does not charge, so the scanner refuses such
/// input (see `prescan`).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Usage {
    /// Element storage: each vector's capacity in slots, times the element bound.
    pub slot_bytes: usize,
    /// Heap held by the finished message: byte and string fields (at their
    /// minimum capacity), plus one [`ALLOC_OVERHEAD`] per vector buffer.
    pub heap_bytes: usize,
    /// The largest temporary live during decoding: a vector's old buffer while it
    /// grows, or a byte field's copy while prost fills it.
    pub transient_bytes: usize,
}

impl Usage {
    /// A message whose top-level struct costs `base` bytes, with nothing else
    /// charged yet.
    pub fn with_base(base: usize) -> Usage {
        return Usage {
            slot_bytes: base,
            heap_bytes: 0,
            transient_bytes: 0,
        };
    }

    /// Charge one vector that ends with `n` elements of at most `elem` bytes.
    pub fn vector(&mut self, n: usize, elem: usize) {
        let slots = vec_slots(n);
        if slots == 0 {
            return;
        }
        self.slot_bytes = self.slot_bytes.saturating_add(slots.saturating_mul(elem));
        self.heap_bytes = self.heap_bytes.saturating_add(ALLOC_OVERHEAD);
        if slots > MIN_VEC_CAP {
            let old = (slots / 2)
                .saturating_mul(elem)
                .saturating_add(ALLOC_OVERHEAD);
            self.transient(old);
        }
    }

    /// Charge one temporary vector of `n` elements of `elem` bytes, allocated at exactly
    /// that size. It is live beside the finished message, but only after decoding, so it
    /// is a transient: it raises the peak only if it is the largest one. Empty vectors
    /// allocate nothing.
    pub fn temporary(&mut self, n: usize, elem: usize) {
        if n == 0 {
            return;
        }
        let bytes = n.saturating_mul(elem).saturating_add(ALLOC_OVERHEAD);
        self.transient(bytes);
    }

    /// Charge one byte field of `len` bytes. Empty fields allocate nothing.
    ///
    /// prost decodes a `bytes` or `string` field by copying it through a
    /// temporary of the same length, and only then into the field's own buffer.
    /// The temporary is live while that buffer is allocated, so it is charged
    /// as a transient.
    pub fn bytes(&mut self, len: usize) {
        if len == 0 {
            return;
        }
        let cap = core::cmp::max(len, MIN_BYTES_CAP);
        let block = cap.saturating_add(ALLOC_OVERHEAD);
        self.heap_bytes = self.heap_bytes.saturating_add(block);
        self.transient(block);
    }

    fn transient(&mut self, bytes: usize) {
        self.transient_bytes = core::cmp::max(self.transient_bytes, bytes);
    }

    /// The modeled decode peak, saturating so hostile counts can never wrap
    /// into a small number.
    pub fn accounted(&self) -> usize {
        return self
            .slot_bytes
            .saturating_add(self.heap_bytes)
            .saturating_add(self.transient_bytes);
    }
}
