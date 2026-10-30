# Project operation format (revision 17)

This document specifies the operations this project adds on top of the
REPLACE/ZERO baseline, and the byte-range model they all share. The outer
framing is the reference's: the `CrAU` header, the manifest and signature
messages, the blob layout and the signed ranges (`docs/verify.md`) are unchanged.
What changed in revision 17 is what an extent means and how operations execute.

**These operations are not upstream-compatible.** A reference client refuses such
a payload before it writes anything, for two independent reasons given in §2.

## 0. Summary of the model

- An extent is a **byte range**: a start offset and a length in the partition.
- Operations execute in **declared order**. Their destinations may lie anywhere,
  in any order, and may overlap other operations. **Later writes win.**
- An operation's destination bytes form a **logical stream**: its extents
  concatenated in declared order. The stream is cut into **chunks** of the
  manifest's `block_size`. The last chunk may be shorter.
- Every operation that carries bytes is authenticated by a **data hash** of its
  blob. A copy is authenticated by a **source hash** of the bytes it reads.
- An operation is the unit of **commit**. The client records progress after each
  one, and a resume replays only the operation that was not committed
  (`docs/checkpoint.md`).

## 1. Operations

| Type | Name | Blob | Extents | Origin |
| --- | --- | --- | --- | --- |
| 0 | `REPLACE` | the destination's logical stream, raw | destination | reference |
| 6 | `ZERO` | none | destination | reference |
| 4 | `SOURCE_COPY` | none | source (old image), destination | reference value, project semantics (§5.1) |
| 256 | `REPLACE_FRAMES` | one frame per chunk: raw or zlib | destination; `frame_table` (100) | project |
| 257 | `REPLACE_SPARSE` | raw or zlib frames for the chunks that are neither kept nor zero | destination; `frame_table` (100), the chunk map and the frame table | project |
| 258 | `TARGET_COPY` | none | source (the target), destination | project (§5.2) |

`SOURCE_COPY` keeps the reference's value because its meaning matches: it copies
from one partition's bytes into the destination, carries no blob, and is
authenticated by `src_sha256_hash`. Its source is the immutable old image, not
the target (§5.1). `TARGET_COPY` is the one operation whose source is the target.

Types 256 to 258 lie above every value the reference assigns (its
`InstallOperation.Type` enum stops at 13), so a reference client that does not
know them reports an unknown operation.

## 2. Revision

The manifest's `minor_version` (field 12) is the revision. Revision **17** is the
byte-range revision, and it is the only one this project applies.

| Client | Behaviour on a payload with minor 17 | Behaviour on any other minor |
| --- | --- | --- |
| This project, revision 17 | accepts, subject to §9 | refuses with `unsupported_minor_version` before any write |
| This project, earlier revisions | refuses with `unsupported_minor_version` before any write | — |
| Reference update_engine | refuses with `kUnsupportedMinorPayloadVersion` (its maximum is 9) before any write | — |

Revision 16 and earlier gave extent fields the meaning of block counts. Under
revision 17 the same tags carry byte offsets and lengths, so an old payload is
refused before any of its extents is used. The constant is
`ota_payload::layout::PROJECT_MINOR`. Breaking changes to the wire are allowed by
the project's rules, so the refusal is the compatibility mechanism.

## 3. Byte ranges, the logical stream and chunks

### 3.1 Extents

An extent is `{start_byte, num_bytes}` (`Extent` fields 1 and 2). Both are
unsigned 64-bit values, and **both must be present on the wire**. An absent field
would read as zero, so an extent that omits one is refused with
`required_field_missing`, before any write. A zero start is written out like any
other value, and it is valid.

This is a compatibility change. Earlier builds of revision 17 read an absent field
as zero and accepted such payloads. A producer that leaves zero-valued fields out,
as a proto3 writer does, must now write `start_byte` explicitly, or its payloads
are refused.

- `num_bytes` is non-zero, and `start_byte + num_bytes` does not overflow.
- An operation's extents are non-empty, and their total length does not overflow.
- **Order is free.** Extents may be listed in any order.
- **Overlap is free**, both between extents of one operation and between
  operations. Later writes win (§6).
- The `dst_length` field, when present, equals the destination's total length.

