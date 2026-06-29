//! Wire and output types: parser outputs, CLI rendering inputs, and the IPC protocol.
//!
//! Split into two submodules:
//! - [`result`]: output schema (`CommandResult` and related types), used as parser output and rendering input.
//! - [`wire`]: IPC wire types (`Request`/`Response`/`Command`) for the CLI↔Daemon protocol.

pub mod result;
pub mod wire;

// Re-export everything at crate::protocol level for backward compatibility.
pub use result::*;
pub use wire::*;
