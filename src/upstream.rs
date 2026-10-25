//! Upstream TCP connection management.

use std::{
    io,
    net::{SocketAddr, TcpStream},
    time::Duration,
};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const READ_TIMEOUT:    Duration = Duration::from_secs(30);
const WRITE_TIMEOUT:   Duration = Duration::from_secs(10);

/// Open a TCP connection to `addr` (e.g. "127.0.0.1:4433" or "host:443").
///
/// Errors:
/// - Connection refused  -> caller should return 502 Bad Gateway.
/// - Connection timeout  -> caller should return 504 Gateway Timeout.
pub fn connect(addr: &str) -> io::Result<TcpStream> {
    // Try parse as raw IP:port first to get connect_timeout support.
    let stream = if let Ok(sock) = addr.parse::<SocketAddr>() {
        TcpStream::connect_timeout(&sock, CONNECT_TIMEOUT)?
    } else {
        // Hostname: falls through to OS DNS resolution (no connect_timeout).
        TcpStream::connect(addr)?
    };
    stream.set_read_timeout(Some(READ_TIMEOUT))?;
    stream.set_write_timeout(Some(WRITE_TIMEOUT))?;
    Ok(stream)
}