The target's operation **reach** is the largest `start_byte + num_bytes` of any
destination extent or `TARGET_COPY` source. These ranges must lie inside the
recorded **new** image length, when present. `SOURCE_COPY` sources instead lie
inside the independently recorded **old** length; they do not add to target reach.
Old and new lengths may differ, and `Geometry.size` is the optional new length.

At apply, device capacity must be at least the new length and target reach.
A separate `OldImage` must match the recorded old length when present. No operation
resizes, pads or truncates storage: bytes after the new image remain unchanged.
Each recorded size is any byte count, not necessarily a multiple of the chunk size.
See `docs/image-geometry-and-cli.md` for examples and hash boundaries.

### 3.2 The logical stream

The destination bytes of an operation, in declared extent order, are the
**logical stream** of length `L`. Byte `p` of the stream is written to the
`p`-th destination byte. A copy's source is a logical stream the same way, and
its length equals the destination's.

### 3.3 Chunks

The manifest's `block_size` (default 4096) is the **codec chunk size**. It is not
a storage geometry. A payload cuts each operation's logical stream into chunks:

```text
chunk i covers logical bytes [i · c, min((i + 1) · c, L))      for i in 0 .. ceil(L / c)
```

Every chunk is `c` bytes except the last, which is `L − (ceil(L/c) − 1) · c`
bytes and is the **partial final chunk**. A chunk's bytes are written to every
destination piece they cover, in order. Storage is addressed in bytes, so a
chunk need not align with anything the device uses.

Frames and maps describe chunks, so their length depends on `L` and `c`, and
their stored lengths are checked against the chunk each one describes (§4).

## 4. Blobs: raw, zlib, zero and keep

### 4.1 Frames

A **frame** is the stored form of one chunk.

- **Raw.** The chunk's bytes, verbatim. For a partial final chunk, its own length.
- **Zlib.** One complete zlib stream (RFC 1950) that wraps one DEFLATE stream
  (RFC 1951), produced from exactly one chunk by a fresh compressor. The stored
  length `n` satisfies `1 ≤ n < chunk_len`. A chunk is zlib when that is
  strictly smaller than raw, and raw otherwise, so framing can never make a
  payload larger.

"Zlib" is the encoded format, and miniz_oxide is the implementation. A different
zlib implementation can produce the same format.

#### 4.1.1 Decoder rules

A zlib frame is accepted only if all of these hold. Each failure is
`frame_malformed`:

1. The first two bytes are a valid zlib header: method 8, window size at most
   32 KiB (`CINFO ≤ 7`), no preset dictionary (`FDICT` clear), and
   `(CMF · 256 + FLG) mod 31 = 0`.
2. The DEFLATE data decodes without error, and the decoder reaches the end of
   the stream within the stored bytes.
3. The Adler-32 trailer matches the decoded data.
4. The stream ends exactly at the last stored byte. Any byte after it is an error.
5. The stream decodes to exactly its chunk's length. Longer output and shorter
   output are both errors.

A preset dictionary is refused, because a dictionary would be a history shared
across frames, and frames must be independent.

Rules 2 and 3 detect corruption. They are not authentication (§8).

The decoder writes into the caller's chunk buffer, which is the history
(`TINFL_FLAG_USING_NON_WRAPPING_OUTPUT_BUF`), so no 32 KiB ring is allocated. Its
state is about 10.5 KiB and is created on the first zlib frame of a replay
(`docs/memory.md`).

### 4.2 `REPLACE_FRAMES` (type 256)

- Its destination is one or more extents. No source extents.
- `frame_table` (field 100) holds one canonical unsigned LEB128 varint **per
  chunk**, in chunk order:
  - `0` is a raw chunk: its `chunk_len` bytes are in the blob.
  - `n > 0` is a zlib frame of `n` bytes in the blob, with `n < chunk_len`.
- The table has exactly `ceil(L / c)` entries.
- The blob length is the sum over chunks of `chunk_len` for each zero entry and
  `n` for each other entry. It must equal `data_length`.
- Canonical varints: a value uses the fewest bytes, so `0x80 0x00` is refused. A
  value past 64 bits, and a varint with no terminating byte, are refused.
- The operation carries `data_sha256_hash` (§8).

