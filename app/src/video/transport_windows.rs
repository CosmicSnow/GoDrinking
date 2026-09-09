//! Windows transport for the GLV1 helper IPC: TCP loopback.
//!
//! No filesystem sockets on Windows, so the feeder BINDS an ephemeral
//! `127.0.0.1` port and hands its address (as text) to the helper, which
//! CONNECTS to it. Loopback-only by construction: never exposed to the LAN.

#![cfg(windows)]

use std::io;
use std::path::{Path, PathBuf};

/// Accepted socket, feeder side (owns handshake + frame writes + ack reads).
pub type FeedStream = std::net::TcpStream;
/// Connected socket, helper side (owns handshake + frame reads + ack writes).
pub type HelperStream = std::net::TcpStream;
/// Bound socket, feeder side (accepts exactly one helper connection).
pub type Listener = std::net::TcpListener;

/// Helper binary file name on this OS (`.exe` on Windows).
pub fn helper_file_name() -> &'static str {
    "golive-video.exe"
}

/// Binds an ephemeral loopback port. `sock_path` is ignored (kept for
/// signature parity with the Unix transport); the returned `PathBuf`
/// carries the `127.0.0.1:port` address text the helper connects to.
pub fn bind(_sock_path: &Path) -> io::Result<(Listener, PathBuf)> {
    let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
    listener.set_nonblocking(true)?;
    let addr = listener.local_addr()?;
    Ok((listener, PathBuf::from(addr.to_string())))
}

/// Connects to a bound `127.0.0.1:port` address (as handed out by [`bind`]).
pub fn connect(addr: &str) -> io::Result<HelperStream> {
    let sock = std::net::TcpStream::connect(addr)?;
    sock.set_nodelay(true)?;
    Ok(sock)
}

/// Interactive video acks are latency-sensitive: disable Nagle on the
/// accepted socket (the helper side already sets it in [`connect`]).
pub fn prepare_feed_stream(sock: &FeedStream) {
    let _ = sock.set_nodelay(true);
}
