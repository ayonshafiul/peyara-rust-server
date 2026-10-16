//! Persistent TLS identity for the Peyara server.
//!
//! The first launch mints a self-signed certificate + pairing code and saves
//! them to the OS config dir; later launches reuse them, so phones stay
//! paired across restarts. Rotation happens only on demand (Regenerate
//! button), on network-driven regeneration (new IPs need new SANs), or when
//! the stored identity is missing, corrupt, stale, or doesn't cover the
//! current addresses.
//!
//! The phone never trusts the OS CA store for this connection: it pins the
//! certificate by the SHA-256 fingerprint carried inside the scanned QR code
//! (see `docs/qr-code-format.md`).
//!
//! Everything is TLS-only: `https://<lan-ip>:1313/` for discovery and
//! `wss://<lan-ip>:1313/ws` for the input protocol. There is no plaintext
//! fallback port.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

/// In-memory server identity: DER certificate + key plus the fingerprint
/// the QR code advertises.
#[derive(Debug, Clone)]
pub struct CertBundle {
    pub cert_der: Vec<u8>,
    pub key_der: Vec<u8>,
    /// Lowercase hex SHA-256 of `cert_der` (64 chars).
    pub fingerprint: String,
}

/// Generate a fresh self-signed certificate (ECDSA P-256) covering
/// `localhost`, loopback and every address in `sans` (our LAN IPs).
pub fn generate_cert(sans: &[String]) -> Result<CertBundle, String> {
    let mut names = vec![
        "localhost".to_owned(),
        "127.0.0.1".to_owned(),
        "::1".to_owned(),
    ];
    for s in sans {
        if !names.contains(s) {
            names.push(s.clone());
        }
    }
    let key = rcgen::generate_simple_self_signed(names)
        .map_err(|e| format!("certificate generation failed: {e}"))?;
    let cert_der = key.cert.der().to_vec();
    let key_der = key.key_pair.serialize_der();
    let fingerprint = hex::encode(Sha256::digest(&cert_der));
    Ok(CertBundle {
        cert_der,
        key_der,
        fingerprint,
    })
}

/// Server identity: TLS certificate plus the pairing code phones echo.
#[derive(Debug, Clone)]
pub struct StoredIdentity {
    pub bundle: CertBundle,
    pub pairing: String,
}

/// Force rotation once the stored identity passes this age, so expiry can
/// never sneak up (rcgen certificates carry a finite validity window).
const MAX_IDENTITY_AGE_SECS: u64 = 300 * 24 * 3600; // ~10 months

#[derive(Debug, Serialize, Deserialize)]
struct IdentityFile {
    v: u32,
    cert_hex: String,
    key_hex: String,
    pairing: String,
    created_unix: u64,
    sans: Vec<String>,
}

/// Path of the persisted identity, e.g. `~/.config/peyara-egui/identity.json`.
pub fn identity_path() -> Option<PathBuf> {
    dirs::config_dir().map(|d| d.join("peyara-egui").join("identity.json"))
}

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Pure check: is this stored identity still usable for `current_sans`?
fn identity_usable(file: &IdentityFile, current_sans: &[String], now: u64) -> bool {
    if file.v != 1 {
        return false;
    }
    if file.cert_hex.is_empty() || file.key_hex.is_empty() {
        return false;
    }
    if file.pairing.len() != 8 || !file.pairing.chars().all(|c| c.is_ascii_hexdigit()) {
        return false;
    }
    if now.saturating_sub(file.created_unix) > MAX_IDENTITY_AGE_SECS {
        return false;
    }
    // Every current address must be covered by the stored cert's SANs.
    if !current_sans
        .iter()
        .all(|ip| file.sans.iter().any(|s| s == ip))
    {
        return false;
    }
    hex::decode(&file.cert_hex).is_ok() && hex::decode(&file.key_hex).is_ok()
}

/// Load the persisted identity, or `None` when it is missing, corrupt,
/// stale, or doesn't cover the current addresses (caller generates fresh).
pub fn load_identity(current_sans: &[String]) -> Option<StoredIdentity> {
    load_identity_from(&identity_path()?, current_sans, now_unix())
}

