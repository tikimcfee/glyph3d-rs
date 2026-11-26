//! Synchronous Unix-socket client for sending command lines to a running wall.

use std::io::BufReader;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::paths::{check_socket_parent, current_uid, verify_peer_uid, Target};
use crate::wire::{read_reply_frame, write_request_line, Error, Reply};

/// Connected control client capable of sending one or many sequential verb lines over a
/// single Unix stream connection.
pub struct Client {
    socket_path: PathBuf,
    reader: BufReader<UnixStream>,
    writer: UnixStream,
}

impl Client {
    /// Connect to `target`, verifying that the socket parent directory is a private
    /// `0700` directory owned by [`current_uid`] and that the listening server runs
    /// under [`current_uid`].
    pub fn connect(target: &Target) -> Result<Self, Error> {
        Self::connect_with_expected_uid(target, current_uid())
    }

    /// Connect to `target`, verifying ownership against `expected_uid`.
    pub fn connect_with_expected_uid(target: &Target, expected_uid: u32) -> Result<Self, Error> {
        let socket_path = target.resolve_socket().map_err(Error::InvalidTarget)?;
        check_socket_parent(&socket_path, current_uid())?;
        let stream = UnixStream::connect(&socket_path)?;
        verify_peer_uid(&stream, expected_uid)?;
        let writer = stream.try_clone()?;
        Ok(Client {
            socket_path,
            reader: BufReader::new(stream),
            writer,
        })
    }

    /// Resolved path of the connected Unix domain socket.
    pub fn socket_path(&self) -> &Path {
        &self.socket_path
    }

    /// Set read and write timeouts on the underlying socket (`None` blocks indefinitely).
    pub fn set_timeout(&mut self, timeout: Option<Duration>) -> std::io::Result<()> {
        self.writer.set_read_timeout(timeout)?;
        self.writer.set_write_timeout(timeout)?;
        Ok(())
    }

    /// Send a single verb line (e.g. `"wall.split right"`) and read back the wall's [`Reply`].
    pub fn send(&mut self, line: &str) -> Result<Reply, Error> {
        write_request_line(&mut self.writer, line)?;
        read_reply_frame(&mut self.reader)
    }
}

/// Connect once to `target`, send `line`, and return the resulting [`Reply`].
pub fn send_one(target: &Target, line: &str) -> Result<Reply, Error> {
    let mut client = Client::connect(target)?;
    client.send(line)
}