### 4.3 `REPLACE_SPARSE` (type 257)

Sparse replacement covers a destination in which some chunks keep their current
content. Those chunks cost no blob bytes.

- Its destination is one or more extents. No source extents.
- The `frame_table` field (100) carries two sections, back to back:
  1. **The chunk map**: exactly `ceil(chunks / 4)` bytes, two bits per chunk,
     packed LSB-first. Chunk `i` is at byte `i / 4`, bits `2·(i mod 4)` and
     `2·(i mod 4) + 1`.
  2. **The frame table**: one canonical varint per **zlib** chunk, in chunk order.

| Mode | Value | Meaning | Blob bytes |
| --- | --- | --- | --- |
| Keep | `00` | The destination already holds this chunk's content. Nothing is written. | 0 |
| Zero | `01` | The chunk is written as zeros. | 0 |
| Raw | `10` | The next `chunk_len` blob bytes are the chunk. | `chunk_len` |
| Zlib | `11` | The next blob bytes are a zlib frame whose length is the next table entry. | that entry |

- The map is canonical: unused bits in its final byte are zero.
- The table has exactly one entry per `11` chunk, and each entry lies in `1 … chunk_len − 1`.
- The blob length is the sum of the blob bytes above, and it must equal `data_length`.
- The operation carries `data_sha256_hash`. An operation whose blob is empty,
  because every chunk is kept or zero, carries the SHA-256 of the empty input
  (`e3b0c442 98fc1c14 9afbf4c8 996fb924 27ae41e4 649b934c a495991b 7852b855`).

Keep is a claim about the target's current content. The generator emits it only
where the target model already holds the new bytes at that point, including bytes
an earlier operation wrote. Growth beyond the old length is unknown, even if the
new bytes are zero: it must be explicitly written before it can be kept or read by
`TARGET_COPY`. The client performs no per-keep read/compare. With no separate old
image, a fresh readable target's pre-flight checks the recorded old-length prefix,
not the whole device. Supplying a separate image validates that image, not the
target's kept bytes; it never initializes them. Post-flight checks the new-length
prefix on readable targets (`docs/client.md` §2.5).

The field is one bytes field, not two, so the operation's decoded size does not
grow (`docs/memory.md` §2). The map and the table are each limited on their own
(§10).

### 4.3.1 Worked example

Six chunks, with modes keep, zero, raw, zlib (100 bytes), keep and zlib (7 bytes),
in a `REPLACE_SPARSE` over `L = 6 · 4096` bytes with `block_size` 4096:

```text
chunk   0    1     2     3       4     5
mode    Keep Zero  Raw   Zlib    Keep  Zlib
bits    00   01    10    11      00    11

map byte 0 (chunks 0-3): 00 | 01<<2 | 10<<4 | 11<<6  = 0xe4
map byte 1 (chunks 4-5): 00 | 11<<2                  = 0x0c   (bits 4-7 unused, clear)
table:                   varint(100) = 0x64, varint(7) = 0x07

frame_table (field 100) = e4 0c 64 07               (4 bytes: 2 map + 2 table)
blob                    = 4096 raw bytes | 100 bytes | 7 bytes   (4203 total)
data_length             = 4203
```

Nothing is written for chunk 0 or chunk 4. Chunk 1 is written as zeros, and chunk
2 is the next 4096 blob bytes. Chunks 3 and 5 are each decoded from their frame,
in order.

### 4.3.2 Partial final chunks

With `L = 2 · 4096 + 300`, the operation has three chunks, and the third holds 300
bytes. A raw third chunk costs 300 blob bytes. A zlib third chunk must store
fewer than 300 bytes, so a 300-byte chunk that does not compress is stored raw.
Its table entry, if any, is at most 299.

### 4.4 Frame table example

`REPLACE_FRAMES` over four chunks of `block_size` 4096: raw, a 1200-byte frame, raw,
and a 37-byte frame.

```text
entries   0      1200        0      37
varint    00     b0 09       00     25
frame_table = 00 b0 09 00 25         (5 bytes)
blob        = 4096 | 1200 | 4096 | 37     (9429 total)
```

### 4.5 `REPLACE` (type 0)

