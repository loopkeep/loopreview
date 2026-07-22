//! The local-socket transport: a JSON-Lines connection over an
//! [`interprocess`] local socket (a Unix domain socket, or a Windows named
//! pipe).
//!
//! A session's socket is addressed by a stored string: on Unix a filesystem path
//! under the sessions directory, on Windows a namespaced pipe name. The same
//! string round-trips through the registry so a client can reconnect. The wire
//! framing (one JSON object per line) is the same on both sides via
//! [`Connection`].

use std::io::{self, BufRead, BufReader};

use interprocess::local_socket::traits::{ListenerExt, Stream as _};
#[cfg(not(windows))]
use interprocess::local_socket::{GenericFilePath, ToFsName};
#[cfg(windows)]
use interprocess::local_socket::{GenericNamespaced, ToNsName};
use interprocess::local_socket::{ListenerOptions, Name, Stream};
use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::error::{ControlError, Result};

/// A local-socket listener that accepts control connections.
pub type Listener = interprocess::local_socket::Listener;

/// The socket identifier to store for a session with `id`.
///
/// On Windows this is a namespaced pipe name. On Unix it is a filesystem path,
/// placed in the temp directory rather than the (possibly deeply nested) config
/// directory: a Unix domain socket path must fit in `sun_path` (about 104 bytes
/// on macOS, 108 on Linux), which a long `$HOME`/`$XDG_CONFIG_HOME` would blow
/// past. The registry record carries this path, so the socket's location is
/// independent of where the record lives. A pathologically long temp dir falls
/// back to `/tmp`.
pub fn socket_id(id: &str) -> String {
    #[cfg(windows)]
    {
        format!("loopreview-{id}.sock")
    }
    #[cfg(not(windows))]
    {
        let name = format!("loopreview-{id}.sock");
        let path = std::env::temp_dir().join(&name);
        if path.as_os_str().len() < 100 {
            path.to_string_lossy().into_owned()
        } else {
            std::path::Path::new("/tmp")
                .join(&name)
                .to_string_lossy()
                .into_owned()
        }
    }
}

/// Resolve a stored socket identifier into an addressable name.
fn name(socket: &str) -> io::Result<Name<'_>> {
    #[cfg(windows)]
    {
        socket.to_ns_name::<GenericNamespaced>()
    }
    #[cfg(not(windows))]
    {
        socket.to_fs_name::<GenericFilePath>()
    }
}

/// Start listening on `socket`. On Unix a stale socket file from a crashed
/// session is removed first so binding does not fail with "address in use".
pub fn listen(socket: &str) -> io::Result<Listener> {
    #[cfg(not(windows))]
    {
        let _ = std::fs::remove_file(socket);
    }
    ListenerOptions::new().name(name(socket)?).create_sync()
}

/// Connect to the session listening on `socket`.
pub fn connect(socket: &str) -> Result<Connection<Stream>> {
    let stream = Stream::connect(name(socket)?).map_err(ControlError::Connect)?;
    Ok(Connection::new(stream))
}

/// A framed JSON-Lines connection over a byte stream.
///
/// Reads buffer through a [`BufReader`]; writes go straight to the underlying
/// stream. The control protocol is strictly request/response (half duplex), so a
/// single stream carries both directions without splitting.
pub struct Connection<S> {
    reader: BufReader<S>,
}

impl<S: io::Read + io::Write> Connection<S> {
    /// Wrap a stream.
    pub fn new(stream: S) -> Connection<S> {
        Connection {
            reader: BufReader::new(stream),
        }
    }

    /// Read one message (one line of JSON). Returns [`ControlError::Closed`] when
    /// the peer hangs up first.
    pub fn read<T: DeserializeOwned>(&mut self) -> Result<T> {
        let mut line = String::new();
        let read = self.reader.read_line(&mut line)?;
        if read == 0 {
            return Err(ControlError::Closed);
        }
        Ok(serde_json::from_str(line.trim_end())?)
    }

    /// Write one message as a single line, then flush.
    pub fn write<T: Serialize>(&mut self, message: &T) -> Result<()> {
        let stream = self.reader.get_mut();
        let mut line = serde_json::to_vec(message)?;
        line.push(b'\n');
        stream.write_all(&line)?;
        stream.flush()?;
        Ok(())
    }
}

/// Iterate accepted connections, ignoring individual accept errors.
pub fn incoming(listener: &Listener) -> impl Iterator<Item = Stream> + '_ {
    listener.incoming().filter_map(std::result::Result::ok)
}