fn load_identity_from(
    path: &std::path::Path,
    current_sans: &[String],
    now: u64,
) -> Option<StoredIdentity> {
    let text = std::fs::read_to_string(path).ok()?;
    let file: IdentityFile = serde_json::from_str(&text).ok()?;
    if !identity_usable(&file, current_sans, now) {
        return None;
    }
    let cert_der = hex::decode(&file.cert_hex).ok()?;
    let key_der = hex::decode(&file.key_hex).ok()?;
    let fingerprint = hex::encode(Sha256::digest(&cert_der));
    Some(StoredIdentity {
        bundle: CertBundle {
            cert_der,
            key_der,
            fingerprint,
        },
        pairing: file.pairing.to_lowercase(),
    })
}

/// Persist the identity (directory `0700`, file `0600` on Unix).
pub fn save_identity(bundle: &CertBundle, pairing: &str, sans: &[String]) -> Result<(), String> {
    let path = identity_path().ok_or_else(|| "no config dir".to_owned())?;
    save_identity_to(&path, bundle, pairing, sans, now_unix())
}

fn save_identity_to(
    path: &std::path::Path,
    bundle: &CertBundle,
    pairing: &str,
    sans: &[String],
    now: u64,
) -> Result<(), String> {
    if let Some(dir) = path.parent() {
        #[cfg(unix)]
        {
            use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
            std::fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(dir)
                .map_err(|e| format!("identity dir: {e}"))?;
            let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
        }
        #[cfg(not(unix))]
        {
            std::fs::create_dir_all(dir).map_err(|e| format!("identity dir: {e}"))?;
        }
    }
    let file = IdentityFile {
        v: 1,
        cert_hex: hex::encode(&bundle.cert_der),
        key_hex: hex::encode(&bundle.key_der),
        pairing: pairing.to_owned(),
        created_unix: now,
        sans: sans.to_vec(),
    };
    let text = serde_json::to_string(&file).map_err(|e| format!("identity encode: {e}"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
        let mut opts = std::fs::OpenOptions::new();
        std::io::Write::write_all(
            &mut opts
                .write(true)
                .create(true)
                .truncate(true)
                .mode(0o600)
                .open(path)
                .map_err(|e| format!("identity write: {e}"))?,
            text.as_bytes(),
        )
        .map_err(|e| format!("identity write: {e}"))?;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
            .map_err(|e| format!("identity perms: {e}"))?;
        Ok(())
    }
    #[cfg(not(unix))]
    {
        std::fs::write(path, text).map_err(|e| format!("identity write: {e}"))?;
        Ok(())
    }
}

/// Serve `app` forever over TLS on `0.0.0.0:1313`.
///
/// Retries the bind briefly: when this follows a regeneration the previous
/// listener may still be releasing the port.
pub async fn serve_tls(app: axum::Router, bundle: CertBundle) -> std::io::Result<()> {
    let config = axum_server::tls_rustls::RustlsConfig::from_der(
        vec![bundle.cert_der.clone()],
        bundle.key_der.clone(),
    )
    .await?;
    let addr = SocketAddr::from(([0, 0, 0, 0], crate::netinfo::PORT));
    let mut last_err = std::io::Error::new(std::io::ErrorKind::AddrInUse, "bind retries exhausted");
    for _ in 0..50 {
        match std::net::TcpListener::bind(addr) {
            Ok(listener) => {
                listener.set_nonblocking(true)?;
                return axum_server::from_tcp_rustls(listener, config)
                    .serve(app.into_make_service())
                    .await;
            }
            Err(e) => {
                last_err = e;
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }
    }
    Err(last_err)
}

/// Blocking self-check used at startup (and after regeneration): TLS-connect
/// to `url` trusting exactly `cert_der`, `GET /`, and confirm the body
/// contains the `"peyara"` discovery marker.
pub fn verify_server_tls(url: &str, cert_der: &[u8], timeout: Duration) -> bool {
    let cert_der = cert_der.to_vec();
    let url = url.to_owned();
    // Run the async handshake on a short-lived throwaway runtime so callers
    // (egui button handlers included) stay synchronous.
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        rt.block_on(verify_inner(&url, &cert_der, timeout))
    })
    .join()
    .unwrap_or(Ok(false))
    .unwrap_or(false)
}

