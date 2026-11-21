//! Serving-side TLS for the API surface (TODO **T1.3a**, decision **Q47**).
//!
//! Opt-in only: without `--tls-san` the server binds the exact same h2c
//! listener as before (Q11 revised by Q47 — cleartext stays the DEFAULT;
//! TLS is additive). With `--tls-san`, [`generate_self_signed`] mints a
//! throwaway CA + a serving certificate whose SAN list is the flag values
//! (IPs and DNS names auto-detected) plus the conventional `localhost`
//! entries, and [`acceptor`] turns the PEM pair into a tokio-rustls
//! acceptor for the axum serve branch.
//!
//! Single rustls stack for the control plane (Q47 LOCKED): direct rustls
//! dependency with the ring CryptoProvider — the same provider openrusty
//! pins — so no dual-provider process panic is possible.

use std::net::IpAddr;
use std::sync::Arc;

use rcgen::{
    BasicConstraints, CertificateParams, DistinguishedName, DnType, ExtendedKeyUsagePurpose, IsCa,
    KeyPair, KeyUsagePurpose, SanType,
};

/// PEM material for one serving setup: leaf cert + key + the CA cert that
/// signed it (clients trust `ca_pem`; k3s writes the same bytes to
/// `<data-dir>/server/tls/client-ca.crt`).
pub struct TlsMaterial {
    pub cert_pem: String,
    pub key_pem: String,
    pub ca_pem: String,
}

/// The configured acceptor handed to [`crate::serve`].
pub struct TlsAcceptor {
    inner: tokio_rustls::TlsAcceptor,
}

impl TlsAcceptor {
    pub(crate) fn inner(&self) -> tokio_rustls::TlsAcceptor {
        self.inner.clone()
    }
}

/// Mint a self-signed CA + a serving cert covering `extra_sans`
/// (each entry an IP or a DNS name) plus `localhost`/`127.0.0.1`.
///
/// Ten-year validity, single CN (`init-pro-ca` / `init-pro-serving`):
/// the supported-subset posture (Q59) — no rotation, no renewal
/// (that is T3.3 bootstrap/rotation territory).
pub fn generate_self_signed(extra_sans: &[String]) -> std::io::Result<TlsMaterial> {
    let ca_key = KeyPair::generate().map_err(io_err)?;
    let mut ca_params = CertificateParams::new(Vec::new()).map_err(io_err)?;
    let mut ca_dn = DistinguishedName::new();
    ca_dn.push(DnType::CommonName, "init-pro-ca");
    ca_params.distinguished_name = ca_dn;
    ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    ca_params.key_usages = vec![
        KeyUsagePurpose::DigitalSignature,
        KeyUsagePurpose::KeyCertSign,
        KeyUsagePurpose::CrlSign,
    ];
    let ca_cert = ca_params.self_signed(&ca_key).map_err(io_err)?;

    let leaf_key = KeyPair::generate().map_err(io_err)?;
    let mut sans: Vec<SanType> = vec![
        SanType::DnsName(rcgen::Ia5String::try_from("localhost").expect("ascii dns name")),
        SanType::IpAddress("127.0.0.1".parse::<IpAddr>().expect("literal ip")),
    ];
    for entry in extra_sans {
        match entry.trim().parse::<IpAddr>() {
            Ok(ip) => sans.push(SanType::IpAddress(ip)),
            Err(_) => sans.push(SanType::DnsName(
                rcgen::Ia5String::try_from(entry.trim()).map_err(io_err)?,
            )),
        }
    }
    let mut leaf_params = CertificateParams::new(Vec::new()).map_err(io_err)?;
    let mut leaf_dn = DistinguishedName::new();
    leaf_dn.push(DnType::CommonName, "init-pro-serving");
    leaf_params.distinguished_name = leaf_dn;
    leaf_params.subject_alt_names = sans;
    leaf_params.is_ca = IsCa::ExplicitNoCa;
    leaf_params.key_usages = vec![
        KeyUsagePurpose::DigitalSignature,
        KeyUsagePurpose::KeyEncipherment,
    ];
    leaf_params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
    let leaf_cert = leaf_params
        .signed_by(&leaf_key, &ca_cert, &ca_key)
        .map_err(io_err)?;

    Ok(TlsMaterial {
        cert_pem: leaf_cert.pem(),
        key_pem: leaf_key.serialize_pem(),
        ca_pem: ca_cert.pem(),
    })
}

