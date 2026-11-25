//! The front door: what the hub knows about where a connection comes from, and the limits that follow.
//!
//! Stub of the wave two spine, see `docs/factory.md` sections 11.3 and 12.1. Network addresses are
//! personal data. They live in a side store that is never logged and never served. Other modules get
//! only the opaque block of a member through [`block_of`].

use crate::hub::Hub;

/// Settings of the front door. Lives in `HubConfig.door`. Everything is off by default.
#[derive(Clone, Debug, Default)]
pub struct DoorConfig {}

/// The opaque address block a member was last seen in, or `None` when unknown. Two members with the
/// same block count as one owner for reviews and for the quorum of test reports.
pub fn block_of(_hub: &Hub, _member: &str) -> Option<String> {
    None
}