Its blob is the destination's logical stream, raw, so `data_length` equals the
destination's total length. It carries `data_sha256_hash`. In revision 17 a
`REPLACE` without its hash is refused: a data-bearing operation is always
authenticated.

### 4.6 `ZERO` (type 6)

It carries no blob, and `data_length` is 0. Its hash is optional: absent, or
present and the empty-input digest.

## 5. Copies

Both copy operations take a source logical stream and a destination logical
stream of equal length. Neither carries a blob, a data hash or a `data_length`.
Each carries `src_sha256_hash`, the SHA-256 of its source's logical stream, and
the client refuses a copy whose source does not match it.

### 5.1 `SOURCE_COPY` (type 4): from the old image

- The source is the **immutable old image**, which is a separate capability from
  the target (`ota_client::image::OldImage`). A copy never reads the target.
- `src_length`, when present, equals the total length.
- The source must lie inside the old image's capacity. When the manifest records
  the old partition's size, the image must have exactly that size.
- Several copies may read the same source. The client validates every copy's
  source hash **before its first destination write** (`docs/client.md` §2.5).

### 5.2 `TARGET_COPY` (type 258): within the target

`TARGET_COPY` copies bytes the target holds into other places in the target.

- **It needs a readable target.** A write-only sink refuses the plan with
  `target_not_readable`, before any write.
- **Its source must not intersect its destination.** The check covers the whole
  range sets: every source extent against every destination extent of the same
  operation. An intersection is refused at prepare with `copy_overlap`. This
  rejects a copy onto its own source, and a cycle between extent pairs of one
  operation. The check sorts each list by start and merges the two in one pass, so
  its cost grows with `n log n` in the operation's extent count, not with the
  product of the two counts. `docs/client.md` §3.2 gives the sweep.
- **Its source is verified before its first write.** The client reads the source
  through the target, hashes it, and refuses the copy with `source_mismatch` if the
  hash differs. The copy then reads and writes chunk by chunk. Its source is
  disjoint from its destination, so the copy reads the bytes that were hashed.
- **It may read what an earlier operation wrote.** Operations execute in order, so
  a copy reads the target as the operations before it left it. A copy of data
  written by an earlier operation is legal and common.
- **Its source counts toward the device's reach.** It must fit the new image
  length when recorded, and the target capacity. It is not bounded by the old length.

## 6. Execution: order, overlap and replay

### 6.1 Order and last write wins

Operations execute in manifest order. Within an operation, each extent is written
in declared order, and each chunk is written to its pieces in logical order. So
for any byte of the target, the value after the update is the value written by
the **last** write that covered it: the last operation that covered it, and
within that operation the last extent that covered it, and within that extent
the last chunk that covered it. A kept chunk writes nothing, so it does not
overwrite.

### 6.2 Replay safety

An interrupted operation is executed again from its first chunk, and every
operation must give the same result when it is replayed. Revision 17 guarantees
that, for these reasons:

| Operation | Why a replay is safe |
| --- | --- |
| `REPLACE`, `ZERO`, `REPLACE_FRAMES`, `REPLACE_SPARSE` | The destination bytes are determined by the blob, which is authenticated. Nothing in the target is read. |
| `SOURCE_COPY` | The source is the old image, which is immutable for the whole update (§8). |
| `TARGET_COPY` | Its source is disjoint from its own destination (§5.2). The partial writes of the interrupted run do not touch the source, and no later operation has run yet. |

Earlier operations are committed and are not replayed, so a copy never reads
bytes that a later operation of the same run has already written. A
`TARGET_COPY` may read what an earlier operation wrote, because that operation's
writes are durable before the copy starts.

## 7. Authentication, and when each byte is checked

| Bytes | Authenticated by | When it is checked |
| --- | --- | --- |
| Manifest: logical partition name, image lengths/hashes, extents, `frame_table`, every `data_sha256_hash` and `src_sha256_hash` | metadata signature (`verify_message`) | before the manifest is decoded |
| Blob bytes, including every zlib frame | payload signature, and each operation's `data_sha256_hash` | buffered: every hash in `prepare`, before any write. Streamed: each hash after its operation's writes and before its checkpoint; the payload signature at end of stream |
| Separate old image contents | recorded old size, whole-image hash (when recorded), and each `SOURCE_COPY` source hash | before any destination write whenever an image is supplied, even without copies |
| Target image prefixes | recorded old/new hashes | old prefix: fresh readable run without a separate image; new prefix: post-flight on a readable target, including completed reruns |
| Target contents read by `TARGET_COPY` | that copy's `src_sha256_hash` | before that copy's first write |
| Zlib header, DEFLATE structure, Adler-32 | nothing: corruption checks | when the frame is decoded |

