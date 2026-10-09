//! Tools the model can call (stage 1.5: registry + schemas; concrete
//! tools land in stages 1.6–1.8).

pub mod registry;

pub use registry::{Tool, ToolContext, ToolError, ToolRegistry};
