//! In-memory device: a byte buffer, addressed by byte offset. Used by host tests
//! and dry-run tooling; on a real device it is the last resort because it holds
//! the whole partition in RAM. It implements both [`Sink`] and [`Readable`](delta-update-client),
//! so the client middleware's full device pipeline runs against it.
//!
//! The buffer is volatile: its `flush` has nothing to persist to, so it reports
//! success without a durability guarantee. Use it for tests and dry runs only,
//! never as the target of a real update.

use delta_update_client::sink::Readable;
use delta_update_client::sink::Sink;
use delta_update_client::sink::check_range;
use delta_update_payload::error::PayloadError;

pub struct RamDevice {
    data: Vec<u8>,
}

impl RamDevice {
    /// Wrap `data` as a device. Its length is the capacity.
    pub fn new(data: Vec<u8>) -> RamDevice {
        return RamDevice { data: data };
    }

    pub fn bytes(&self) -> &[u8] {
        return &self.data;
    }
}

impl Sink for RamDevice {
    fn capacity(self: &RamDevice) -> u64 {
        return self.data.len() as u64;
    }

    fn write_at(self: &mut RamDevice, offset: u64, data: &[u8]) -> Result<(), PayloadError> {
        check_range(offset, data.len(), self.capacity())?;
        let off = offset as usize;
        self.data[off..(off + data.len())].copy_from_slice(data);
        return Ok(());
    }

    fn flush(self: &mut RamDevice) -> Result<(), PayloadError> {
        return Ok(());
    }
}

impl Readable for RamDevice {
    fn read_at(self: &RamDevice, offset: u64, out: &mut [u8]) -> Result<(), PayloadError> {
        check_range(offset, out.len(), self.capacity())?;
        let off = offset as usize;
        out.copy_from_slice(&self.data[off..(off + out.len())]);
        return Ok(());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn access_is_bounded_by_the_capacity() {
        let mut dev = RamDevice::new(vec![0u8; 16]);
        assert!(dev.write_at(10, &[1; 7]).is_err());
        dev.write_at(9, &[2; 7]).unwrap();
        assert_eq!(dev.bytes()[9..16], [2; 7]);
        let mut out = [0u8; 3];
        assert!(dev.read_at(14, &mut out).is_err());
        dev.read_at(13, &mut out).unwrap();
        assert_eq!(out, [2, 2, 2]);
    }
}
