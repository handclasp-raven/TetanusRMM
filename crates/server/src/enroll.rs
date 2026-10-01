//! Agent enrollment: one-time tokens and the internal CA that issues
//! per-agent client certificates.
//!
//! 1. A user creates a download link ([`create_token`]). The link carries a
//!    random token; only its SHA-256 is stored, with an expiry.
//! 2. The agent connects over QUIC without a client certificate and sends
//!    `Enroll { token, csr }` (see `crate::quic`).
//! 3. [`enroll`] checks and consumes the token, assigns an agent id, signs a
//!    certificate for the CSR's public key, and pins that certificate's
//!    fingerprint to the agent in the registry. All in one transaction.
//! 4. The agent reconnects with the certificate; `Hello` is accepted only if
//!    the certificate's fingerprint matches the pinned one.

use std::time::Duration;

use chrono::{DateTime, Utc};
use rcgen::{
    CertificateParams, CertificateSigningRequestParams, DistinguishedName, DnType,
    ExtendedKeyUsagePurpose, IsCa, Issuer, KeyPair, KeyUsagePurpose,
};
use ring::rand::{SecureRandom, SystemRandom};
use rustls::pki_types::CertificateSigningRequestDer;
use serde_json::json;
use sqlx::PgPool;

use crate::audit::{self, Action, NewEntry};
use crate::auth::{new_token, token_hash};
use crate::registry;

/// Default and maximum lifetime of an enrollment token.
pub const DEFAULT_TOKEN_TTL: Duration = Duration::from_secs(24 * 60 * 60);
pub const MAX_TOKEN_TTL: Duration = Duration::from_secs(7 * 24 * 60 * 60);

/// Validity of an issued agent certificate. Renewal is not implemented yet;
/// revocation is immediate regardless, because `Hello` checks the registry.
pub const AGENT_CERT_VALIDITY: Duration = Duration::from_secs(5 * 365 * 24 * 60 * 60);

#[derive(Debug, thiserror::Error)]
pub enum CaError {
    #[error("loading CA: {0}")]
    Load(String),
    #[error("certificate signing request rejected: {0}")]
    BadCsr(rcgen::Error),
    #[error("issuing certificate: {0}")]
    Issue(rcgen::Error),
}

/// The server's internal CA, used to sign agent certificates.
pub struct AgentCa {
    issuer: Issuer<'static, KeyPair>,
    ca_pem: String,
}

impl AgentCa {
    pub fn from_pem(ca_cert_pem: &str, ca_key_pem: &str) -> Result<Self, CaError> {
        let key = KeyPair::from_pem(ca_key_pem).map_err(|e| CaError::Load(e.to_string()))?;
        let issuer =
            Issuer::from_ca_cert_pem(ca_cert_pem, key).map_err(|e| CaError::Load(e.to_string()))?;
        Ok(Self {
            issuer,
            ca_pem: ca_cert_pem.to_owned(),
        })
    }

    pub fn ca_pem(&self) -> &str {
        &self.ca_pem
    }

    /// Issue a client certificate for `agent_id` over the CSR's public key.
    ///
    /// The CSR's signature is verified (proving the agent holds the private
    /// key), but everything else it asks for is ignored: subject, validity,
    /// key usage and extensions all come from this template, so an agent
    /// cannot request a CA certificate or another agent's name.
    pub fn issue(&self, agent_id: &str, csr_der: &[u8]) -> Result<String, CaError> {
        let csr =
            CertificateSigningRequestParams::from_der(&CertificateSigningRequestDer::from(csr_der))
                .map_err(CaError::BadCsr)?;

        let mut params = CertificateParams::default();
        let mut dn = DistinguishedName::new();
        dn.push(DnType::OrganizationName, "RMM");
        dn.push(DnType::CommonName, agent_id);
        params.distinguished_name = dn;
        params.is_ca = IsCa::ExplicitNoCa;
        // Names the CA by its key, as strict verifiers require.
        params.use_authority_key_identifier_extension = true;
        params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
        params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ClientAuth];
        params.serial_number = Some(random_serial().into());
        let now = time::OffsetDateTime::now_utc();
        params.not_before = now - time::Duration::minutes(5);
        params.not_after = now + AGENT_CERT_VALIDITY;