Two consequences follow:

- The zlib checksum does not authenticate a frame. The operation's data hash
  does, and the signed manifest carries it.
- A frame that does not decode is refused at its chunk, and the chunks before it
  in the same operation stay written. A frame whose bytes match their hash and
  still fail to decode are the generator's fault, which its self-check rules out.

## 8. Platform contract

A generic trait cannot prove that two storage handles never overlap, that a
source is unchanged, or that a write is confined to its range. The contract is
therefore stated here.

- **The old image is immutable** for the whole update, including every resumed
  attempt. Each source read returns the bytes the old-image hash describes.
- **Partition names are logical labels**, nonempty UTF-8 in metadata, not paths or
  automatic A/B selectors. Adapters may inspect `Prepared::partition_name()` before
  opening storage and apply their own allowlist/routing policy. Only
  `Trust::Verified` authenticates that label; unverified preparation does not.
  The engine still applies exactly one partition, regardless of resource caps.
- **The target is exclusive** during the apply and during every recovery. No
  other writer may touch it, and the target must not be the old image. `ota-cli`
  refuses a source path that resolves to the device's own file. That check does
  not see two partitions that share one disk.
- **A failed write** may leave part of its requested range modified. It must not
  modify any byte outside that range. The client never relies on a failed write
  being atomic, since a rerun repairs the range.
- **A flush is a persistence barrier.** After it returns, every write made before
  it survives power loss (`docs/client.md` §2.1).
- **The checkpoint store** is atomic and durable, and writable only by the
  updater (`docs/checkpoint.md` §5).

## 9. Validation summary

The plan refuses each of these before any write. Codes are in
`ota_payload::error::Code`.

| Condition | Code |
| --- | --- |
| Minor version other than 17 | `unsupported_minor_version` |
| Operation type outside the six above | `unknown_operation` |
| An extent that omits `start_byte` or `num_bytes` (a written zero start is present, and valid) | `required_field_missing`, at the wire scan or in the plan |
| An extent of zero length, or an overflow, or an empty extent list | `extent_out_of_range` |
| A destination or `TARGET_COPY` source past the new length, or a `SOURCE_COPY` source past the old length (when recorded) | `extent_out_of_range` |
| Source extents on `REPLACE`, `ZERO` or `REPLACE_FRAMES`/`REPLACE_SPARSE` | `unsupported_field` |
| A frame table on an operation that does not use one | `unsupported_field` |
| A data hash missing or empty on `REPLACE`, `REPLACE_FRAMES`, `REPLACE_SPARSE` | `required_field_missing` |
| A copy's source hash missing, or a copy with a data hash or a blob | `required_field_missing`, `unsupported_field`, `blob_mismatch` |
| Source and destination logical lengths differ | `blob_mismatch` |
| An in-target copy whose source intersects its destination | `copy_overlap` |
| A `REPLACE_SPARSE` with an empty blob whose hash is not the empty-input digest | `blob_mismatch` |
| A packed component over its limit (§10), in the scan or in the plan | `resource_limit` |
| Frame table with the wrong entry count, a non-canonical varint, or a stored length outside `1 … chunk_len − 1` | `frame_table_invalid` |
| Chunk map with the wrong length or unused bits set | `block_map_invalid` |
| Blob length that disagrees with the table, the map or `data_length` | `blob_mismatch` |
| Operation blobs not contiguous, or `signatures_offset` not the blob total | `extent_out_of_range`, `overflow` |
| Empty logical partition name | `required_field_missing` |
| Chunk size of zero or above the profile | `unsupported_block_size` |
| Device capacity below the recorded new size, or too small for target reach | `partition_mismatch`, `extent_out_of_range` (at bind) |
| Old image absent for a copying plan | `source_missing`, before any write |
| Old image capacity differs from the recorded size | `source_geometry`, before any write |
| Old image hash, or a copy's source hash, does not match | `source_mismatch`, before any write |
| `TARGET_COPY` with no readable target | `target_not_readable`, before any write |
| Target's source hash for an in-target copy does not match | `source_mismatch`, before that copy's write |
| A zlib frame that fails §4.1.1 | `frame_malformed` (at replay) |
| A blob whose hash does not match | `blob_mismatch` (at prepare when buffered, at its operation when streamed) |
| A checkpoint that is damaged, of an unknown phase, or inconsistent | `checkpoint_invalid` |
| A checkpoint of another payload or target | `checkpoint_mismatch` |

