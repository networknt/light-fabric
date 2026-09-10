//! Owner-local persistent multi-repository workspaces.
//!
//! The caller is trusted runner code, not an authenticated public API. Agent
//! identities must come from the caller's authenticated execution context.
//! Filesystem ownership is the local administration boundary. Worker processes
//! must use `sandbox_command`; handing out the host path is not enforcement.
mod checkpoint;
mod delivery;
mod discovery;
mod git;
mod indexing;
mod model;
mod store;

pub use delivery::DeliveryReceipt;
pub use discovery::{Discovery, SkippedRepository, discover};
pub use indexing::{IndexReceipt, IndexerCommand};
pub use model::*;
pub use store::*;
