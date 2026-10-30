# Signature-message encoding

## Purpose and scope

`ota-payload/src/sign.rs` implements the signature-serialization APIs:
`sig_entry_size`, `signature_message_size`, and `build_signature_message`.
The generator uses them to reserve metadata and payload signature slots
before signing, then fill those slots without changing the signed header
or manifest.

`Signatures` and the nested `Signature` are `prost-build`-generated
messages (see `docs/protobuf.md`); the encoder's job is to construct the
right message *value* (padding + size recording) and hand it to prost's
`encode_to_vec()`, plus to compute the slot size *before* the message
exists.

This is serialization, not cryptographic signing: callers provide signature
bytes already produced by `rsa_sign` or another signing implementation.
The crate is `no_std` + `alloc`; building the padded message now allocates
a `Vec<u8>` of the reserved size, which the previous sink-based encoder
avoided — an accepted trade-off (see `docs/protobuf.md`) since the
reservation is bounded by the RSA key size.

The read-only golden references are:

- `update_engine/payload_generator/payload_signer.cc`,
  `ConvertSignaturesToProtobuf`: omit the deprecated version field, append
  zero padding to the signature, and record its original size.
- `update_engine/update_metadata.proto`, `Signatures` and its nested
  `Signature`: field numbers and the `fixed32` wire type of
  `unpadded_signature_size`.

No files in the reference tree are changed.

## Wire format and size calculation

Let `K` be the reserved data size (`key_size`, **in bytes**), `N` be the
supplied signature length, and `V(x)` be the number of bytes needed for
protobuf varint encoding of `x`. The writer emits exactly one nested
signature:

```text
Signatures
+-- field 1: signatures (length-delimited)
    +-- field 2: data (length-delimited)
    |   +-- signature[0..N]
    |   +-- K-N zero bytes
    +-- field 3: unpadded_signature_size (fixed32 little-endian N)
```

| Component | Encoding | Byte count |
| --- | --- | --- |
| Data tag and length | `0x12`, varint `K` | `1 + V(K)` |
| Padded data | Original bytes followed by zeros | `K` |
| Unpadded size | `0x1d`, little-endian `u32` | `5` |
| Entry size `E` | Inner `Signature` message | `1 + V(K) + K + 5` |
| Message size `M` | `0x0a`, varint `E`, entry | `1 + V(E) + E` |

`sign.rs` keeps a tiny private `varint_size` for this arithmetic; it
must stay in sync with prost's encoder, and the
`size_helpers_agree_with_generated_encoder` test asserts, at every
varint-length boundary (120/121, 16375/16376, ...), that
`signature_message_size(K)` equals the `encoded_len()` of a message prost
actually built. Any future drift in encoding fails the test instead of
silently producing payloads whose pre-reserved slot sizes mismatch the
real signature messages.

| Reservation | Padded data bytes | Entry bytes | Message bytes |
| --- | ---: | ---: | ---: |
| RSA-2048 | 256 | 264 | 267 |
| RSA-4096 | 512 | 520 | 523 |

The fixed-width unpadded-size field makes `E` and `M` independent of `N`.
Using a varint for that field, writing only the unpadded data, or adding a
version field would invalidate the generator's precomputed slot sizes.

### Byte-level illustration

For `K = 3` and signature bytes `aa bb`:

```text
0a 0a | 12 03 | aa bb 00 | 1d 02 00 00 00
  |       |        |              |
  |       |        |              +-- fixed32 unpadded length = 2
  |       |        +----------------- signature and one padding byte
  |       +-------------------------- data tag and padded length = 3
  +---------------------------------- outer tag and entry length = 10
```

This exact encoding is asserted against the generated encoder in the
unit tests. `data` is always passed as `Some(padded)`, so both zero-length
fields remain explicitly present for an empty signature and zero-sized
reservation (`12 00`), matching the reference's setter-based encoding and
prost's proto2 presence semantics.

## Construction design and error handling

```mermaid
flowchart TD
    Input[reserved size K, signature bytes] --> Validate[Check N fits K and fixed32]
    Validate --> Sizes[checked_message_size: entry and total sizes fit usize]
    Sizes --> Pad[Fallibly reserve K bytes, zero-pad, copy N bytes]
    Pad --> Msg[Signatures { Signature { data: Some, unpadded: Some } }]
    Msg --> Encode[prost encode_to_vec]
    Encode --> Output[owned Vec<u8> slot bytes]
```

All validation precedes any allocation. A signature longer than its
reservation returns `signature_mismatch`; unrepresentable lengths or
encoded-size additions return `overflow` (checked with `checked_add`,
mirroring the reference's pre-signing size reservation, and bounding the
padded allocation before it happens). The arithmetic is `checked_message_size`, which the
generator's profile preflight calls as well, so the wire sizes have one checked definition. The
padded reservation is made with `try_reserve_exact`: an allocation the allocator cannot make
returns `resource_limit` rather than aborting the process.

Empty and short signatures are accepted because this function mirrors the
reference's serialization contract, not its verifier. They need not be
valid cryptographic signatures. Likewise, the encoder does not restrict
reservations to 256 or 512 bytes; supported RSA key sizes remain the RSA
module's concern.

## Validation and review

Six unit tests cover:

- Exact protobuf bytes, omission of the version field, and explicit empty
  fields (golden `0a 0a ...` fixtures).
- Known RSA-2048/RSA-4096 serialized sizes.
- Size-helper agreement with prost's own `encoded_len()` across padding
  and varint boundaries, plus decode round-trip of padding, unpadded size,
  and data presence.
- Oversized signatures and entry/message arithmetic overflow.
- The checked size agrees with the unchecked wire size at each boundary
  tested, and wrapping sizes (`usize::MAX` and its neighbours) are refused
  with `overflow`.
- An existing OpenSSL signature encoded with both exact and larger
  reservations, decoded, truncated to its recorded length, and
  successfully RSA-verified.

Validation performed:

- `cargo test -p ota-payload`: all unit tests pass, including the 6
  signing tests; doc-tests pass.
- `cargo test --workspace`: passes, including the 5 `ota-cli` end-to-end
  tests (the earlier non-contiguous-operation failures came from the
  hand-rolled manifest decoder and are gone with prost codegen).
- `cargo build --workspace`: clean, no warnings.
- `git diff --check`: passes.

Review kept the change limited to the signature encoder, its colocated
tests, and this document. There is no payload transport or new
cryptographic implementation.
