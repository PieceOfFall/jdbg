//! Wire / output 类型——解析器的产出物、CLI 的渲染输入、IPC 协议。
//!
//! 拆分为两个子模块：
//! - [`result`]：输出 schema（CommandResult 及其组成类型）——解析器产出、渲染输入。
//! - [`wire`]：IPC wire 类型（Request/Response/Command）——CLI↔Daemon 协议。

pub mod result;
pub mod wire;

// Re-export everything at crate::protocol level for backward compatibility.
pub use result::*;
pub use wire::*;
