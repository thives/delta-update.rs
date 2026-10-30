//! What an application must establish before it writes, and what a successful
//! application is allowed to claim.
//!
//! Two questions are kept apart. [`Trust`](crate::engine::Trust) says how the payload's
//! metadata was authenticated, at `prepare`. [`Policy`] says what the host requires of
//! the update at apply time, and it is the only place a weaker result is permitted.
//! [`Outcome`] reports the strength of a success, so the two cannot be confused: only
//! [`Outcome::Verified`] means the target was read back and matched the authenticated
//! new image, under authenticated metadata.
//!
//! `docs/client.md` §10 states the strict requirements; `docs/platform.md` states what
//! the host must provide around them.

use ota_payload::error::PayloadError;

/// The host's record of the lowest release it still accepts: the rollback floor.
///
/// The value must come from state an attacker cannot roll back, such as a counter in
/// secure storage or a fuse. A checkpoint file, an ordinary configuration file or a
/// value passed on the command line is not such state. The client reads the floor
/// once, before its first write. It never advances the floor; the host does that only
/// after boot confirmation (`docs/platform.md` §3).
pub trait ReleaseFloor {
    /// The lowest acceptable release version. An error refuses the update before any
    /// write.
    fn minimum(&self) -> Result<u64, PayloadError>;
}

/// What the host requires of an update when it applies it.
#[derive(Clone, Copy)]
pub enum Policy<'f> {
    /// The strict policy, and the recommended one. The update is refused before any
    /// write unless all of these hold:
    ///
    /// - the metadata and payload were authenticated (`Trust::Verified`);
    /// - the signed new image records a size and a 32-byte SHA-256;
    /// - the target is readable, so the new image can be read back;
    /// - the payload carries a release version, at or above the floor;
    /// - when the program is base-dependent, the signed old image records a size and
    ///   a hash, and the destination must match them before the first write.
    ///
    /// A success is [`Outcome::Verified`].
    Strict(&'f dyn ReleaseFloor),
    /// No requirement beyond what `prepare` established, and no release floor.
    /// Development and test use only. The outcome is [`Outcome::Verified`] only when
    /// the metadata was authenticated and the new image was read back, and
    /// [`Outcome::Applied`] otherwise.
    Development,
}

/// How strongly a successful application is established.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Outcome {
    /// Every operation was written, flushed and committed, and the payload stream
    /// ended correctly. The target was not read back against the new image, or the
    /// metadata was not authenticated. This is not an installation the host may
    /// activate on the client's word.
    Applied,
    /// The target was read back and its new-image prefix matched the signed hash, and
    /// the metadata was authenticated. Activation may follow, subject to the host's own
    /// commit ordering.
    Verified,
}

/// A rollback floor held in memory. It is volatile, lost on restart, and not
/// rollback-resistant, so it is for tests and dry runs only.
pub struct VolatileFloor {
    minimum: u64,
}

impl VolatileFloor {
    pub fn new(minimum: u64) -> VolatileFloor {
        return VolatileFloor { minimum: minimum };
    }

    /// Raise the floor to `to`. A floor never moves down, so a lower value is ignored.
    pub fn advance(self: &mut Self, to: u64) {
        if to > self.minimum {
            self.minimum = to;
        }
    }
}

impl ReleaseFloor for VolatileFloor {
    fn minimum(&self) -> Result<u64, PayloadError> {
        return Ok(self.minimum);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_volatile_floor_only_moves_up() {
        let mut floor = VolatileFloor::new(5);
        floor.advance(3);
        assert_eq!(floor.minimum().unwrap(), 5);
        floor.advance(9);
        assert_eq!(floor.minimum().unwrap(), 9);
    }
}
