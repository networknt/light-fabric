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
mod inspection;
mod job_execution;
mod jobs;
mod model;
mod runner_config;
mod store;
mod tool_request;
mod tool_session;

pub use delivery::DeliveryReceipt;
pub use discovery::{Discovery, SkippedRepository, discover};
pub use indexing::{IndexReceipt, IndexerCommand};
pub use inspection::*;
pub use job_execution::*;
pub use jobs::*;
pub use model::*;
pub use runner_config::*;
pub use store::*;
pub use tool_request::workspace_tool_definition;
pub use tool_session::*;
