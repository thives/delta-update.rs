//! Wire-compatible implementation of Google's update_engine payload format
//! (Android `CrAU` / `DeltaArchiveManifest` variant).
//!
//! Protobuf message types (manifest, operations, partition updates,
//! signatures) are *generated* by `prost-build` from the crate's vendored
//! `schema/update_metadata.proto` (see `build.rs`) and exposed as the
//! `schema` module; they are re-exported under short names by `manifest`.
//! The vendored schema is a copy of the golden reference's
//! `update_metadata.proto`: our code never reads from the `update_engine/`
//! reference tree, it is kept in sync manually (see `build.rs`).
//! Everything that is not protobuf — the `CrAU` header framing, blob layout,
//! RSA signing and verification, SHA-256 hashing — is hand-written here and
//! mirrors `payload_metadata.cc`, `payload_signer.cc` and `payload_verifier.cc`
//! from the golden reference tree.
//!
//! This crate remains `no_std`: `prost` is used with `default-features = false`.
//! Decoding is preceded by a bounded pre-decode scan (`prescan`) against a
//! `ResourceProfile` (`profile`), so every allocation prost makes is charged
//! before it happens. Authentication (`verify`) runs before the manifest is
//! decoded, and metadata policy (`plan`) runs before anything is applied.
//! Unlike the previous hand-rolled wire codec, message decode/encode now
//! allocates through `prost`'s `Vec`/`String` buffers (via `alloc`), as
//! does RSA key parsing.

#![no_std]

extern crate alloc;

pub mod encoding;
pub mod error;
pub mod layout;
pub mod manifest;
pub mod plan;
pub mod prescan;
pub mod profile;
pub mod rsa;
pub mod sha256;
pub mod sign;
pub mod verify;

pub mod schema {
    include!(env!("OTA_SCHEMA_FILE"));
}
