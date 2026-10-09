//! Tools the model can call (stage 1.5: registry + schemas; stages
//! 1.6–1.8: concrete tools).

pub mod list;
pub mod permissions;
pub mod read;
pub mod registry;
pub mod shell;
pub mod write;

mod path;

pub use list::{ListTool, MAX_LIST_ENTRIES};
pub use permissions::{AllowAll, Decision, DenyAll, Gate, PermissionGate};
pub use read::{MAX_READ_BYTES, ReadTool};
pub use registry::{Tool, ToolContext, ToolError, ToolRegistry};
pub use shell::{DEFAULT_TIMEOUT_SECS, MAX_OUTPUT_BYTES, MAX_TIMEOUT_SECS, RunCommandTool};
pub use write::WriteTool;
