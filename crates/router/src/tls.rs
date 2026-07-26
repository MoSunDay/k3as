//! TLS termination config: builds a rustls `ServerConfig` with an SNI cert
//! resolver (T5.4 Scope B).
//!
//! Cert/key material is **injected** from configuration — no secrets are
//! hardcoded (R5). The resolver maps the TLS SNI host to a pre-loaded
//! (cert chain, key) pair. This is the data-plane equivalent of openresty's
//! `ssl_certificate_by_lua` SNI hook for M1: dynamic, Lua-driven cert issuance
//! (calling back into the VM mid-handshake) is the deferred openresty path —
//! rustls's `ResolvesServerCert` is exactly the synchronous SNI selection point.
//!
//! The `ring` crypto provider is used (ADR **Q16**): smaller, widely audited,
//! consistent with the project's minimal-dependency posture.

use std::collections::HashMap;
use std::sync::Arc;

use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use rustls::server::{ClientHello, ResolvesServerCert, ServerConfig};
use rustls::sign::CertifiedKey;

/// One (cert chain, private key) pair, loaded from PEM config material.
///
/// Both fields hold raw PEM bytes so the source can be a kube `Secret`
/// (`tls.crt` / `tls.key`) or a file — the caller decides, never this module.
#[derive(Clone, Debug)]
pub struct CertKey {
    /// PEM-encoded certificate chain (leaf first, then intermediates).
    pub cert_pem: Vec<u8>,
    /// PEM-encoded PKCS#8 / PKCS#1 / SEC1 private key.
    pub key_pem: Vec<u8>,
}

impl CertKey {
    /// Construct from PEM byte slices.
    pub fn pem(cert_pem: impl Into<Vec<u8>>, key_pem: impl Into<Vec<u8>>) -> Self {
        Self { cert_pem: cert_pem.into(), key_pem: key_pem.into() }
    }
}

/// Parse PEM cert + key bytes into rustls DER types.
fn parse_pem(
    cert_pem: &[u8],
    key_pem: &[u8],
) -> std::io::Result<(Vec<CertificateDer<'static>>, PrivateKeyDer<'static>)> {
    let certs: Vec<_> = rustls_pemfile::certs(&mut &cert_pem[..])
        .collect::<Result<_, _>>()
        .map_err(io_err("parsing certificate PEM"))?;
    if certs.is_empty() {
        return Err(io_err_raw("no certificate found in PEM"));
    }
    let key = rustls_pemfile::private_key(&mut &key_pem[..])
        .map_err(io_err("parsing private key PEM"))?
        .ok_or_else(|| io_err_raw("no private key found in PEM"))?;
    Ok((certs, key))
}

/// Build a `CertifiedKey` (the rustls cert+key bundle) from PEM material.
pub fn certified_key(ck: &CertKey) -> std::io::Result<CertifiedKey> {
    let (certs, key) = parse_pem(&ck.cert_pem, &ck.key_pem)?;
    let signing = rustls::crypto::ring::sign::any_supported_type(&key)
        .map_err(io_err("unsupported/invalid private key"))?;
    Ok(CertifiedKey::new(certs, signing))
}

/// SNI-aware cert resolver: maps the ClientHello SNI to a `CertifiedKey`.
///
/// An empty host string registers a default (used when the client sends no SNI
/// or an unknown one). Hosts are matched case-insensitively (DNS is).
#[derive(Debug)]
pub struct SniCertResolver {
    by_sni: HashMap<String, Arc<CertifiedKey>>,
    default: Option<Arc<CertifiedKey>>,
}

impl SniCertResolver {
    /// Build from `(sni_host, CertKey)` pairs.
    pub fn new(entries: &[(String, CertKey)]) -> std::io::Result<Self> {
        let mut by_sni = HashMap::new();
        let mut default = None;
        for (host, ck) in entries {
            let key = Arc::new(certified_key(ck)?);
            if host.is_empty() {
                default = Some(key);
            } else {
                by_sni.insert(host.to_ascii_lowercase(), key);
            }
        }
        Ok(Self { by_sni, default })
    }
}

impl ResolvesServerCert for SniCertResolver {
    fn resolve(&self, client_hello: ClientHello) -> Option<Arc<CertifiedKey>> {
        let name = client_hello.server_name().map(str::to_ascii_lowercase);
        match name {
            Some(n) => self.by_sni.get(&n).cloned().or_else(|| self.default.clone()),
            None => self.default.clone(),
        }
    }
}

/// Build a TLS server config from `(sni_host, CertKey)` pairs, using the
/// `ring` provider. The result wraps an [`Arc`]`<`[`ServerConfig`]`>` ready for
/// `tokio_rustls::TlsAcceptor::from`.
pub fn build_server_config(
    entries: &[(String, CertKey)],
) -> std::io::Result<Arc<ServerConfig>> {
    let resolver = Arc::new(SniCertResolver::new(entries)?);
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let mut cfg = ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(io_err("selecting TLS protocol versions"))?
        .with_no_client_auth()
        .with_cert_resolver(resolver);
    // Advertise HTTP/1.1 only (the data plane is HTTP/1.1; ALPN keeps clients
    // honest and avoids accidental h2 negotiation).
    cfg.alpn_protocols = vec![b"http/1.1".to_vec()];
    Ok(Arc::new(cfg))
}

fn io_err_raw(msg: impl Into<String>) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidData, msg.into())
}

fn io_err<E: std::fmt::Display>(ctx: &str) -> impl Fn(E) -> std::io::Error + '_ {
    move |e: E| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("{ctx}: {e}"),
        )
    }
}

// ============================ client (outbound TLS) ============================

/// Build a rustls **client** config. `verify=true` uses the Mozilla root store
/// (`webpki-roots`); `verify=false` skips verification (the self-signed /
/// internal-CA path openresty's `sslhandshake` exposes for tests and meshes).
///
/// Shared by cosocket `sslhandshake` (T5.3 Scope B) and `resty.http`.
pub(crate) fn build_client_config(verify: bool) -> std::io::Result<rustls::ClientConfig> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let builder = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(io_err("selecting TLS protocol versions"))?;
    if verify {
        let mut roots = rustls::RootCertStore::empty();
        roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        Ok(builder.with_root_certificates(roots).with_no_client_auth())
    } else {
        Ok(builder
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(NoVerify))
            .with_no_client_auth())
    }
}

/// A cert verifier that accepts anything (the `verify=false` path). Only ever
/// constructed here for `verify=false` configs.
#[derive(Debug)]
struct NoVerify;

impl rustls::client::danger::ServerCertVerifier for NoVerify {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &rustls::pki_types::ServerName<'_>,
        _ocsp: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }
    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _signed: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }
    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _signed: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }
    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        rustls::crypto::ring::default_provider()
            .signature_verification_algorithms
            .supported_schemes()
    }
}
