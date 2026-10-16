//! Local network discovery and QR-code payload handling.
//!
//! The mobile app scans a QR code whose content is a comma-separated list:
//! `<app-version>,<peyara>,<hostname>,sha256:<fingerprint>,<url>,...` where
//! each URL is an `https://<lan-ip>:1313/` candidate and the fingerprint is
//! the lowercase hex SHA-256 of the server's ephemeral certificate DER.
//! Full spec: `docs/qr-code-format.md`.

use std::net::{IpAddr, TcpStream};
use std::time::Duration;

pub const PORT: u16 = 1313;
pub const QR_SECRET: &str = "<peyara>";
pub const SERVER_REST_RESPONSE: &str = "peyara";
/// QR field prefix for the certificate fingerprint (see `tls` module).
pub const FINGERPRINT_PREFIX: &str = "sha256:";
/// QR field prefix for the pairing code (see below).
pub const PAIR_PREFIX: &str = "pair:";

/// Mint a fresh 8-digit hex pairing code (32 bits from the OS RNG).
/// The code is a per-launch secret shown only in the QR: a phone must echo
/// it in its first `hello` message or the server closes the connection.
pub fn generate_pairing() -> Result<String, String> {
    use rand::TryRngCore;
    let mut buf = [0u8; 4];
    rand::rngs::OsRng
        .try_fill_bytes(&mut buf)
        .map_err(|e| format!("pairing rng failed: {e}"))?;
    Ok(hex::encode(buf))
}

/// Extract the `pair:<8hex>` pairing-code field from a QR payload, if present.
/// (Documented client contract; exercised by unit tests.)
#[allow(dead_code)]
pub fn parse_pairing(qr_payload: &str) -> Option<String> {
    qr_payload.split(',').find_map(|field| {
        field
            .strip_prefix(PAIR_PREFIX)
            .filter(|c| c.len() == 8 && c.chars().all(|c| c.is_ascii_hexdigit()))
            .map(str::to_owned)
    })
}

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

/// A usable IPv4 address together with its interface name.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct LanAddr {
    /// Interface name, e.g. `"wlo1"`.
    pub name: String,
    /// IPv4 address, e.g. `"192.168.1.16"`.
    pub ip: String,
}

/// Interface name prefixes that are never the phone's LAN: container/VM
/// bridges and virtual links. They are still advertised (last) when they
/// verify, but never preferred.
const VIRTUAL_PREFIXES: &[&str] = &[
    "virbr",
    "docker",
    "veth",
    "br-",
    "lxc",
    "cni",
    "vmnet",
    "vboxnet",
    "tap",
    "tun",
    "wg",
    "tailscale",
    "warp",
];

pub fn is_virtual_iface(name: &str) -> bool {
    let lower = name.to_lowercase();
    VIRTUAL_PREFIXES.iter().any(|p| lower.starts_with(p))
}

/// All usable IPv4 addresses with interface names, ranked best-first:
/// default-route interface, then other physical interfaces, then virtual
/// ones (e.g. `virbr0`, `docker0` — unreachable from the phone on Wi-Fi).
pub fn lan_addrs() -> Vec<LanAddr> {
    let mut raw = Vec::new();
    if let Ok(ifaces) = if_addrs::get_if_addrs() {
        for iface in ifaces {
            if let IpAddr::V4(v4) = iface.addr.ip() {
                if v4.is_loopback()
                    || v4.is_link_local()
                    || v4.is_unspecified()
                    || v4.is_multicast()
                    || v4.is_broadcast()
                {
                    continue;
                }
                raw.push(LanAddr {
                    name: iface.name,
                    ip: v4.to_string(),
                });
            }
        }
    }
    rank_lan_addrs(raw, default_iface().as_deref())
}

/// Order addresses: default-route interface first, then other physical
/// interfaces, then virtual ones. Pure (testable) half of [`lan_addrs`].
pub fn rank_lan_addrs(mut addrs: Vec<LanAddr>, default: Option<&str>) -> Vec<LanAddr> {
    addrs.sort();
    addrs.dedup();
    let mut first = Vec::new();
    let mut phys = Vec::new();
    let mut virt = Vec::new();
    for a in addrs {
        if is_virtual_iface(&a.name) {
            virt.push(a);
        } else if default == Some(a.name.as_str()) {
            first.push(a);
        } else {
            phys.push(a);
        }
    }
    first.into_iter().chain(phys).chain(virt).collect()
}

