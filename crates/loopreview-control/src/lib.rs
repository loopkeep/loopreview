//! loopreview-control: the session control plane behind loopreview's `lr
//! session` verbs.
//!
//! A running review UI hosts a local socket and registers itself so that an
//! external agent (or a second terminal) can discover it, read the diff and
//! threads, steer the human's view, leave draft comments, and wait for events —
//! all without a central daemon. This crate holds the pieces both sides share:
//!
//! * the [`protocol`] types (JSON-Lines request/response with a hello handshake);
//! * the [`transport`] (an [`interprocess`] local socket: Unix socket / Windows
//!   named pipe);
//! * the session [`registry`] (per-session JSON records, with liveness-based
//!   cleanup of dead sessions);
//! * the [`events`] log a session publishes and a `wait` blocks on;
//! * a [`client`] that speaks the protocol.
//!
//! Like loopreview-core it carries no UI dependency, so it can be reused and, in
//! time, published on its own.

pub mod client;
pub mod error;
pub mod events;
pub mod protocol;
pub mod registry;
pub mod transport;

pub use client::Client;
pub use error::{ControlError, Result};
pub use events::EventLog;
pub use protocol::PROTOCOL_VERSION;
pub use registry::SessionRecord;
