//! Unix transport for the GLV1 helper IPC: filesystem sockets.
//!
//! The feeder BINDS this path; the helper CONNECTS to it. Stale socket
//! files from a crashed feeder are unlinked at bind time (same as before:
//! `bind` owns that cleanup, the common module never touches the fs).

#![cfg(unix)]

use std::io;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};

/// Accepted socket, feeder side (owns handshake + frame writes + ack reads).
pub type FeedStream = UnixStream;
/// Connected socket, helper side (owns handshake + frame reads + ack writes).
pub type HelperStream = UnixStream;
/// Bound socket, feeder side (accepts exactly one helper connection).
pub type Listener = UnixListener;

/// Helper binary file name on this OS (no `.exe` on Unix).
pub fn helper_file_name() -> &'static str {
    super::HELPER_NAME
}

/// Binds `sock_path`, nonblocking for the accept poll. Removes any stale
/// file first (crashed-feeder leftover), exactly as before.
pub fn bind(sock_path: &Path) -> io::Result<(Listener, PathBuf)> {
    let _ = std::fs::remove_file(sock_path);
    let listener = UnixListener::bind(sock_path)?;
    listener.set_nonblocking(true)?;
    Ok((listener, sock_path.to_path_buf()))
}

/// Connects to a bound `sock_path`.
pub fn connect(addr: &str) -> io::Result<HelperStream> {
    UnixStream::connect(addr)
}

/// Accepted sockets need no tuning on Unix (blocking IO + timeouts are set
/// by the common module after accept).
pub fn prepare_feed_stream(_sock: &FeedStream) {}
