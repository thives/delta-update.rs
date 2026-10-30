//! `no_std` OTA apply middleware with bounded allocation.
//!
//! Positioning: this crate is the middleware between payload *transport*
//! (deliberately out of project scope — hosts supply complete payload bytes)
//! and *device integration* (deferred — hosts implement the storage traits
//! in `sink.rs`). Everything in between is owned here: signature gating,
//! manifest policy, blob-cursor bookkeeping and block replay.
//!
//! Mirrors the A/B apply pipeline of the golden reference (`update_engine`)
//! restricted to the block flow: verify signatures, check the partition
//! hashes, then replay REPLACE/ZERO operations. Storage access is abstracted
//! by [`Sink`](sink.rs), the whole coupling surface of the core pipeline;
//! the [`Readable`](sink.rs) capability layer opts a host into the device
//! path's whole-image pre-flight and post-flight hashes. So future target
//! platforms (block stores, dm-verity devices, RAM images) plug in without
//! touching this crate. Payload transport is deliberately out of scope.
//!
//! [`stream`](stream.rs) is the second apply pipeline: it pulls the payload
//! through the [`Source`](source.rs) capability instead of taking a complete
//! buffer, so the delta never has to be resident, and it derives resume state
//! from the destination itself, so an interrupted update can be re-run to
//! convergence with no journal or device-side marker.

#![no_std]

extern crate alloc;

pub mod apply;
pub mod checkpoint;
pub mod engine;
pub mod image;
pub mod policy;
pub mod sink;
pub mod source;
pub mod stream;

#[cfg(test)]
mod testing;