/// Name of the interface holding the default route (Linux), if any.
pub fn default_iface() -> Option<String> {
    #[cfg(target_os = "linux")]
    {
        std::fs::read_to_string("/proc/net/route")
            .ok()
            .and_then(|t| default_iface_from_table(&t))
    }
    #[cfg(not(target_os = "linux"))]
    {
        None
    }
}

/// Parse `/proc/net/route` text: first interface whose destination is
/// `00000000` with the gateway flag set. Pure (testable) half of
/// [`default_iface`].
pub fn default_iface_from_table(table: &str) -> Option<String> {
    for line in table.lines().skip(1) {
        let f: Vec<&str> = line.split_whitespace().collect();
        if f.len() >= 4 && f[1] == "00000000" {
            let gateway = u32::from_str_radix(f[3], 16)
                .map(|flags| flags & 0x2 != 0)
                .unwrap_or(false);
            if gateway {
                return Some(f[0].to_owned());
            }
        }
    }
    None
}

/// One-line summary of discovered addresses for the UI log, e.g.
/// `"wlo1=192.168.1.16, virbr0=192.168.122.1 (virtual)"`.
pub fn describe_addrs(addrs: &[LanAddr]) -> String {
    if addrs.is_empty() {
        return "no LAN addresses found".to_owned();
    }
    addrs
        .iter()
        .map(|a| {
            if is_virtual_iface(&a.name) {
                format!("{}={} (virtual)", a.name, a.ip)
            } else {
                format!("{}={}", a.name, a.ip)
            }
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// Candidate server URLs for every address, rank order preserved
/// (TLS-only, no plaintext).
pub fn candidate_urls(addrs: &[LanAddr]) -> Vec<String> {
    addrs
        .iter()
        .map(|a| format!("https://{}:{PORT}/", a.ip))
        .collect()
}

/// Derive the WebSocket endpoint from an `https://` candidate URL:
/// `https://192.168.1.5:1313/` -> `wss://192.168.1.5:1313/ws`.
/// (Documented client contract; exercised by unit tests.)
#[allow(dead_code)]
pub fn ws_url(https_url: &str) -> String {
    let rest = https_url
        .trim_start_matches("https://")
        .trim_start_matches("http://")
        .trim_end_matches('/');
    format!("wss://{rest}/ws")
}

/// Extract the `sha256:<hex>` fingerprint field from a QR payload, if present.
/// (Documented client contract; exercised by unit tests.)
#[allow(dead_code)]
pub fn parse_fingerprint(qr_payload: &str) -> Option<String> {
    qr_payload.split(',').find_map(|field| {
        field
            .strip_prefix(FINGERPRINT_PREFIX)
            .filter(|fp| fp.len() == 64 && fp.chars().all(|c| c.is_ascii_hexdigit()))
            .map(str::to_owned)
    })
}

/// Blocking self-check: TLS `GET` the URL trusting exactly `cert_der` and
/// confirm the body contains the expected `"peyara"` discovery marker.
/// There is no plaintext fallback — the server is TLS-only.
pub fn verify_server_url(url: &str, cert_der: &[u8], timeout: Duration) -> bool {
    crate::tls::verify_server_tls(url, cert_der, timeout)
}

/// One candidate address plus its TLS self-check outcome.
#[derive(Debug, Clone)]
pub struct AddrCheck {
    pub addr: LanAddr,
    pub url: String,
    pub verified: bool,
    /// False for container/VM/virtual interfaces.
    pub physical: bool,
}

/// TLS-check every candidate in parallel; retry only the still-unverified
/// ones. A slow first handshake must never permanently hide an address.
pub fn verify_all(addrs: &[LanAddr], cert_der: &[u8]) -> Vec<AddrCheck> {
    let mut state: Vec<AddrCheck> = addrs
        .iter()
        .map(|a| AddrCheck {
            url: format!("https://{}:{PORT}/", a.ip),
            addr: a.clone(),
            verified: false,
            physical: !is_virtual_iface(&a.name),
        })
        .collect();
    for _ in 0..6 {
        let pending: Vec<(usize, String)> = state
            .iter()
            .enumerate()
            .filter(|(_, c)| !c.verified)
            .map(|(i, c)| (i, c.url.clone()))
            .collect();
        if pending.is_empty() {
            break;
        }
        let handles: Vec<_> = pending
            .into_iter()
            .map(|(i, url)| {
                let cert = cert_der.to_vec();
                std::thread::spawn(move || {
                    (
                        i,
                        verify_server_url(&url, &cert, Duration::from_millis(800)),
                    )
                })
            })
            .collect();
        for h in handles {
            if let Ok((i, ok)) = h.join() {
                state[i].verified = ok;
            }
        }
        if state.iter().all(|c| c.verified) {
            break;
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    state
}

/// URLs for the QR, rank order preserved within each tier:
/// verified first, then unverified physical, then verified virtual.
/// Physical addresses are never silently dropped — a failed laptop
/// self-check (slow startup handshake, local firewall quirk) must not hide
/// the Wi-Fi address from the phone. Unverified virtual interfaces
/// (`virbr0`, `docker0`, …) are dropped: the phone can't reach them anyway.
pub fn urls_from_checks(checks: &[AddrCheck]) -> Vec<String> {
    let mut urls = Vec::new();
    for tier in 0..2 {
        for c in checks {
            let take = match tier {
                0 => c.verified,
                _ => !c.verified && c.physical,
            };
            if take {
                urls.push(c.url.clone());
            }
        }
    }
    urls
}

/// Extract the host (IP) part of a candidate URL for logging/dialing.
pub fn url_host(url: &str) -> &str {
    let rest = url
        .trim_start_matches("https://")
        .trim_start_matches("http://")
        .split('/')
        .next()
        .unwrap_or("");
    rest.split(':').next().unwrap_or(rest)
}

/// Cheap TCP reachability probe (no TLS): is something listening on 1313?
/// Used only to fail fast before the real TLS verification.
#[allow(dead_code)]
pub fn port_open(url: &str, timeout: Duration) -> bool {
    let addr: std::net::SocketAddr = match format!("{}:{PORT}", url_host(url)).parse() {
        Ok(a) => a,
        Err(_) => return false,
    };
    TcpStream::connect_timeout(&addr, timeout).is_ok()
}

/// Build the QR-code payload string. `urls` should already be verified.
/// Format: `<version>,<peyara>,<host>,sha256:<fingerprint>,pair:<code>,<url>,...`
pub fn qr_payload(
    version: &str,
    host: &str,
    fingerprint: &str,
    pairing: &str,
    urls: &[String],
) -> String {
    let mut parts = vec![
        version.to_owned(),
        QR_SECRET.to_owned(),
        host.to_owned(),
        format!("{FINGERPRINT_PREFIX}{fingerprint}"),
        format!("{PAIR_PREFIX}{pairing}"),
    ];
    parts.extend(urls.iter().cloned());
    parts.join(",")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn payload_format_matches_qr_spec() {
        let fp = "a".repeat(64);
        let p = qr_payload(
            "2.0.0",
            "mypc",
            &fp,
            "9f2ce812",
            &["https://192.168.1.5:1313/".to_owned()],
        );
        assert_eq!(
            p,
            format!("2.0.0,<peyara>,mypc,sha256:{fp},pair:9f2ce812,https://192.168.1.5:1313/")
        );
        assert_eq!(parse_fingerprint(&p).as_deref(), Some(fp.as_str()));
        assert_eq!(parse_pairing(&p).as_deref(), Some("9f2ce812"));
    }

    #[test]
    fn payload_with_no_urls_still_has_prefix() {
        let fp = "b".repeat(64);
        let p = qr_payload("2.0.0", "mypc", &fp, "00ff00ff", &[]);
        assert_eq!(p, format!("2.0.0,<peyara>,mypc,sha256:{fp},pair:00ff00ff"));
    }

    #[test]
    fn pairing_codes_are_8_lowercase_hex_and_unique() {
        let mut seen = std::collections::HashSet::new();
        for _ in 0..1000 {
            let c = generate_pairing().unwrap();
            assert_eq!(c.len(), 8);
            assert!(c.chars().all(|c| c.is_ascii_hexdigit()));
            assert_eq!(c, c.to_lowercase());
            assert!(seen.insert(c));
        }
    }

    #[test]
    fn parse_pairing_rejects_garbage() {
        assert_eq!(parse_pairing("2.0.0,<peyara>,mypc"), None);
        assert_eq!(parse_pairing("pair:xyz"), None);
        assert_eq!(parse_pairing("pair:ABCDEF12EXTRA"), None);
        assert_eq!(
            parse_pairing("2.0.0,<peyara>,mypc,sha256:aa,pair:abcdef12"),
            Some("abcdef12".to_owned())
        );
    }

    #[test]
    fn candidate_urls_are_https_on_port_1313() {
        let urls = candidate_urls(&[LanAddr {
            name: "wlo1".to_owned(),
            ip: "10.0.0.2".to_owned(),
        }]);
        assert_eq!(urls, vec!["https://10.0.0.2:1313/".to_owned()]);
    }

    #[test]
    fn virtual_interfaces_classified() {
        for v in [
            "virbr0", "docker0", "veth1234", "br-abc", "lxcbr0", "vmnet8", "tun0", "wg0",
        ] {
            assert!(is_virtual_iface(v), "{v}");
        }
        for p in ["wlo1", "eth0", "enp0s3", "lo"] {
            assert!(!is_virtual_iface(p), "{p}");
        }
    }

    #[test]
    fn ranking_prefers_default_then_physical_then_virtual() {
        let lan = |name: &str, ip: &str| LanAddr {
            name: name.to_owned(),
            ip: ip.to_owned(),
        };
        let ranked = rank_lan_addrs(
            vec![
                lan("virbr0", "192.168.122.1"),
                lan("eth0", "10.0.0.2"),
                lan("wlo1", "192.168.1.16"),
                lan("wlo1", "192.168.1.16"), // deduped
            ],
            Some("wlo1"),
        );
        let ips: Vec<&str> = ranked.iter().map(|a| a.ip.as_str()).collect();
        assert_eq!(ips, vec!["192.168.1.16", "10.0.0.2", "192.168.122.1"]);
    }

    #[test]
    fn default_route_table_parsing() {
        let table = "Iface\tDestination\tGateway\tFlags\n\
             wlo1\t00000000\t0101A8C0\t0003\n\
             virbr0\t007AA8C0\t00000000\t0001\n";
        assert_eq!(default_iface_from_table(table).as_deref(), Some("wlo1"));
        assert_eq!(default_iface_from_table("Iface\tDestination\n"), None);
    }

    #[test]
    fn qr_keeps_unverified_physical_but_drops_unverified_virtual() {
        let check = |name: &str, ip: &str, verified: bool| AddrCheck {
            addr: LanAddr {
                name: name.to_owned(),
                ip: ip.to_owned(),
            },
            url: format!("https://{ip}:1313/"),
            verified,
            physical: !is_virtual_iface(name),
        };
        let urls = urls_from_checks(&[
            check("virbr0", "192.168.122.1", true),
            check("wlo1", "192.168.1.16", false), // transient self-check miss: still advertised
            check("docker0", "172.17.0.1", false), // unverified virtual: dropped
        ]);
        assert_eq!(
            urls,
            vec![
                "https://192.168.122.1:1313/".to_owned(),
                "https://192.168.1.16:1313/".to_owned(),
            ]
        );
    }

    #[test]
    fn ws_url_derivation() {
        assert_eq!(
            ws_url("https://192.168.1.5:1313/"),
            "wss://192.168.1.5:1313/ws"
        );
        assert_eq!(url_host("https://192.168.1.5:1313/"), "192.168.1.5");
    }

    #[test]
    fn parse_fingerprint_rejects_garbage() {
        assert_eq!(parse_fingerprint("2.0.0,<peyara>,mypc"), None);
        assert_eq!(parse_fingerprint("sha256:xyz"), None);
        assert_eq!(parse_fingerprint("sha256:AAAA"), None);
    }

    #[test]
    fn verify_rejects_closed_port() {
        let fake_cert = vec![0u8; 32];
        assert!(!verify_server_url(
            "https://127.0.0.1:1313/",
            &fake_cert,
            Duration::from_millis(500)
        ));
    }
}
