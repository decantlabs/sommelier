//! Submissions as git bundles: packing on the member side, checking and safe unpacking on the hub side.
//!
//! Part of the public factory, see `docs/factory.md` sections 8.1 and 8.2. This file is the stub that the
//! spine commit created. Unpacking produces a [`crate::gate::diff::ParsedDiff`] and never checks out with
//! hooks, never builds and never runs anything from the bundle.
