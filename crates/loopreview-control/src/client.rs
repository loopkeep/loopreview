//! The control-plane client: connect to a session's socket, shake hands, and
//! exchange one request for one reply.
//!
//! Each `lr session` verb is a short-lived process, so the client is one-shot by
//! design: [`Client::connect`] performs the hello handshake, then [`Client::call`]
//! sends a single request and returns its reply.

use crate::error::{ControlError, Result};
use crate::protocol::{PROTOCOL_VERSION, Reply, Request, Response};
use crate::transport::{self, Connection};

use interprocess::local_socket::Stream;

/// A connected control client, past the handshake.
pub struct Client {
    conn: Connection<Stream>,
    /// The session id reported in the handshake.
    session: String,
}

impl Client {
    /// Connect to `socket` and complete the hello handshake.
    pub fn connect(socket: &str) -> Result<Client> {
        let mut conn = transport::connect(socket)?;
        conn.write(&Request::Hello {
            version: PROTOCOL_VERSION,
        })?;
        match conn.read::<Response>()? {
            Response::Ok(Reply::Hello(hello)) => {
                if hello.protocol != PROTOCOL_VERSION {
                    return Err(ControlError::Version {
                        ours: PROTOCOL_VERSION,
                        theirs: hello.protocol,
                    });
                }
                Ok(Client {
                    conn,
                    session: hello.session,
                })
            }
            Response::Error(message) => Err(ControlError::Remote(message)),
            _ => Err(ControlError::Unexpected),
        }
    }

    /// The session id from the handshake.
    pub fn session(&self) -> &str {
        &self.session
    }

    /// Send one request and return its reply, mapping a remote error.
    pub fn call(&mut self, request: &Request) -> Result<Reply> {
        self.conn.write(request)?;
        match self.conn.read::<Response>()? {
            Response::Ok(reply) => Ok(reply),
            Response::Error(message) => Err(ControlError::Remote(message)),
        }
    }
}
