//! `java-agent-debugger` — library root.
//!
//! Cross-platform jdb wrapper engine: spawn the JDK's `jdb`, read output with prompt awareness,
//! parse it into structured results, and keep debug sessions alive in stateful [`session::Session`]s.
//! See CLAUDE.md §6 for module boundaries.
//!
//! Layering, with high cohesion and low coupling:
//! - [`error`] / [`protocol`]: foundational types with no internal dependencies.
//! - [`jdkpath`]: locate the jdb executable.
//! - [`jdb`]: jdb engine subsystem (spawn / read / parse), cooperating internally without depending on upper layers.
//! - [`session`]: coordination layer that binds the jdb child and reader thread and drives the RunState machine.
//! - [`registry`]: on-disk registry (`daemon.json` / `sessions.json`).
//! - [`daemon`]: IPC listener, session management, and lifecycle.
//! - [`client`]: CLI-side RPC client for connecting to the daemon.
//! - bin (`src/main.rs`): CLI / daemon entry point, depending only on the library's public API.

pub mod backend;
pub mod cli;
pub mod client;
pub mod daemon;
pub mod error;
pub mod jdb;
pub mod jdi;
pub mod jdkpath;
pub mod mcp;
pub mod output;
pub mod protocol;
pub mod registry;
pub mod session;
pub mod setup;
pub mod tui;
pub mod update;

pub use error::{Error, Result};
