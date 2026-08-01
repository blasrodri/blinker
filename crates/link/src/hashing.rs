//! The workspace's fast hasher, re-exported so `crate::hashing::FastMap` keeps
//! meaning what it did.
//!
//! It moved to its own crate when `blinker-output`'s symbol-table encoder was
//! found interning every name through a `std::collections::HashMap` — SipHash,
//! on the stage that scales worst of any in the link (finding 130). `output`
//! cannot depend on `link`, so the hasher had to live below both.
pub use blinker_hashing::*;
