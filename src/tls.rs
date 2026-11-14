//! Self-signed TLS identity.
//!
//! LocalSend identifies HTTPS peers by the SHA-256 fingerprint of their
//! self-signed certificate. The certificate doubles as our client
//! certificate because current LocalSend servers request one from senders.

use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName, UnixTime};
use rustls::{DigitallySignedStruct, SignatureScheme};
use sha2::{Digest, Sha256};
use std::fs;
use std::path::PathBuf;
use std::sync::Arc;

#[derive(Debug, Clone)]
pub struct Identity {
    pub cert_der: Vec<u8>,
    pub key_der: Vec<u8>,
    pub fingerprint: String,
}

fn identity_paths() -> (PathBuf, PathBuf) {
    let dir = crate::config::config_dir();
    (dir.join("server_cert.der"), dir.join("server_key.der"))
}

pub fn load_or_create() -> Result<Identity, String> {
    let (cert_path, key_path) = identity_paths();
    if let Some(parent) = cert_path.parent() {
        fs::create_dir_all(parent)
            .map_err(|e| format!("Could not create {}: {e}", parent.display()))?;
    }

    if let (Ok(cert), Ok(key)) = (fs::read(&cert_path), fs::read(&key_path)) {
        if !cert.is_empty() && !key.is_empty() {
            return Ok(Identity {
                fingerprint: sha256_hex(&cert),
                cert_der: cert,
                key_der: key,
            });
        }
    }

    let certified = rcgen::generate_simple_self_signed(vec!["localsend.local".to_string()])
        .map_err(|e| format!("Could not generate certificate: {e}"))?;
    let cert_der = certified.cert.der().to_vec();
    let key_der = certified.key_pair.serialize_der();

    fs::write(&cert_path, &cert_der)
        .map_err(|e| format!("Could not write {}: {e}", cert_path.display()))?;
    fs::write(&key_path, &key_der)
        .map_err(|e| format!("Could not write {}: {e}", key_path.display()))?;
    restrict_permissions(&key_path);

    Ok(Identity {
        fingerprint: sha256_hex(&cert_der),
        cert_der,
        key_der,
    })
}

fn restrict_permissions(path: &std::path::Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o600));
    }
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn sha256_bytes(bytes: &[u8]) -> [u8; 32] {
    let digest = Sha256::digest(bytes);
    let mut out = [0u8; 32];
    out.copy_from_slice(&digest);
    out
}

fn hex_decode(value: &str) -> Option<Vec<u8>> {
    let value = value.trim();
    if value.len() != 64 {
        return None;
    }
    let mut out = Vec::with_capacity(32);
    for chunk in value.as_bytes().chunks(2) {
        let text = std::str::from_utf8(chunk).ok()?;
        out.push(u8::from_str_radix(text, 16).ok()?);
    }
    Some(out)
}

pub fn server_config(identity: &Identity) -> Result<rustls::ServerConfig, String> {
    let certs = vec![CertificateDer::from(identity.cert_der.clone())];
    let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(identity.key_der.clone()));
    rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .map_err(|e| format!("Could not build TLS server configuration: {e}"))
}

pub fn client_config(
    identity: &Identity,
    expected_fingerprint: &str,
) -> Result<rustls::ClientConfig, String> {
    let expected = hex_decode(expected_fingerprint).ok_or_else(|| {
        "The peer did not provide a valid SHA-256 certificate fingerprint".to_string()
    })?;
    let mut expected_bytes = [0u8; 32];
    expected_bytes.copy_from_slice(&expected);

    let verifier = Arc::new(FingerprintVerifier {
        expected: expected_bytes,
    });
    let cert = CertificateDer::from(identity.cert_der.clone());
    let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(identity.key_der.clone()));

    rustls::ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(verifier)
        .with_client_auth_cert(vec![cert], key)
        .map_err(|e| format!("Could not build TLS client configuration: {e}"))
}

#[derive(Debug)]
struct FingerprintVerifier {
    expected: [u8; 32],
}

impl ServerCertVerifier for FingerprintVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        let actual = sha256_bytes(end_entity.as_ref());
        if actual == self.expected {
            Ok(ServerCertVerified::assertion())
        } else {
            Err(rustls::Error::General(
                "LocalSend certificate fingerprint mismatch".to_string(),
            ))
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &rustls::crypto::ring::default_provider().signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &rustls::crypto::ring::default_provider().signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        rustls::crypto::ring::default_provider()
            .signature_verification_algorithms
            .supported_schemes()
    }
}
