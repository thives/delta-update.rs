# Protobuf wire format via `prost-build` codegen

## Purpose and scope

All protobuf messages in the payload (`DeltaArchiveManifest`, `InstallOperation`,
`PartitionUpdate`, `Extent`, `PartitionInfo`, `Signatures` and the nested
`Signature`) are **generated at build time** by
[`prost-build`](https://crates.dev/crate/prost-build) from the crate's
**vendored** copy of the golden schema, `delta-update-payload/schema/update_metadata.proto`.
The hand-rolled wire codec (this crate's former `proto.rs` with its
`Sink`/`Reader` types) is deleted; field numbers, defaults, `reserved` ranges and
the proto2 wire rules can no longer drift from the schema because they are
*read* from it on every build. Per project rule, `update_engine/` is only a
reference: no build script, source file or test in our workspace reads from it.

This document records the design decisions behind that migration.

## Why `prost` (and not gRPC-style codegen or a hand-rolled codec)

| Candidate | Verdict | Reason |
| --- | --- | --- |
| `prost` + `prost-build` | **Chosen** | Build-time codegen from the schema file itself (vendored copy, see below); `no_std`-compatible (`default-features = false`, allocator via `alloc`); proto2 presence semantics (0.14.x maps `optional` to `Option<T>`, including `bytes`); the only proto dependency needed. |
| `protolink`-style gRPC stack | Rejected | The project has no protobuf service definitions — payload format only. gRPC transport is out of scope by project rules, so the service/runtime half of that dependency would be dead weight. |
| Hand-rolled wire codec (previous state) | Replaced | Every field number, tag and wire-type constant was maintained by hand next to (not derived from) the schema; adding or renaming a field required auditing the codec. |

`prost-build` shells out to `protoc` (`protobuf-compiler`, already installed
by the devcontainer image; `PROTOC` env var also honored).

## Codegen pipeline

```mermaid
flowchart TD
    Golden[update_engine/update_metadata.proto — golden reference, never read by our code] -.->|manual refresh + diff review| Vendored[delta-update-payload/schema/update_metadata.proto — vendored schema]
    Vendored --> BuildRS[delta-update-payload/build.rs]
    BuildRS -->|prost_build::compile_protos| Protoc[protoc --descriptor_set_out]
    Protoc -->|prost codegen| Gen["$OUT_DIR/chromeos_update_engine.rs — named after the proto package"]
    Gen -->|cargo:rustc-env OTA_SCHEMA_FILE| Env["OTA_SCHEMA_FILE=<abs path>"]
    Env -->|include!(env!(OTA_SCHEMA_FILE))| Schema["crate::schema — messages at top level, nested types in snake_case submodules"]
    Schema -->|pub use with short names| Manifest["crate::manifest: Manifest, Operation, PartInfo, Extent, PartitionUpdate, Signatures, Signature"]
    Manifest --> Validate["Semantic validation: decode_manifest / decode_signatures"]
    Validate --> Consumers["delta-update-client, delta-update-generator, delta-update-cli build/consume messages directly"]
```

Two mechanical details worth recording:

- `prost-build` 0.14 requires the two-argument form
  `compile_protos(&[protos], &[includes])`; the generated file is named after
  the **proto package** (`chromeos_update_engine`), and its messages live at
  the top level of that file — the proto package becomes the file name, not a
  Rust module. So the include in `src/lib.rs` targets
  `chromeos_update_engine.rs` and re-exports live directly under `crate::schema`.
- The build script exports the generated file's absolute path through
  `cargo:rustc-env=OTA_SCHEMA_FILE=...` and `lib.rs` includes it with
  `include!(env!("OTA_SCHEMA_FILE"))`. `cargo:rerun-if-changed` is set on the
  **vendored** `.proto` so codegen re-runs only when that copy changes.

## Schema vendoring and drift policy

The vendored `delta-update-payload/schema/update_metadata.proto` is a byte copy of
the golden schema. Keeping it current is a deliberate, manual step — the
alternative (a build script reading `../update_engine/`) would couple
builds to the reference tree's layout and change set, which the project
rules forbid ("update_engine is only a reference"). The refresh procedure is
documented in `build.rs` itself, next to the code it affects:

```sh
cp update_engine/update_metadata.proto delta-update-payload/schema/
git diff -- delta-update-payload/schema/update_metadata.proto
```

Reviewing that diff is where upstream schema changes become conscious
decisions: any changed field number or type flows through codegen into
`crate::schema`, fails to compile if consumers relied on a removed shape,
or — for wire-visible changes to `Signatures` — trips the golden byte-fixture
tests in `sign.rs`. Unreviewed drift between the vendored copy and the
reference cannot be caught mechanically by the build; that trade-off is the
cost of decoupling and is accepted by the rule.

## Project extensions to the schema

The vendored copy has two additions the reference does not have. Both are
documented in `docs/format.md` §1 and §4.

- `InstallOperation.frame_table` (field 100, `optional bytes`) holds the packed
  frame descriptions of a `REPLACE_FRAMES` operation, and the chunk map followed
  by the frame descriptions of a `REPLACE_SPARSE` operation. The field number is
  well above the reference's, and the reference's parser skips fields it does not
  know, so the field is inert to it.
- Operation types 256 (`REPLACE_FRAMES`), 257 (`REPLACE_SPARSE`) and 258
  (`TARGET_COPY`) are project values. The reference's `InstallOperation.Type` stops
  at 13.
- `Extent` fields 1 and 2 carry **byte** offsets and lengths in revision 17
  (`start_byte`, `num_bytes`). Both must be present, so an absent field is refused
  rather than read as zero (`docs/format.md` §3.1). The reference uses the same tags
  for block counts. The two meanings are separated by the manifest's
  `minor_version`, which the client reads before it interprets any extent
  (`docs/format.md` §2).

The drift policy above still applies, and a refresh must keep both additions.
They are marked in the schema with a comment, so a diff of a refreshed copy shows
them as removed rather than silently losing them. The scanner's seen-field record
covers field numbers below 128, so field 100 is recorded like any singular field
and a repeat is refused (`docs/memory.md` §2.3).

## Proto2 → Rust mapping (what the generated code looks like)

The golden schema is `syntax = "proto2"`. prost 0.14 models proto2 *presence*
faithfully:

| Schema construct | Generated Rust | Semantics |
| --- | --- | --- |
| `optional <scalar>` | `Option<T>` | Absent decodes to `None`; `Some(x)` always encodes, even for zero values. |
| `optional bytes` | `Option<Vec<u8>>` | Present-but-empty (`0x12 0x00`) round-trips — this is what the reference's setter-based encoding does. |
| `required <scalar/string>` | plain `T` | **Not validated at decode time**: a missing required field decodes to the zero value (e.g. empty `partition_name`). The validation layer in `manifest.rs` closes this gap (see below). |
| `repeated message` | `Vec<T>` | Non-packed per proto2 default; one tag+len entry per element, byte-compatible with upstream. |
| `enum` field | `i32` field + generated `enum` module (`install_operation::Type`) | Unknown enum values survive as raw ints — forward compatible. `type` is a Rust keyword, so the field is `r#type`. |
| `[default = 4096]` | (annotation only) | Consumers apply schema defaults explicitly: `block_size.unwrap_or(4096)` in the applier and `minor_version.unwrap_or(0)` in the layout parser, mirroring what the C++ accessor would return. |
| `deprecated = true` (`version` field) | Rust `#[deprecated]` | Still decodable; `sign.rs` never writes it, matching `ConvertSignaturesToProtobuf`. |
| Nested `Signatures.Signature` | `signatures::Signature` submodule | Nested types/messages get their own snake_case module; `enum Type` likewise (`install_operation::Type`). |
| `reserved` ranges (e.g. `DeltaArchiveManifest` 1,2,6..11) | compile-time enforcement | The old field numbers can never be (re)used by our code. |

## Why the validation layer still exists on top of prost

`prost` owns the **wire format**, not this implementation's resource policy.
`manifest::decode_manifest`/`decode_signatures` run the bounded scan in
`prescan` before the generated decoder, and add:

| Guard | prost provides | This crate adds |
| --- | --- | --- |
| Count and size caps (operations, extents, partitions, signatures, strings, encoded manifest, decoded memory) | none (bounded only by input size, and each element costs allocation) | `prescan` against the `ResourceProfile`, before any allocation. |
| Hash-length limits (empty or exactly 32 bytes) | none | `hash_length` on any other nonempty length. |
| Required-field presence (`partition_name`, `type`) | no decode-time check; a missing `type` decodes as `0` | `required_field_missing`, decided on the wire. |
| `has_sig` semantics (offset **and** size both present) | presence exposed via `Option` | `layout.rs` requires both, matching `payload_metadata.cc`. |
| Unsupported structures | decoded and kept | `unsupported_field`, rejected before decode. |
| Error taxonomy | `prost::DecodeError` | Mapped to `Code::bad_protobuf` with stable `PayloadError` codes. |

## Byte compatibility with upstream update_engine

- proto2 encodes known fields in ascending field-number order and repeated
  fields unpacked — prost follows the schema, and the golden reader accepts
  the result identically.
- The one place with golden *byte fixtures* (not just round-trip) is
  `sign.rs`: `matches_reference_wire_format` asserts the exact
  `ConvertSignaturesToProtobuf` bytes, including the explicitly present empty
  `data` field (`12 00`) for a zero-length reservation — the reason
  `data: Some(padded)` is never elided.
- `size_helpers_agree_with_generated_encoder` asserts the hand-written size
  arithmetic used to *pre-reserve* signature slots matches prost's own
  `encoded_len()` at every varint-length boundary (120/121, 16375/16376,
  ...). If a prost upgrade ever changes encoding, that test fails rather
  than silently producing unverifiable payloads — the header's
  `metadata_signature_size` and the manifest's `signatures_offset`/`signatures_size`
  are fixed *before* signing, so any drift breaks verification by design.

## Allocation trade-off

The previous codec was allocation-free (fixed-capacity views and `Reader`
cursors borrowing the payload). Generated messages own their `Vec`/`String`
storage, so manifest decode/encode allocates via `alloc`. The allocation is now
bounded: the pre-decode scan charges every allocation against the resource
profile, and `docs/memory.md` gives the model and the measured peak. The crate remains `#![no_std]` (`extern crate alloc`), and `prost`
is pulled in with `default-features = false`. This was accepted because:

- RSA key parsing and payload assembly already allocated on every path that
  consumes a manifest (the client crate and the generator are heap-using);
- fixed `MAX_*` caps keep the allocations bounded by hostile input;
- the applier's chunk I/O still uses a caller-owned scratch buffer — the
  hot path (writing chunks) is unchanged; only metadata handling allocates.

## Validation performed

- `cargo build --workspace` — clean, no warnings.
- `cargo test --workspace` — `delta-update-payload` 26 unit tests (including the
  golden wire-byte fixtures and malformed-suffix rejection suite: prost's
  decoder rejects truncated tags, field number 0, wire type 7, group tags
  without openers, oversized lengths, and field numbers ≥ 2²⁹, all mapped
  to `bad_protobuf`) and all 5 `delta-update-cli` end-to-end tests (generate →
  sign → verify → apply → post-flight hash) pass.
- `rustfmt --edition 2021 --check` on the touched files.

## Bounded pre-decode scan

Prost allocates while it decodes, so the decoder never sees an input whose size
or shape has not been approved. `delta_update_payload::prescan` walks the wire bytes first.
It allocates nothing and produces the decoded-memory `Usage` that the
resource profile checks (see `docs/memory.md`). Only then does prost build
the owned messages.

The scan follows the schema from `DeltaArchiveManifest` to `PartitionUpdate`,
then `InstallOperation`, then `Extent`: a fixed depth of four, so recursion is
bounded by construction. Unknown fields are skipped by their declared length.
Groups (wire types 3 and 4) are rejected, which is stricter than prost.

| Rule | Code |
| --- | --- |
| field number is in `1..=2^29-1`; varints are at most ten bytes | `bad_protobuf` |
| a singular bytes, string or message field appears at most once; repeated scalars and unknown fields are accepted (`docs/memory.md` §2.3) | `bad_protobuf` |
| a known field carries its declared wire type | `bad_protobuf` |
| encoded manifest ≤ profile | `manifest_too_big` |
| partitions, operations, extents (per operation and total), signature entries and signature data ≤ profile | `resource_limit` |
| modeled decode peak ≤ profile budget, for a manifest and for a signature message (`docs/memory.md`) | `resource_limit` |
| signature message ≤ profile, checked before any byte is scanned | `resource_limit` |
| `PartitionUpdate.partition_name` present and nonempty | `required_field_missing` |
| `InstallOperation.type` present (an explicit `0`, REPLACE, is valid) | `required_field_missing` |
| `Extent` `start_byte` and `num_bytes` both present (an explicit zero start is present, and valid) | `required_field_missing` |
| SHA-256 fields (`data_sha256_hash`, `src_sha256_hash`, partition `hash`) are empty or exactly 32 bytes | `hash_length` |
| strings ≤ 256 bytes | `resource_limit` |
| unsupported structures are rejected rather than decoded and ignored: `DeltaArchiveManifest` `dynamic_partition_metadata` and `apex_info`; `PartitionUpdate` `postinstall_path`, a true `run_postinstall`, `new_partition_signature`, `hash_tree_algorithm`, `hash_tree_salt`, hash-tree and FEC extents, `merge_operations` | `unsupported_field` |

The required-field rule needs the wire scan. A missing `type` decodes through
prost as `0`, which is `REPLACE`, so a presence check after decode would not
detect it. The scan is the only place that can tell "missing" from "zero". An
extent's two fields work the same way: a missing `start_byte` decodes as `0`, so
the scan refuses an extent that omits either field. The plan checks the same
fields too, for a manifest that is built without the scan, so both paths refuse
the same payloads (`docs/format.md` §3.1).
