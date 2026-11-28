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

/// Work the service has in hand: turns running, waiting on a reply, or queued.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Activity { pub active: u64, pub waiting: u64, pub queued: u64 }

impl Activity {
    pub fn total(&self) -> u64 { self.active + self.waiting + self.queued }
}

/// The service's local activity counts (`GET /api/shell/activity` with the
/// local header; counts only, nothing about groups). `None` when it does not
/// answer, which callers must treat as busy.
pub fn activity(port: u16) -> Option<Activity> {
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    let mut stream = TcpStream::connect_timeout(&addr, Duration::from_millis(800)).ok()?;
    stream.set_read_timeout(Some(Duration::from_secs(3))).ok()?;
    stream.set_write_timeout(Some(Duration::from_secs(2))).ok()?;
    write!(stream, "GET /api/shell/activity HTTP/1.0\r\nHost: 127.0.0.1:{port}\r\nX-Sbot-Local: 1\r\nConnection: close\r\n\r\n").ok()?;
    let mut bytes = Vec::new();
    stream.take(64 * 1024).read_to_end(&mut bytes).ok()?;
    parse_activity(&String::from_utf8_lossy(&bytes))
}

fn parse_activity(response: &str) -> Option<Activity> {
    if parse_status(response)? != 200 { return None }
    let body = response.split_once("\r\n\r\n")?.1;
    let value: serde_json::Value = serde_json::from_str(body.trim()).ok()?;
    let count = |key: &str| value.get(key).and_then(serde_json::Value::as_u64);
    Some(Activity { active: count("active")?, waiting: count("waiting").unwrap_or(0), queued: count("queued").unwrap_or(0) })
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

    #[test]
    fn activity_counts() {
        let ok = "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\r\n{\"running\":true,\"active\":1,\"waiting\":0,\"queued\":2,\"held\":0}";
        let counts = parse_activity(ok).unwrap();
        assert_eq!((counts.active, counts.waiting, counts.queued, counts.total()), (1, 0, 2, 3));
        let idle = "HTTP/1.1 200 OK\r\n\r\n{\"running\":true,\"active\":0,\"waiting\":0,\"queued\":0}";
        assert_eq!(parse_activity(idle).unwrap().total(), 0);
        // An older service without the endpoint, or a refused request: unknown.
        assert_eq!(parse_activity("HTTP/1.1 401 Unauthorized\r\n\r\n{\"error\":\"x\"}"), None);
        assert_eq!(parse_activity("HTTP/1.1 200 OK\r\n\r\n<html>"), None);
    }
}
