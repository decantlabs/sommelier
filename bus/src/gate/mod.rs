//! Static gates for a submission: checks that read a patch and never build or run it.
//!
//! Part of the public factory, see `docs/factory.md` section 8.2. This file is the stub that the spine
//! commit created. The gates are pure functions over a parsed diff and an order.

pub mod diff;