async fn verify_inner(url: &str, cert_der: &[u8], timeout: Duration) -> std::io::Result<bool> {
    let host = url
        .trim_start_matches("https://")
        .trim_start_matches("http://")
        .split('/')
        .next()
        .unwrap_or("");
    let host = host.split(':').next().unwrap_or(host);
    let ip: IpAddr = host.parse().map_err(|_| {
        std::io::Error::new(std::io::ErrorKind::InvalidInput, "qr url is not an ip")
    })?;
    let addr = SocketAddr::new(ip, crate::netinfo::PORT);

    let handshake = async {
        let mut roots = rustls::RootCertStore::empty();
        roots
            .add(rustls::pki_types::CertificateDer::from(cert_der.to_vec()))
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;
        let config = rustls::ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth();
        let connector = tokio_rustls::TlsConnector::from(Arc::new(config));
        let stream = tokio::net::TcpStream::connect(addr).await?;
        let name = rustls::pki_types::ServerName::IpAddress(ip.into());
        let mut tls = connector.connect(name, stream).await?;
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        tls.write_all(
            format!("GET / HTTP/1.0\r\nHost: {host}\r\nConnection: close\r\n\r\n").as_bytes(),
        )
        .await?;
        let mut body = Vec::new();
        tls.read_to_end(&mut body).await?;
        Ok::<_, std::io::Error>(
            String::from_utf8_lossy(&body).contains(crate::netinfo::SERVER_REST_RESPONSE),
        )
    };
    tokio::time::timeout(timeout, handshake)
        .await
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::TimedOut, "tls verify timed out"))?
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fingerprint_is_64_lowercase_hex() {
        let b = generate_cert(&["192.168.1.5".to_owned()]).unwrap();
        assert_eq!(b.fingerprint.len(), 64);
        assert!(b.fingerprint.chars().all(|c| c.is_ascii_hexdigit()));
        assert_eq!(b.fingerprint, b.fingerprint.to_lowercase());
        assert!(!b.cert_der.is_empty());
        assert!(!b.key_der.is_empty());
    }

    #[test]
    fn every_generation_is_unique() {
        let a = generate_cert(&[]).unwrap();
        let b = generate_cert(&[]).unwrap();
        assert_ne!(a.fingerprint, b.fingerprint);
        assert_ne!(a.cert_der, b.cert_der);
    }

    #[test]
    fn fingerprint_matches_cert_bytes() {
        let b = generate_cert(&[]).unwrap();
        assert_eq!(hex::encode(Sha256::digest(&b.cert_der)), b.fingerprint);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn tls_serves_discovery_with_pinned_cert() {
        // End-to-end: serve a trivial app over TLS, verify it the same way
        // the phone/startup check does (trust exactly this cert).
        let bundle = generate_cert(&[]).unwrap();
        let app = axum::Router::new().route("/", axum::routing::get(|| async { "\"peyara\"" }));
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let port = listener.local_addr().unwrap().port();
        let config = axum_server::tls_rustls::RustlsConfig::from_der(
            vec![bundle.cert_der.clone()],
            bundle.key_der.clone(),
        )
        .await
        .unwrap();
        tokio::spawn(async move {
            axum_server::from_tcp_rustls(listener, config)
                .serve(app.into_make_service())
                .await
        });
        // Give the listener a moment to start.
        tokio::time::sleep(Duration::from_millis(200)).await;
        let url = format!("https://127.0.0.1:{port}/");
        // Port differs from PORT, so check via a direct handshake on that port.
        let ok = tokio::time::timeout(Duration::from_secs(5), async {
            let mut roots = rustls::RootCertStore::empty();
            roots
                .add(rustls::pki_types::CertificateDer::from(
                    bundle.cert_der.clone(),
                ))
                .unwrap();
            let config = rustls::ClientConfig::builder()
                .with_root_certificates(roots)
                .with_no_client_auth();
            let connector = tokio_rustls::TlsConnector::from(Arc::new(config));
            let stream = tokio::net::TcpStream::connect(("127.0.0.1", port))
                .await
                .unwrap();
            let name = rustls::pki_types::ServerName::IpAddress(
                std::net::IpAddr::from([127, 0, 0, 1]).into(),
            );
            let mut tls = connector.connect(name, stream).await.unwrap();
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            tls.write_all(b"GET / HTTP/1.0\r\nHost: x\r\nConnection: close\r\n\r\n")
                .await
                .unwrap();
            let mut body = Vec::new();
            tls.read_to_end(&mut body).await.unwrap();
            String::from_utf8_lossy(&body).contains("\"peyara\"")
        })
        .await
        .unwrap();
        assert!(ok, "tls discovery failed for {url}");
    }

    fn test_identity_file() -> IdentityFile {
        IdentityFile {
            v: 1,
            cert_hex: "aabb".to_owned(),
            key_hex: "ccdd".to_owned(),
            pairing: "9f2ce812".to_owned(),
            created_unix: 1_700_000_000,
            sans: vec!["192.168.1.16".to_owned()],
        }
    }

    #[test]
    fn identity_usability_rules() {
        let now = 1_700_000_000 + 60;
        let sans = vec!["192.168.1.16".to_owned()];
        assert!(identity_usable(&test_identity_file(), &sans, now));
        // New address not covered by stored SANs.
        assert!(!identity_usable(
            &test_identity_file(),
            &["192.168.1.16".to_owned(), "10.0.0.2".to_owned()],
            now
        ));
        // Subset is fine.
        assert!(identity_usable(&test_identity_file(), &[], now));
        // Stale, wrong version, bad pairing, bad hex.
        assert!(!identity_usable(
            &test_identity_file(),
            &sans,
            1_700_000_000 + MAX_IDENTITY_AGE_SECS + 1
        ));
        let mut f = test_identity_file();
        f.v = 2;
        assert!(!identity_usable(&f, &sans, now));
        let mut f = test_identity_file();
        f.pairing = "xyz".to_owned();
        assert!(!identity_usable(&f, &sans, now));
        let mut f = test_identity_file();
        f.cert_hex = "zz".to_owned();
        assert!(!identity_usable(&f, &sans, now));
    }

    #[test]
    fn identity_save_load_roundtrip() {
        let dir = std::env::temp_dir().join(format!("peyara-ident-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("identity.json");
        let bundle = generate_cert(&["192.168.1.16".to_owned()]).unwrap();
        let sans = vec!["192.168.1.16".to_owned()];
        save_identity_to(&path, &bundle, "9f2ce812", &sans, 1_700_000_000).unwrap();
        let loaded = load_identity_from(&path, &sans, 1_700_000_000 + 60).expect("roundtrip load");
        assert_eq!(loaded.bundle.fingerprint, bundle.fingerprint);
        assert_eq!(loaded.pairing, "9f2ce812");
        // New address forces regeneration path (load refuses).
        assert!(load_identity_from(
            &path,
            &["192.168.1.16".to_owned(), "10.0.0.2".to_owned()],
            1_700_000_000 + 60
        )
        .is_none());
        // Garbage file refuses too.
        std::fs::write(&path, "not json").unwrap();
        assert!(load_identity_from(&path, &sans, 1_700_000_000 + 60).is_none());
        let _ = std::fs::remove_dir_all(&dir);

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            // Permissions re-checked on a fresh write.
            let dir2 =
                std::env::temp_dir().join(format!("peyara-ident-perm-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir2);
            let path2 = dir2.join("identity.json");
            save_identity_to(&path2, &bundle, "9f2ce812", &sans, 1_700_000_000).unwrap();
            let mode = std::fs::metadata(&path2).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "identity file must be owner-only");
            let _ = std::fs::remove_dir_all(&dir2);
        }
    }
}
