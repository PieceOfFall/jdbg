//! `java-agent-debugger` — 库根。
//!
//! 跨平台 jdb 包装引擎：spawn JDK 的 `jdb`、prompt-aware 读取输出、解析为结构化结果，
//! 并以有状态的 [`session::Session`] 在后台维持调试会话。模块划分见 CLAUDE.md §6。
//!
//! 分层（高内聚低耦合）：
//! - [`error`] / [`protocol`]：基础类型，无内部依赖。
//! - [`jdkpath`]：定位 jdb 可执行文件。
//! - [`jdb`]：jdb 引擎子系统（spawn / 读取 / 解析），彼此协作但不依赖上层。
//! - [`session`]：协调层——绑定 jdb 子进程与读取线程，驱动 RunState 状态机。
//! - [`registry`]：磁盘注册表（daemon.json / sessions.json）。
//! - [`daemon`]：IPC 监听、会话管理、生命周期。
//! - [`client`]：CLI 端连接 daemon 的 RPC 客户端。
//! - bin (`src/main.rs`)：CLI / daemon 入口，仅依赖本库的公共 API。

pub mod cli;
pub mod client;
pub mod daemon;
pub mod error;
pub mod jdb;
pub mod jdkpath;
pub mod mcp;
pub mod output;
pub mod protocol;
pub mod registry;
pub mod session;
pub mod setup;
pub mod update;

pub use error::{Error, Result};
