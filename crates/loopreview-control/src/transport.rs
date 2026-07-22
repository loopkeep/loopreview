//! The local-socket transport: a JSON-Lines connection over an
//! [`interprocess`] local socket (a Unix domain socket, or a Windows named
//! pipe).
//!
//! A session's socket is addressed by a stored string: on Unix a filesystem path
//! under the sessions directory, on Windows a namespaced pipe name. The same
//! string round-trips through the registry so a client can reconnect. The wire
//! framing (one JSON object per line) is the same on both sides via
//! [`Connection`].

use std::io::{self, BufRead, BufReader, Read};

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

/// A cheap liveness probe: whether something is actually listening on `socket`.
///
/// A live session accepts the connect; a crashed one leaves a stale socket file
/// (Unix) or a vanished pipe (Windows) that refuses it. This distinguishes a
/// genuinely running session from a registry record whose pid has since been
/// reused by an unrelated process. The probe connection is dropped immediately;
/// the session's accept loop treats it as a peer that hung up.
pub fn is_reachable(socket: &str) -> bool {
    match name(socket) {
        Ok(name) => Stream::connect(name).is_ok(),
        Err(_) => false,
    }
}

/// The largest control message accepted, in bytes. A peer that sends a longer
/// line (or a line with no terminator) is dropped rather than allowed to grow the
/// read buffer without bound.
const MAX_LINE_BYTES: u64 = 1 << 20; // 1 MiB

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
    /// the peer hangs up first, and [`ControlError::LineTooLong`] when a single
    /// message exceeds [`MAX_LINE_BYTES`] (so a hostile peer cannot exhaust
    /// memory with an unterminated line).
    pub fn read<T: DeserializeOwned>(&mut self) -> Result<T> {
        let mut buf = Vec::new();
        let read = (&mut self.reader)
            .take(MAX_LINE_BYTES)
            .read_until(b'\n', &mut buf)?;
        if read == 0 {
            return Err(ControlError::Closed);
        }
        // The cap was reached without a line terminator: the message is too long.
        if read as u64 == MAX_LINE_BYTES && buf.last() != Some(&b'\n') {
            return Err(ControlError::LineTooLong);
        }
        // `from_slice` validates UTF-8 and tolerates the trailing newline.
        Ok(serde_json::from_slice(&buf)?)
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::Request;

    /// A Read+Write double: reads from a fixed buffer, discards writes.
    struct Fake {
        data: io::Cursor<Vec<u8>>,
    }

    impl io::Read for Fake {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            self.data.read(buf)
        }
    }

    impl io::Write for Fake {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            Ok(buf.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    fn conn(bytes: Vec<u8>) -> Connection<Fake> {
        Connection::new(Fake {
            data: io::Cursor::new(bytes),
        })
    }

    #[test]
    fn reads_a_normal_message() {
        let mut c = conn(b"{\"op\":\"get\"}\n".to_vec());
        assert_eq!(c.read::<Request>().unwrap(), Request::Get);
    }

    #[test]
    fn empty_stream_reports_closed() {
        let mut c = conn(Vec::new());
        assert!(matches!(
            c.read::<Request>().unwrap_err(),
            ControlError::Closed
        ));
    }

    #[test]
    fn an_over_length_line_is_rejected() {
        // A line past the cap with no terminator must not grow the buffer without
        // bound; it is reported as too long so the connection can be dropped.
        let big = vec![b'a'; MAX_LINE_BYTES as usize + 16];
        let mut c = conn(big);
        assert!(matches!(
            c.read::<Request>().unwrap_err(),
            ControlError::LineTooLong
        ));
    }
}
