//! IPC between the daemon and the UI: a small line-delimited JSON protocol
//! ([`proto`]) over a stream transport ([`transport`]) with an async
//! [`client::IpcClient`].
//!
//! The daemon side (accepting connections and answering requests) lives in
//! `daemon::server`; this module has no dependency on it.

pub mod client;
pub mod proto;
pub mod transport;

pub use client::IpcClient;
pub use proto::*;
#[cfg(unix)]
pub use transport::UnixTransport;
pub use transport::{default_socket_path, default_transport, transport_at, IpcTransport};
#[cfg(windows)]
pub use transport::NamedPipeTransport;