## 10. Limits

The packed `frame_table` field has two components, and each has its own limit.
The chunk count is `ceil(L / c)`, so the limits are the same as before, measured
in chunks:

| Component | Limit | Embedded value | Basis |
| --- | --- | --- | --- |
| Chunk map of a `REPLACE_SPARSE`, `ceil(chunks / 4)` bytes | `max_map_bytes` | 1,024 | 4,096 chunks |
| Frame table: the whole field of `REPLACE_FRAMES`, or the field after the map of `REPLACE_SPARSE` | `max_frame_table_bytes` | 8,192 | two bytes per zlib chunk at 4 KiB chunks |
| The field as a whole | their sum, 9,216 | an early bound | checked in the scan, before the field is charged |

The scanner cannot know the chunk count, because the manifest's `block_size` may
follow the operations on the wire. So it bounds the field as a whole, and the plan
checks each component once `block_size` and the operation's length are known.
Neither component can be spent on the other.

The decoded budget is unchanged at 32 KiB (`docs/memory.md`).

## 11. Not in this format

- Deduplication of blob literals across operations. Each frame is independent.
- Global payload dictionaries and back-references between frames.
- Copies from a source the old image or the target does not hold.
- Reference operations other than the six in §1: BSDIFF, MOVE, DISCARD, the XZ
  and BZ variants, and the verity and FEC structures.
- Transport of the payload. Hosts supply the bytes (`docs/client.md` §0).

## 12. Full-image payloads

A **full-image payload** writes a whole partition image without an old image. It is a
revision-17 payload with the conventions below. It adds no operation type and no field,
and the client applies it under the rules of §9 unchanged. The design is in
`docs/full-image.md`.

| Rule | Full-image payload | Refused by |
| --- | --- | --- |
| Old partition | `old_partition_info` is absent | the full checker: `unsupported_field` |
| New partition | the size and SHA-256 of the image, both recorded | the full checker: `required_field_missing`, `partition_mismatch`, `hash_mismatch` |
| Operation types | `REPLACE`, `REPLACE_FRAMES` and `ZERO` only | the full checker: `unsupported_field` |
| Coverage | the destinations, in operation order and then declared extent order, form one run from `0` to the image length. A gap, an overlap, or a write out of order is refused. | the full checker: `partition_mismatch` for a gap or a short cover, `extent_overlap` for an overlap |
| Replay | the image, from zeros, from erased flash (`0xff`), and from a fixed pattern | the full checker: `blob_mismatch` |

The coverage rule is what proves every byte is written. A replay from zeros alone
would also match a payload that never writes the zero bytes of the image.

The client needs no old image for this form. `needs_source` is false, so `apply` never
asks for one, and no pre-flight hash runs, because the manifest records no old hash.
Device capacity must be at least the new size as in §9; the entire new prefix is
written and any remaining tail is untouched. Post-flight hashes only the new prefix
on a readable device. A write-only sink cannot check it.

Framing, frame tables and `ZERO` follow §4.1, §4.2 and §4.6 exactly. A full-image
payload carries no copy and no `REPLACE_SPARSE`, because a kept chunk depends on
known destination contents, not merely on supplying a separate old image.

## 13. Release version (project extension)

`DeltaArchiveManifest` field 100, `release_version` (`uint64`), is a project extension
that the reference schema does not define. It is inside the manifest, so both signatures
cover it. The signer sets it as a monotonic counter. The client refuses a payload that
lacks it under the strict policy, and refuses a value below the host's floor
(`docs/platform.md` §3). The scanner refuses a repeated occurrence, so the value is
unambiguous. A payload without the field still decodes under the development policy.
