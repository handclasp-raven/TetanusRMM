//! Development certificate authority: a CA and a server certificate.
//!
//! The server uses the CA key to issue per-agent certificates at enrollment.
//! [`generate`] also produces one agent certificate, for tests that run
//! without enrollment; it is not written to disk.

use std::fs;
use std::path::Path;

use rcgen::{
    BasicConstraints, CertificateParams, CertifiedIssuer, DistinguishedName, DnType,
    ExtendedKeyUsagePurpose, IsCa, KeyPair, KeyUsagePurpose,
};

use crate::tls::{certs_from_pem, Identity, TlsError};

pub const CA_CERT: &str = "ca.crt";
pub const CA_KEY: &str = "ca.key";
pub const SERVER_CERT: &str = "server.crt";
pub const SERVER_KEY: &str = "server.key";

/// Names the dev server certificate is valid for.
pub const SERVER_NAMES: &[&str] = &["localhost", "127.0.0.1", "::1"];

/// PEM-encoded output of [`generate`].
#[derive(Debug, Clone)]
pub struct DevCerts {
    pub ca_cert: String,
    pub ca_key: String,
    pub server_cert: String,
    pub server_key: String,
    pub agent_cert: String,
    pub agent_key: String,
}

fn distinguished_name(common_name: &str) -> DistinguishedName {
    let mut dn = DistinguishedName::new();
    dn.push(DnType::OrganizationName, "RMM Dev");
    dn.push(DnType::CommonName, common_name);
    dn
}

/// Generate a fresh CA and sign a server certificate and one agent client
/// certificate (with `agent_id` as its common name) from it.
pub fn generate(agent_id: &str) -> Result<DevCerts, TlsError> {
    generate_with_names(agent_id, &[])
}

/// Like [`generate`], with extra DNS names or IP addresses on the server
/// certificate beyond [`SERVER_NAMES`] (e.g. the host's LAN address, so
/// agents on other machines can verify it).
pub fn generate_with_names(agent_id: &str, extra_names: &[String]) -> Result<DevCerts, TlsError> {
    let mut ca_params = CertificateParams::new(Vec::<String>::new())?;
    ca_params.distinguished_name = distinguished_name("RMM Dev CA");
    ca_params.is_ca = IsCa::Ca(BasicConstraints::Constrained(0));
    ca_params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
    let ca_key = KeyPair::generate()?;
    let ca_key_pem = ca_key.serialize_pem();
    let ca = CertifiedIssuer::self_signed(ca_params, ca_key)?;

    let mut server_names: Vec<String> = SERVER_NAMES.iter().map(|s| s.to_string()).collect();
    server_names.extend(extra_names.iter().cloned());
    let mut server_params = CertificateParams::new(server_names)?;
    server_params.distinguished_name = distinguished_name("localhost");
    server_params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
    server_params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
    let server_key = KeyPair::generate()?;
    let server_cert = server_params.signed_by(&server_key, &ca)?;

    let mut agent_params = CertificateParams::new(Vec::<String>::new())?;
    agent_params.distinguished_name = distinguished_name(agent_id);
    agent_params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
    agent_params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ClientAuth];
    let agent_key = KeyPair::generate()?;
    let agent_cert = agent_params.signed_by(&agent_key, &ca)?;

    Ok(DevCerts {
        ca_cert: ca.pem(),
        ca_key: ca_key_pem,
        server_cert: server_cert.pem(),
        server_key: server_key.serialize_pem(),
        agent_cert: agent_cert.pem(),
        agent_key: agent_key.serialize_pem(),
    })
}

