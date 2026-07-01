//! JDI backend support.
//!
//! The existing `jdb` backend remains the compatibility path. This module owns
//! the sidecar protocol, lifecycle, transport, and JDI session integration.

pub mod codec;
pub mod lifecycle;
pub mod protocol;
pub mod session;
pub mod transport;