/// Build the serving acceptor from a PEM cert/key pair (ring provider,
/// TLS 1.2+1.3, no client-auth — bearer tokens are the T1.3a authn face).
pub fn acceptor(material: &TlsMaterial) -> std::io::Result<TlsAcceptor> {
    let certs: Vec<_> = rustls_pemfile::certs(&mut material.cert_pem.as_bytes())
        .collect::<Result<_, _>>()
        .map_err(|e| std::io::Error::other(format!("parse serving cert PEM: {e}")))?;
    let key = rustls_pemfile::private_key(&mut material.key_pem.as_bytes())
        .map_err(|e| std::io::Error::other(format!("parse serving key PEM: {e}")))?
        .ok_or_else(|| std::io::Error::other("serving key PEM contains no key"))?;
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let config = rustls::ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|e| std::io::Error::other(format!("tls protocol versions: {e}")))?
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .map_err(|e| std::io::Error::other(format!("tls serving cert: {e}")))?;
    Ok(TlsAcceptor {
        inner: tokio_rustls::TlsAcceptor::from(Arc::new(config)),
    })
}

fn io_err(e: impl std::fmt::Display) -> std::io::Error {
    std::io::Error::other(format!("tls material generation: {e}"))
}

/// axum [`Listener`](axum::serve::Listener) adapter for the TLS branch of
/// [`crate::serve`]: TCP accept -> TLS accept, logging and retrying errors
/// (the trait contract requires `accept` to never return `Err`).
pub struct TlsListener {
    tcp: tokio::net::TcpListener,
    tls: tokio_rustls::TlsAcceptor,
}

impl TlsListener {
    pub(crate) fn new(tcp: tokio::net::TcpListener, acceptor: TlsAcceptor) -> Self {
        Self {
            tcp,
            tls: acceptor.inner(),
        }
    }
}

impl axum::serve::Listener for TlsListener {
    type Io = tokio_rustls::server::TlsStream<tokio::net::TcpStream>;
    type Addr = std::net::SocketAddr;

    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        loop {
            match self.tcp.accept().await {
                Ok((stream, addr)) => match self.tls.accept(stream).await {
                    Ok(tls_stream) => return (tls_stream, addr),
                    // Failed handshakes (client hung up, bad ClientHello)
                    // drop the connection and keep accepting.
                    Err(e) => {
                        tracing::debug!(target: "init-pro", error = %e, "tls handshake failed");
                    }
                },
                Err(e) => {
                    tracing::warn!(target: "init-pro", error = %e, "tcp accept error");
                    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                }
            }
        }
    }

    fn local_addr(&self) -> std::io::Result<Self::Addr> {
        self.tcp.local_addr()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_pair_builds_an_acceptor() {
        let material = generate_self_signed(&["k3as-server".to_string()]).expect("material");
        assert!(material.cert_pem.contains("BEGIN CERTIFICATE"));
        assert!(material.key_pem.contains("BEGIN PRIVATE KEY"));
        assert!(material.ca_pem.contains("BEGIN CERTIFICATE"));
        acceptor(&material).expect("acceptor builds");
    }

    #[test]
    fn sans_deduplicate_literal_entries() {
        // Duplicate entries (flag value == built-in localhost) must not
        // panic rcgen — SAN lists may repeat; the acceptor still builds.
        let material =
            generate_self_signed(&["localhost".to_string()]).expect("material with dup SAN");
        acceptor(&material).expect("acceptor builds");
    }
}
