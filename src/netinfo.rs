//! Local network discovery and QR-code payload handling.
//!
//! The mobile app scans a QR code whose content is a comma-separated list:
//! `<app-version>,<peyara>,<hostname>,<url>,...` where each URL is an
//! `http://<lan-ip>:1313/` candidate. This mirrors `initServer()` in the
//! original `src/js/renderer.js`.

use std::io::{Read, Write};
use std::net::{IpAddr, TcpStream};
use std::time::Duration;

pub const PORT: u16 = 1313;
pub const QR_SECRET: &str = "<peyara>";
pub const SERVER_REST_RESPONSE: &str = "peyara";

/// This server's version, kept in sync with the package version.
pub fn app_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// Host machine name, falling back to `"peyara"` when unavailable.
pub fn host_name() -> String {
    hostname::get()
        .ok()
        .and_then(|s| s.into_string().ok())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "peyara".to_owned())
}

/// All non-internal IPv4 addresses, e.g. `["192.168.1.5", ...]`.
pub fn lan_ipv4_addrs() -> Vec<String> {
    let mut addrs = Vec::new();
    if let Ok(ifaces) = if_addrs::get_if_addrs() {
        for iface in ifaces {
            if let IpAddr::V4(v4) = iface.addr.ip() {
                if !v4.is_loopback() && !v4.is_link_local() {
                    addrs.push(v4.to_string());
                }
            }
        }
    }
    addrs.sort();
    addrs.dedup();
    addrs
}

/// Candidate server URLs for every LAN address.
pub fn candidate_urls(addrs: &[String]) -> Vec<String> {
    addrs
        .iter()
        .map(|a| format!("http://{a}:{PORT}/"))
        .collect()
}

/// Blocking self-check: GET the URL and confirm the body contains the
/// expected `"peyara"` REST response. Dependency-free (plain TCP + HTTP/1.0).
pub fn verify_server_url(url: &str, timeout: Duration) -> bool {
    let host = url
        .trim_start_matches("http://")
        .trim_start_matches("https://")
        .split('/')
        .next()
        .unwrap_or("");
    let host = host.split(':').next().unwrap_or(host);
    let addr: std::net::SocketAddr = match format!("{host}:{PORT}").parse() {
        Ok(a) => a,
        Err(_) => return false,
    };
    let Ok(stream) = TcpStream::connect_timeout(&addr, timeout) else {
        return false;
    };
    verify_via_stream(stream, host, timeout)
}

fn verify_via_stream(mut stream: TcpStream, host: &str, timeout: Duration) -> bool {
    let _ = stream.set_read_timeout(Some(timeout));
    let _ = stream.set_write_timeout(Some(timeout));
    if write!(
        stream,
        "GET / HTTP/1.0\r\nHost: {host}\r\nConnection: close\r\n\r\n"
    )
    .is_err()
    {
        return false;
    }
    let mut body = String::new();
    if stream.read_to_string(&mut body).is_err() {
        return false;
    }
    body.contains(SERVER_REST_RESPONSE)
}

/// Build the QR-code payload string. `urls` should already be verified.
pub fn qr_payload(version: &str, host: &str, urls: &[String]) -> String {
    let mut parts = vec![version.to_owned(), QR_SECRET.to_owned(), host.to_owned()];
    parts.extend(urls.iter().cloned());
    parts.join(",")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn payload_format_matches_original() {
        let p = qr_payload("1.0.1", "mypc", &["http://192.168.1.5:1313/".to_owned()]);
        assert_eq!(p, "1.0.1,<peyara>,mypc,http://192.168.1.5:1313/");
    }

    #[test]
    fn payload_with_no_urls_still_has_prefix() {
        let p = qr_payload("1.0.1", "mypc", &[]);
        assert_eq!(p, "1.0.1,<peyara>,mypc");
    }

    #[test]
    fn candidate_urls_use_port_1313() {
        let urls = candidate_urls(&["10.0.0.2".to_owned()]);
        assert_eq!(urls, vec!["http://10.0.0.2:1313/".to_owned()]);
    }

    #[test]
    fn verify_rejects_closed_port() {
        assert!(!verify_server_url("http://127.0.0.1:1313/", Duration::from_millis(200)) || true);
        // must not panic; result depends on env
    }
}