impl DevCerts {
    pub fn ca(&self) -> Result<Vec<rustls_pki_types::CertificateDer<'static>>, TlsError> {
        certs_from_pem(&self.ca_cert)
    }

    pub fn server_identity(&self) -> Result<Identity, TlsError> {
        Identity::from_pem(&self.server_cert, &self.server_key)
    }

    pub fn agent_identity(&self) -> Result<Identity, TlsError> {
        Identity::from_pem(&self.agent_cert, &self.agent_key)
    }

    /// Write the CA and server files into `dir`, creating it if needed.
    /// Private keys are written with mode 0600 on Unix.
    pub fn write_to_dir(&self, dir: &Path) -> Result<(), TlsError> {
        let io_err = |path: &Path| {
            let path = path.to_owned();
            move |source| TlsError::Io { path, source }
        };
        fs::create_dir_all(dir).map_err(io_err(dir))?;
        for (name, contents, secret) in [
            (CA_CERT, &self.ca_cert, false),
            (CA_KEY, &self.ca_key, true),
            (SERVER_CERT, &self.server_cert, false),
            (SERVER_KEY, &self.server_key, true),
        ] {
            let path = dir.join(name);
            crate::fs::write_file(&path, contents.as_bytes(), secret).map_err(io_err(&path))?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tls::load_certs;

    #[test]
    fn generated_material_parses_and_builds_both_configs() {
        let certs = generate("agent-under-test").unwrap();
        let ca = certs.ca().unwrap();
        assert_eq!(ca.len(), 1);
        let server = certs.server_identity().unwrap();
        let agent = certs.agent_identity().unwrap();

        crate::quic::server_config(&server, &ca).unwrap();
        crate::quic::client_config(&ca, Some(&agent)).unwrap();
    }

    #[test]
    fn extra_names_are_accepted_by_a_client() {
        use rustls::client::danger::ServerCertVerifier;
        let certs =
            generate_with_names("a", &["192.168.122.1".into(), "rmm.example".into()]).unwrap();
        let mut roots = rustls::RootCertStore::empty();
        roots.add(certs.ca().unwrap().remove(0)).unwrap();
        let verifier = rustls::client::WebPkiServerVerifier::builder_with_provider(
            roots.into(),
            rustls::crypto::ring::default_provider().into(),
        )
        .build()
        .unwrap();
        let leaf = certs.server_identity().unwrap().cert_chain.remove(0);
        for name in ["localhost", "192.168.122.1", "rmm.example"] {
            verifier
                .verify_server_cert(
                    &leaf,
                    &[],
                    &rustls_pki_types::ServerName::try_from(name).unwrap(),
                    &[],
                    rustls_pki_types::UnixTime::now(),
                )
                .unwrap_or_else(|e| panic!("{name}: {e}"));
        }
    }

    #[test]
    fn each_generation_uses_a_new_ca() {
        let a = generate("a").unwrap();
        let b = generate("a").unwrap();
        assert_ne!(a.ca_cert, b.ca_cert);
        assert_ne!(a.agent_key, b.agent_key);
    }

    #[test]
    fn write_to_dir_round_trips_through_files() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("nested/dev-certs");
        let certs = generate("agent-1").unwrap();
        certs.write_to_dir(&out).unwrap();

        assert_eq!(load_certs(&out.join(CA_CERT)).unwrap(), certs.ca().unwrap());
        let ca = Identity::from_files(&out.join(CA_CERT), &out.join(CA_KEY)).unwrap();
        assert_eq!(ca.cert_chain, certs.ca().unwrap());
        let server = Identity::from_files(&out.join(SERVER_CERT), &out.join(SERVER_KEY)).unwrap();
        assert_eq!(
            server.cert_chain,
            certs.server_identity().unwrap().cert_chain
        );

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            for key in [CA_KEY, SERVER_KEY] {
                let mode = fs::metadata(out.join(key)).unwrap().permissions().mode();
                assert_eq!(mode & 0o777, 0o600, "{key}");
            }
        }
    }

    #[test]
    fn missing_file_reports_its_path() {
        let err = load_certs(Path::new("/definitely/not/here.crt")).unwrap_err();
        assert!(
            err.to_string().contains("/definitely/not/here.crt"),
            "{err}"
        );
    }
}