        CertificateSigningRequestParams {
            params,
            public_key: csr.public_key,
        }
        .signed_by(&self.issuer)
        .map(|cert| cert.pem())
        .map_err(CaError::Issue)
    }
}

fn random_bytes<const N: usize>() -> [u8; N] {
    let mut bytes = [0u8; N];
    SystemRandom::new()
        .fill(&mut bytes)
        .expect("system RNG available");
    bytes
}

/// Positive 16-byte serial (top bit clear so DER does not read it as negative).
fn random_serial() -> Vec<u8> {
    let mut serial = random_bytes::<16>();
    serial[0] &= 0x7f;
    serial[0] |= 0x01;
    serial.to_vec()
}

fn new_agent_id() -> String {
    format!("agt-{}", hex::encode(random_bytes::<8>()))
}

/// A freshly minted enrollment token. `token` is shown once and never stored.
#[derive(Debug)]
pub struct NewToken {
    pub id: i64,
    pub token: String,
    pub expires_at: DateTime<Utc>,
}

/// Mint a single-use enrollment token. Audited as `enrollment.create`.
pub async fn create_token(pool: &PgPool, actor: &str, ttl: Duration) -> sqlx::Result<NewToken> {
    create_token_in_groups(pool, actor, ttl, &[]).await
}

/// [`create_token`] for an agent that joins `group_ids` when it enrolls
/// (groups deleted meanwhile are skipped). The caller checks the groups
/// exist and that the actor may assign them.
pub async fn create_token_in_groups(
    pool: &PgPool,
    actor: &str,
    ttl: Duration,
    group_ids: &[i64],
) -> sqlx::Result<NewToken> {
    create_link_token(pool, actor, ttl, group_ids, None).await
}

/// Where an agent installed from a link's MSI connects.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallTarget {
    /// QUIC address, `host:port`.
    pub server: String,
    /// Name the server certificate must be valid for.
    pub server_name: String,
}

/// [`create_token_in_groups`], remembering `install` for the link's MSI.
/// The caller validates it (see `crate::msi`).
pub async fn create_link_token(
    pool: &PgPool,
    actor: &str,
    ttl: Duration,
    group_ids: &[i64],
    install: Option<&InstallTarget>,
) -> sqlx::Result<NewToken> {
    let token = new_token();
    let mut tx = pool.begin().await?;
    let (id, expires_at): (i64, DateTime<Utc>) = sqlx::query_as(
        "INSERT INTO enrollment_tokens
             (token_hash, created_by, expires_at, group_ids, install_server, install_server_name)
         VALUES ($1, $2, now() + make_interval(secs => $3), $4, $5, $6)
         RETURNING id, expires_at",
    )
    .bind(token_hash(&token))
    .bind(actor)
    .bind(ttl.as_secs_f64())
    .bind(group_ids)
    .bind(install.map(|i| &i.server))
    .bind(install.map(|i| &i.server_name))
    .fetch_one(&mut *tx)
    .await?;
    let mut detail = json!({ "token_id": id, "expires_at": expires_at });
    if !group_ids.is_empty() {
        detail["group_ids"] = json!(group_ids);
    }
    if let Some(install) = install {
        detail["server"] = json!(install.server);
        detail["server_name"] = json!(install.server_name);
    }
    audit::append(
        &mut tx,
        NewEntry::new(actor, Action::EnrollmentCreate).detail(detail),
    )
    .await?;
    tx.commit().await?;
    Ok(NewToken {
        id,
        token,
        expires_at,
    })
}

