//! What is answering on the console port. Only meaningful once launchd reports
//! our job running; the caller checks that first, so a foreign program on the
//! same port is never presented as SintBot.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::time::Duration;

use serde::Serialize;

#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Http {
    Down,
    /// No chat platform configured yet: the service is serving the connect page.
    Setup,
    Console,
}

fn status_of(port: u16, path: &str) -> Option<u16> {
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    let mut stream = TcpStream::connect_timeout(&addr, Duration::from_millis(800)).ok()?;
    stream.set_read_timeout(Some(Duration::from_secs(3))).ok()?;
    stream.set_write_timeout(Some(Duration::from_secs(2))).ok()?;
    write!(stream, "GET {path} HTTP/1.0\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n").ok()?;
    let mut head = [0u8; 64];
    let read = stream.read(&mut head).ok()?;
    parse_status(&String::from_utf8_lossy(&head[..read]))
}

fn parse_status(head: &str) -> Option<u16> {
    let mut parts = head.split_whitespace();
    if !parts.next()?.starts_with("HTTP/") { return None }
    parts.next()?.parse().ok()
}

pub fn probe(port: u16) -> Http {
    match status_of(port, "/api/setup/state") {
        None => Http::Down,
        Some(200) => Http::Setup,
        Some(_) => if status_of(port, "/") == Some(200) { Http::Console } else { Http::Down },
    }
}

pub fn port_in_use(port: u16) -> bool {
    TcpStream::connect_timeout(&SocketAddr::from(([127, 0, 0, 1], port)), Duration::from_millis(500)).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_line() {
        assert_eq!(parse_status("HTTP/1.1 200 OK\r\n"), Some(200));
        assert_eq!(parse_status("HTTP/1.0 401 Unauthorized"), Some(401));
        assert_eq!(parse_status("SSH-2.0-OpenSSH"), None);
    }
}
