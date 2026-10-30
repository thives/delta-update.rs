//! Reference host bindings for the `no_std` OTA apply middleware: a
//! file-backed `Sink` + `Readable` device (for real host devices and disk
//! images), a read-only file-backed old image for `SOURCE_COPY`, a file-backed
//! payload `Source` for the streaming pipeline, and an in-memory device for
//! tests. `ota-apply` (in `main.rs`) wires these to the `ota-client` device
//! path, buffered by default or with `--stream`.

pub mod filecheckpoint;
pub mod fileimage;
pub mod filedevice;
pub mod filelock;
pub mod filesource;
pub mod identity;
pub mod ramdevice;
pub mod roles;