/// A usable token's install target: `None` if the token is unusable,
/// `Some(None)` if it was made without one.
pub async fn usable_install_target(
    pool: &PgPool,
    token: &str,
) -> sqlx::Result<Option<Option<InstallTarget>>> {
    let row: Option<(Option<String>, Option<String>)> = sqlx::query_as(
        "SELECT install_server, install_server_name FROM enrollment_tokens
         WHERE token_hash = $1 AND used_at IS NULL AND expires_at > now()",
    )
    .bind(token_hash(token))
    .fetch_optional(pool)
    .await?;
    Ok(
        row.map(|(server, server_name)| match (server, server_name) {
            (Some(server), Some(server_name)) => Some(InstallTarget {
                server,
                server_name,
            }),
            _ => None,
        }),
    )
}

/// Whether `token` is currently usable (exists, unused, unexpired).
/// Does not consume it.
pub async fn token_is_usable(pool: &PgPool, token: &str) -> sqlx::Result<bool> {
    sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM enrollment_tokens
         WHERE token_hash = $1 AND used_at IS NULL AND expires_at > now())",
    )
    .bind(token_hash(token))
    .fetch_one(pool)
    .await
}

#[derive(Debug, thiserror::Error)]
pub enum EnrollError {
    /// Unknown, expired or already-used token. Deliberately indistinguishable.
    #[error("enrollment token is invalid, expired or already used")]
    InvalidToken,
    #[error(transparent)]
    Ca(#[from] CaError),
    #[error(transparent)]
    Db(#[from] sqlx::Error),
}

/// A completed enrollment.
#[derive(Debug)]
pub struct Enrollment {
    pub agent_id: String,
    pub cert_pem: String,
    pub fingerprint: String,
}

/// Consume `token` and issue a certificate for the CSR. Single-use: of any
/// number of concurrent or repeated calls with the same token, exactly one
/// succeeds. Audited as `agent.enroll`.
pub async fn enroll(
    pool: &PgPool,
    ca: &AgentCa,
    token: &str,
    csr_der: &[u8],
) -> Result<Enrollment, EnrollError> {
    let mut tx = pool.begin().await?;

    // FOR UPDATE makes a concurrent enrollment with the same token wait for
    // this transaction, then re-check `used_at IS NULL` and find nothing.
    let row: Option<(i64, Vec<i64>)> = sqlx::query_as(
        "SELECT id, group_ids FROM enrollment_tokens
         WHERE token_hash = $1 AND used_at IS NULL AND expires_at > now()
         FOR UPDATE",
    )
    .bind(token_hash(token))
    .fetch_optional(&mut *tx)
    .await?;
    let (token_id, group_ids) = row.ok_or(EnrollError::InvalidToken)?;

    let agent_id = new_agent_id();
    let cert_pem = ca.issue(&agent_id, csr_der)?;
    let fingerprint = crate::quic::cert_fingerprint(
        &common::tls::certs_from_pem(&cert_pem).map_err(|e| CaError::Load(e.to_string()))?[0],
    );

    registry::insert_enrolled(&mut tx, &agent_id, &fingerprint).await?;
    sqlx::query("UPDATE enrollment_tokens SET used_at = now(), agent_id = $2 WHERE id = $1")
        .bind(token_id)
        .bind(&agent_id)
        .execute(&mut *tx)
        .await?;
    let joined: Vec<i64> = sqlx::query_scalar(
        "INSERT INTO agent_group_members (group_id, agent_id)
         SELECT id, $2 FROM agent_groups WHERE id = ANY($1)
         RETURNING group_id",
    )
    .bind(&group_ids)
    .bind(&agent_id)
    .fetch_all(&mut *tx)
    .await?;
    let mut detail = json!({ "token_id": token_id, "cert_fingerprint": fingerprint });
    if !joined.is_empty() {
        detail["group_ids"] = json!(joined);
    }
    audit::append(
        &mut tx,
        NewEntry::new(format!("agent:{agent_id}"), Action::AgentEnroll)
            .target(&agent_id)
            .detail(detail),
    )
    .await?;
    tx.commit().await?;

    Ok(Enrollment {
        agent_id,
        cert_pem,
        fingerprint,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ca() -> (AgentCa, common::devcerts::DevCerts) {
        let certs = common::devcerts::generate("unused").unwrap();
        (
            AgentCa::from_pem(&certs.ca_cert, &certs.ca_key).unwrap(),
            certs,
        )
    }

    fn csr(params: CertificateParams) -> Vec<u8> {
        let key = KeyPair::generate().unwrap();
        params.serialize_request(&key).unwrap().der().to_vec()
    }

    #[test]
    fn issued_cert_is_a_client_cert_for_the_agent_trusted_by_the_ca() {
        let (ca, certs) = ca();
        let pem = ca
            .issue("agt-123", &csr(CertificateParams::default()))
            .unwrap();
        let chain = common::tls::certs_from_pem(&pem).unwrap();

        // Verify with the same verifier the QUIC server uses.
        let mut roots = rustls::RootCertStore::empty();
        roots.add(certs.ca().unwrap().remove(0)).unwrap();
        let verifier = rustls::server::WebPkiClientVerifier::builder_with_provider(
            roots.into(),
            rustls::crypto::ring::default_provider().into(),
        )
        .build()
        .unwrap();
        verifier
            .verify_client_cert(&chain[0], &[], rustls::pki_types::UnixTime::now())
            .unwrap();
    }

    #[test]
    fn csr_cannot_request_ca_rights() {
        let (ca, _) = ca();
        let mut evil = CertificateParams::new(vec!["evil.example".into()]).unwrap();
        evil.is_ca = IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
        evil.distinguished_name
            .push(DnType::CommonName, "agt-someone-else");
        let pem = ca.issue("agt-1", &csr(evil)).unwrap();
        let der = common::tls::certs_from_pem(&pem).unwrap().remove(0);

        // The issued certificate follows the server's template, not the CSR.
        use x509_parser::prelude::*;
        let (_, cert) = X509Certificate::from_der(&der).unwrap();
        let cn: Vec<_> = cert
            .subject()
            .iter_common_name()
            .map(|cn| cn.as_str().unwrap().to_owned())
            .collect();
        assert_eq!(cn, ["agt-1"]);
        let constraints = cert.basic_constraints().unwrap().map(|ext| ext.value.ca);
        assert_eq!(constraints, Some(false), "must be explicitly not a CA");
        assert!(cert.subject_alternative_name().unwrap().is_none());
        let eku = cert.extended_key_usage().unwrap().unwrap().value;
        assert!(eku.client_auth && !eku.server_auth);

        // The authority key identifier names the issuing CA's key.
        let ca_der = common::tls::certs_from_pem(ca.ca_pem()).unwrap().remove(0);
        let (_, ca_cert) = X509Certificate::from_der(&ca_der).unwrap();
        let ca_ski = ca_cert
            .extensions()
            .iter()
            .find_map(|e| match e.parsed_extension() {
                ParsedExtension::SubjectKeyIdentifier(id) => Some(id.0.to_vec()),
                _ => None,
            });
        let aki = cert
            .extensions()
            .iter()
            .find_map(|e| match e.parsed_extension() {
                ParsedExtension::AuthorityKeyIdentifier(aki) => {
                    aki.key_identifier.as_ref().map(|id| id.0.to_vec())
                }
                _ => None,
            });
        assert!(ca_ski.is_some());
        assert_eq!(aki, ca_ski);
    }

    #[test]
    fn garbage_csr_is_rejected() {
        let (ca, _) = ca();
        assert!(matches!(
            ca.issue("agt-1", b"not a csr"),
            Err(CaError::BadCsr(_))
        ));
    }

    #[test]
    fn csr_with_bad_signature_is_rejected() {
        let (ca, _) = ca();
        let mut der = csr(CertificateParams::default());
        let last = der.len() - 1;
        der[last] ^= 0xff; // flip a bit in the signature
        assert!(matches!(ca.issue("agt-1", &der), Err(CaError::BadCsr(_))));
    }

    #[test]
    fn agent_ids_and_serials_are_random() {
        assert_ne!(new_agent_id(), new_agent_id());
        let s = random_serial();
        assert_eq!(s.len(), 16);
        assert!(s[0] & 0x80 == 0 && s[0] != 0);
    }
}
