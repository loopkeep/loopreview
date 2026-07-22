//! Errors surfaced by the control-plane client and transport.

use thiserror::Error;

/// Something went wrong talking to (or finding) a review session.
#[derive(Debug, Error)]
pub enum ControlError {
    /// The socket could not be reached — usually the session has exited.
    #[error("could not reach the session: {0}")]
    Connect(#[source] std::io::Error),

    /// A read or write on the connection failed.
    #[error("control connection failed: {0}")]
    Io(#[from] std::io::Error),

    /// A message could not be encoded or decoded.
    #[error("malformed control message: {0}")]
    Protocol(#[from] serde_json::Error),

    /// The peer closed the connection before replying.
    #[error("the session closed the connection")]
    Closed,

    /// A single message exceeded the maximum line length (a malformed or hostile
    /// peer); the connection is dropped.
    #[error("control message too large")]
    LineTooLong,

    /// The session speaks a different protocol version.
    #[error("session speaks protocol {theirs}, this client speaks {ours}")]
    Version {
        /// The client's protocol version.
        ours: u32,
        /// The session's protocol version.
        theirs: u32,
    },

    /// The session answered a request it should not have (a protocol violation).
    #[error("unexpected reply from the session")]
    Unexpected,

    /// The session rejected the request with a message.
    #[error("{0}")]
    Remote(String),
}

/// A result from the control-plane client.
pub type Result<T> = std::result::Result<T, ControlError>;
