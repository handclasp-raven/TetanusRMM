//! The session's keys: a Noise handshake between viewer and agent, relayed
//! by the server, and the records sealed under its keys.
//!
//! **Handshake.** `Noise_XX_25519_ChaChaPoly_BLAKE2s`, viewer initiating,
//! three messages carried in `Envelope::Handshake`:
//!
//! ```text
//!  viewer                      server                     agent
//!    -> e ─────────────────────────────────────────────────▶
//!    ◀──────────────────── e, ee, s, es + AgentProof ─── <-
//!    -> s, se ─────────────────────────────────────────────▶
//! ```
//!
//! Each side then knows the other's static key and has proven possession of
//! its own. What makes those keys *identities*:
//!
//! - **agent**: its payload is an [`AgentProof`]: the agent's enrolled
//!   certificate chain and a signature, by the certificate's key, over the
//!   agent id and the Noise static key. The viewer checks the chain against
//!   the CA it trusts, that the certificate names the agent it asked for,
//!   and the signature ([`verify_proof`]).
//! - **viewer**: its static key is generated per connection and announced
//!   to the server with its (single-use, authorised) viewer token; the
//!   server tells the agent which key belongs to the session it asked
//!   consent for, and the agent completes the handshake with no other.
//!
//! The server can therefore route and authorise sessions but cannot read
//! or alter them. It could only get in the middle by also forging an agent
//! certificate with the CA it operates (and, being the authority on who may
//! view, it already decides that); a passive or compromised relay, or
//! anyone on the network paths, learns nothing.
//!
//! **Records.** After the handshake, each direction has its own key from
//! the Noise split. Records ([`Control`] messages) are sealed with
//! ChaCha20-Poly1305 under an explicit 64-bit counter nonce, never reused
//! under a key. Noise's own transport mode is not used because it caps
//! messages at 64 KiB (a clipboard can be 1 MiB) and requires in-order
//! delivery, while records may arrive over the relay or a direct path in
//! either order. [`Inbox`] restores the order and rejects replays.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use protocol::e2e::{agent_proof_message, AgentProof, Control, Envelope, KEY_LEN};
use ring::aead::{Aad, LessSafeKey, Nonce, UnboundKey, CHACHA20_POLY1305};
use rustls::pki_types::{CertificateDer, UnixTime};
use rustls::SignatureScheme;

/// The Noise protocol name. Both ends must agree.
pub const NOISE_PARAMS: &str = "Noise_XX_25519_ChaChaPoly_BLAKE2s";

/// Largest Noise handshake message.
const MAX_HANDSHAKE: usize = 65_535;

#[derive(Debug, thiserror::Error)]
pub enum E2eError {
    #[error("noise: {0}")]
    Noise(#[from] snow::Error),
    #[error("handshake message out of order")]
    OutOfOrder,
    #[error("the agent's identity proof is malformed")]
    MalformedProof,
    #[error("the agent's certificate is not valid: {0}")]
    Certificate(String),
    #[error("the agent's certificate is for {found:?}, not {expected:?}")]
    WrongAgent { expected: String, found: String },
    #[error("the agent's identity signature does not verify")]
    BadSignature,
    #[error("the agent's key cannot sign identity proofs: {0}")]
    Signing(String),
    #[error("the viewer's key is not the one the server announced for this session")]
    WrongViewer,
    #[error("record failed authentication")]
    Unauthentic,
    #[error("record is malformed: {0}")]
    Malformed(#[from] postcard::Error),
    /// Authentic, but not a record this side knows (e.g. from a newer
    /// peer). See [`Inbox::unreadable`].
    #[error("record of an unknown kind: {0}")]
    UnknownRecord(postcard::Error),
}

fn params() -> snow::params::NoiseParams {
    NOISE_PARAMS.parse().expect("valid Noise parameters")
}

/// Binds the handshake to this protocol and this agent.
fn prologue(agent_id: &str) -> Vec<u8> {
    let mut p = b"rmm e2e v1\0".to_vec();
    p.extend_from_slice(agent_id.as_bytes());
    p
}

/// An X25519 static key pair.
pub struct StaticKey {
    private: Vec<u8>,
    public: [u8; KEY_LEN],
}

impl StaticKey {
    pub fn generate() -> Self {
        let pair = snow::Builder::new(params())
            .generate_keypair()
            .expect("key generation");
        Self {
            public: pair.public.as_slice().try_into().expect("32-byte key"),
            private: pair.private,
        }
    }

    pub fn public(&self) -> [u8; KEY_LEN] {
        self.public
    }
}

impl Drop for StaticKey {
    fn drop(&mut self) {
        self.private.fill(0);
    }
}

impl std::fmt::Debug for StaticKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "StaticKey({})", hex(&self.public[..6]))
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Sign `static_key` for `agent_id` with the agent's certificate key.
pub fn sign_proof(
    identity: &common::Identity,
    agent_id: &str,
    static_key: &[u8; KEY_LEN],
) -> Result<AgentProof, E2eError> {
    let key = rustls::crypto::ring::sign::any_supported_type(&identity.key)
        .map_err(|e| E2eError::Signing(e.to_string()))?;
    let signer = key
        .choose_scheme(&[
            SignatureScheme::ECDSA_NISTP256_SHA256,
            SignatureScheme::ECDSA_NISTP384_SHA384,
            SignatureScheme::ED25519,
            SignatureScheme::RSA_PSS_SHA256,
        ])
        .ok_or_else(|| E2eError::Signing("no supported signature scheme".into()))?;
    let signature = signer
        .sign(&agent_proof_message(agent_id, static_key))
        .map_err(|e| E2eError::Signing(e.to_string()))?;
    Ok(AgentProof {
        cert_chain: identity
            .cert_chain
            .iter()
            .map(|c| c.as_ref().to_vec())
            .collect(),
        scheme: u16::from(signer.scheme()),
        signature,
    })
}

/// Check that `proof` shows `static_key` belongs to `agent_id`: the
/// certificate chains to `ca`, is valid now for client authentication (as
/// agent certificates are), names `agent_id` as its common name, and its
/// key signed [`agent_proof_message`].
pub fn verify_proof(
    proof: &AgentProof,
    agent_id: &str,
    static_key: &[u8],
    ca: &[CertificateDer<'static>],
) -> Result<(), E2eError> {
    let leaf_der = CertificateDer::from(
        proof
            .cert_chain
            .first()
            .ok_or(E2eError::MalformedProof)?
            .as_slice(),
    );
    let intermediates: Vec<CertificateDer<'_>> = proof.cert_chain[1..]
        .iter()
        .map(|c| CertificateDer::from(c.as_slice()))
        .collect();
    let anchors = ca
        .iter()
        .map(|c| webpki::anchor_from_trusted_cert(c).map(|a| a.to_owned()))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| E2eError::Certificate(format!("trusted CA: {e}")))?;
    let leaf = webpki::EndEntityCert::try_from(&leaf_der)
        .map_err(|e| E2eError::Certificate(e.to_string()))?;
    let algorithms = rustls::crypto::ring::default_provider().signature_verification_algorithms;
    leaf.verify_for_usage(
        algorithms.all,
        &anchors,
        &intermediates,
        UnixTime::now(),
        webpki::KeyUsage::client_auth(),
        None,
        None,
    )
    .map_err(|e| E2eError::Certificate(e.to_string()))?;

    let found = common_name(leaf_der.as_ref())?;
    if found != agent_id {
        return Err(E2eError::WrongAgent {
            expected: agent_id.to_owned(),
            found,
        });
    }

    let scheme = SignatureScheme::from(proof.scheme);
    let message = agent_proof_message(agent_id, static_key);
    let verified = algorithms
        .mapping
        .iter()
        .filter(|(s, _)| *s == scheme)
        .flat_map(|(_, algs)| algs.iter())
        .any(|alg| {
            leaf.verify_signature(*alg, &message, &proof.signature)
                .is_ok()
        });
    if verified {
        Ok(())
    } else {
        Err(E2eError::BadSignature)
    }
}

fn common_name(der: &[u8]) -> Result<String, E2eError> {
    let (_, cert) = x509_parser::parse_x509_certificate(der)
        .map_err(|e| E2eError::Certificate(e.to_string()))?;
    let cn = cert
        .subject()
        .iter_common_name()
        .next()
        .and_then(|cn| cn.as_str().ok())
        .ok_or_else(|| E2eError::Certificate("no common name".into()))?;
    Ok(cn.to_owned())
}

/// The viewer's half of the handshake.
pub struct Initiator {
    state: snow::HandshakeState,
    agent_id: String,
}

impl Initiator {
    /// Start a handshake with `agent_id`; returns the first message.
    pub fn start(key: &StaticKey, agent_id: &str) -> Result<(Self, Vec<u8>), E2eError> {
        let prologue = prologue(agent_id);
        let mut state = snow::Builder::new(params())
            .local_private_key(&key.private)?
            .prologue(&prologue)?
            .build_initiator()?;
        let mut buf = vec![0u8; MAX_HANDSHAKE];
        let n = state.write_message(&[], &mut buf)?;
        buf.truncate(n);
        Ok((
            Self {
                state,
                agent_id: agent_id.to_owned(),
            },
            buf,
        ))
    }

    /// Read the agent's reply, verify its identity against `ca`, and
    /// finish. Returns the last message (for the agent) and the session.
    pub fn finish(
        mut self,
        reply: &[u8],
        ca: &[CertificateDer<'static>],
    ) -> Result<(Vec<u8>, Session), E2eError> {
        let mut payload = vec![0u8; MAX_HANDSHAKE];
        let n = self.state.read_message(reply, &mut payload)?;
        let proof: AgentProof =
            postcard::from_bytes(&payload[..n]).map_err(|_| E2eError::MalformedProof)?;
        let agent_static = self.state.get_remote_static().ok_or(E2eError::OutOfOrder)?;
        verify_proof(&proof, &self.agent_id, agent_static, ca)?;
        let mut last = vec![0u8; MAX_HANDSHAKE];
        let n = self.state.write_message(&[], &mut last)?;
        last.truncate(n);
        if !self.state.is_handshake_finished() {
            return Err(E2eError::OutOfOrder);
        }
        let session = Session::split(&mut self.state, true);
        Ok((last, session))
    }
}

/// The agent's half of the handshake.
pub struct Responder {
    state: snow::HandshakeState,
    viewer_key: [u8; KEY_LEN],
    proof: Vec<u8>,
}

impl Responder {
    /// Expect a handshake from the viewer whose static key is
    /// `viewer_key`; `proof` is this agent's signed identity.
    pub fn new(
        key: &StaticKey,
        agent_id: &str,
        viewer_key: [u8; KEY_LEN],
        proof: &AgentProof,
    ) -> Result<Self, E2eError> {
        let prologue = prologue(agent_id);
        let state = snow::Builder::new(params())
            .local_private_key(&key.private)?
            .prologue(&prologue)?
            .build_responder()?;
        Ok(Self {
            state,
            viewer_key,
            proof: postcard::to_stdvec(proof).expect("proof serialises"),
        })
    }

    /// Read the viewer's first message; returns the reply (with the proof).
    pub fn reply(&mut self, first: &[u8]) -> Result<Vec<u8>, E2eError> {
        let mut scratch = vec![0u8; MAX_HANDSHAKE];
        self.state.read_message(first, &mut scratch)?;
        let mut out = vec![0u8; MAX_HANDSHAKE];
        let n = self.state.write_message(&self.proof, &mut out)?;
        out.truncate(n);
        Ok(out)
    }

    /// Read the viewer's last message and check it is the expected viewer.
    pub fn finish(mut self, last: &[u8]) -> Result<Session, E2eError> {
        let mut scratch = vec![0u8; MAX_HANDSHAKE];
        self.state.read_message(last, &mut scratch)?;
        if !self.state.is_handshake_finished() {
            return Err(E2eError::OutOfOrder);
        }
        let remote = self.state.get_remote_static().ok_or(E2eError::OutOfOrder)?;
        // Constant time is not needed: the key is public.
        if remote != self.viewer_key {
            return Err(E2eError::WrongViewer);
        }
        Ok(Session::split(&mut self.state, false))
    }
}

/// An established session: a sealer for what this side sends, an opener
/// for what it receives.
pub struct Session {
    pub sealer: Sealer,
    pub opener: Opener,
    /// The Noise handshake hash: identical on both ends, unique to this
    /// session. Not secret; shown to prove two ends share a session.
    pub fingerprint: [u8; 32],
}

impl Session {
    fn split(state: &mut snow::HandshakeState, initiator: bool) -> Self {
        let fingerprint = state.get_handshake_hash()[..32]
            .try_into()
            .expect("BLAKE2s hash is 32 bytes");
        // The first key protects initiator -> responder traffic.
        let (i2r, r2i) = state.dangerously_get_raw_split();
        let (send, recv) = if initiator { (i2r, r2i) } else { (r2i, i2r) };
        Self {
            sealer: Sealer::new(&send),
            opener: Opener::new(&recv),
            fingerprint,
        }
    }
}

fn aead_key(key: &[u8; KEY_LEN]) -> LessSafeKey {
    LessSafeKey::new(UnboundKey::new(&CHACHA20_POLY1305, key).expect("32-byte key"))
}

pub(crate) fn nonce(counter: u64) -> Nonce {
    let mut n = [0u8; 12];
    n[4..].copy_from_slice(&counter.to_be_bytes());
    Nonce::assume_unique_for_key(n)
}

/// Seals this side's records. Shared by every task that sends; the
/// counter makes each nonce unique.
pub struct Sealer {
    key: LessSafeKey,
    next: AtomicU64,
}

impl Sealer {
    fn new(key: &[u8; KEY_LEN]) -> Self {
        Self {
            key: aead_key(key),
            next: AtomicU64::new(0),
        }
    }

    pub fn seal(&self, record: &Control) -> Envelope {
        let nonce_value = self.next.fetch_add(1, Ordering::Relaxed);
        let mut buf = postcard::to_stdvec(record).expect("records serialise");
        self.key
            .seal_in_place_append_tag(nonce(nonce_value), Aad::empty(), &mut buf)
            .expect("sealing cannot fail below the length limit");
        Envelope::Record {
            nonce: nonce_value,
            ciphertext: buf,
        }
    }

    /// [`Sealer::seal`], encoded for `Message::Sealed`.
    pub fn seal_bytes(&self, record: &Control) -> Vec<u8> {
        postcard::to_stdvec(&self.seal(record)).expect("envelopes serialise")
    }
}

/// Opens the other side's records.
pub struct Opener {
    key: LessSafeKey,
}

impl Opener {
    fn new(key: &[u8; KEY_LEN]) -> Self {
        Self { key: aead_key(key) }
    }

    pub fn open(&self, nonce_value: u64, ciphertext: &[u8]) -> Result<Control, E2eError> {
        let mut buf = ciphertext.to_vec();
        let plain = self
            .key
            .open_in_place(nonce(nonce_value), Aad::empty(), &mut buf)
            .map_err(|_| E2eError::Unauthentic)?;
        // It is authentic, so the sender meant it: a newer peer's record.
        postcard::from_bytes(plain).map_err(E2eError::UnknownRecord)
    }
}

/// How long a record waits for an earlier one still in flight on the
/// other path before the gap is given up on.
pub const REORDER_WAIT: Duration = Duration::from_millis(500);

/// Records held back waiting for a gap to fill, at most.
const MAX_HELD: usize = 1024;

/// Puts opened records back in the order they were sealed, and drops
/// replays.
///
/// Each record is sent on exactly one path, and each path delivers in
/// order, so a gap means the missing record is still on the slower path
/// (just after a switch between relay and direct) or was lost with a path
/// that died. Records after a gap wait up to [`REORDER_WAIT`] for it.
#[derive(Debug, Default)]
pub struct Inbox {
    next: u64,
    /// `None`: a genuine record this side cannot read (see
    /// [`Inbox::unreadable`]); it only fills its place in the order.
    held: BTreeMap<u64, (Instant, Option<Control>)>,
}

impl Inbox {
    /// Whether `nonce` could still be delivered (neither delivered, skipped
    /// nor held already). Check before opening, to skip the work.
    pub fn wants(&self, nonce: u64) -> bool {
        nonce >= self.next && !self.held.contains_key(&nonce)
    }

    /// An opened record; returns what can now be delivered, in order.
    pub fn accept(&mut self, nonce: u64, record: Control, now: Instant) -> Vec<Control> {
        self.hold(nonce, Some(record), now)
    }

    /// Record `nonce` was authentic but of a kind this side does not know
    /// (the other side is newer). Nothing is delivered for it, but it is
    /// not a gap: the records after it need not wait.
    pub fn unreadable(&mut self, nonce: u64, now: Instant) -> Vec<Control> {
        self.hold(nonce, None, now)
    }

    fn hold(&mut self, nonce: u64, record: Option<Control>, now: Instant) -> Vec<Control> {
        if !self.wants(nonce) {
            return Vec::new();
        }
        self.held.insert(nonce, (now, record));
        if self.held.len() > MAX_HELD {
            return self.skip_gap();
        }
        self.drain()
    }

    /// Give up on gaps that have waited long enough.
    pub fn expire(&mut self, now: Instant) -> Vec<Control> {
        let mut out = Vec::new();
        while let Some((_, (since, _))) = self.held.first_key_value() {
            if now.saturating_duration_since(*since) < REORDER_WAIT {
                break;
            }
            out.extend(self.skip_gap());
        }
        out
    }

    /// When [`Inbox::expire`] next has something to do.
    pub fn deadline(&self) -> Option<Instant> {
        self.held
            .first_key_value()
            .map(|(_, (since, _))| *since + REORDER_WAIT)
    }

    fn skip_gap(&mut self) -> Vec<Control> {
        if let Some((&first, _)) = self.held.first_key_value() {
            self.next = first;
        }
        self.drain()
    }

    fn drain(&mut self) -> Vec<Control> {
        let mut out = Vec::new();
        while let Some((_, record)) = self.held.remove(&self.next) {
            out.extend(record);
            self.next += 1;
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use protocol::input::InputEvent;

    fn key(n: u16) -> Control {
        Control::Input(InputEvent::Key {
            scancode: n,
            down: true,
        })
    }

    /// An agent certificate like enrollment issues: CN = agent id,
    /// client-auth EKU, signed by `ca`.
    pub(crate) fn agent_identity(
        ca: &common::devcerts::DevCerts,
        agent_id: &str,
    ) -> common::Identity {
        use rcgen::{
            CertificateParams, DistinguishedName, DnType, ExtendedKeyUsagePurpose, IsCa, Issuer,
            KeyPair,
        };
        let ca_key = KeyPair::from_pem(&ca.ca_key).unwrap();
        let issuer = Issuer::from_ca_cert_pem(&ca.ca_cert, ca_key).unwrap();
        let key = KeyPair::generate().unwrap();
        let mut params = CertificateParams::default();
        let mut dn = DistinguishedName::new();
        dn.push(DnType::CommonName, agent_id);
        params.distinguished_name = dn;
        params.is_ca = IsCa::ExplicitNoCa;
        params.use_authority_key_identifier_extension = true;
        params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ClientAuth];
        let cert = params.signed_by(&key, &issuer).unwrap();
        common::Identity::from_pem(&cert.pem(), &key.serialize_pem()).unwrap()
    }

    struct Pair {
        viewer: Session,
        agent: Session,
    }

    fn handshake(
        certs: &common::devcerts::DevCerts,
        identity: &common::Identity,
        agent_id: &str,
    ) -> Result<Pair, E2eError> {
        let ca = certs.ca().unwrap();
        let viewer_key = StaticKey::generate();
        let agent_key = StaticKey::generate();
        let proof = sign_proof(identity, "agt-1", &agent_key.public()).unwrap();
        let mut responder =
            Responder::new(&agent_key, "agt-1", viewer_key.public(), &proof).unwrap();
        let (initiator, first) = Initiator::start(&viewer_key, agent_id)?;
        let reply = responder.reply(&first)?;
        let (last, viewer) = initiator.finish(&reply, &ca)?;
        let agent = responder.finish(&last)?;
        Ok(Pair { viewer, agent })
    }

    #[test]
    fn a_handshake_gives_both_ends_one_session() {
        let certs = common::devcerts::generate("unused").unwrap();
        let identity = agent_identity(&certs, "agt-1");
        let Pair { viewer, agent } = handshake(&certs, &identity, "agt-1").unwrap();
        assert_eq!(viewer.fingerprint, agent.fingerprint);

        // Both directions, including a record far over Noise's 64 KiB.
        let big = Control::Clipboard(protocol::clipboard::ClipboardData::Text(
            "x".repeat(1024 * 1024 - 16),
        ));
        for (from, to) in [(&viewer, &agent), (&agent, &viewer)] {
            for record in [key(0x1E), big.clone()] {
                let Envelope::Record { nonce, ciphertext } = from.sealer.seal(&record) else {
                    unreachable!()
                };
                assert_eq!(to.opener.open(nonce, &ciphertext).unwrap(), record);
                // Each direction has its own key.
                assert!(from.opener.open(nonce, &ciphertext).is_err());
            }
        }
    }

    #[test]
    fn records_are_bound_to_their_nonce_and_content() {
        let certs = common::devcerts::generate("unused").unwrap();
        let identity = agent_identity(&certs, "agt-1");
        let Pair { viewer, agent } = handshake(&certs, &identity, "agt-1").unwrap();
        let Envelope::Record { nonce, ciphertext } = viewer.sealer.seal(&key(1)) else {
            unreachable!()
        };
        assert!(matches!(
            agent.opener.open(nonce + 1, &ciphertext),
            Err(E2eError::Unauthentic)
        ));
        let mut tampered = ciphertext.clone();
        tampered[0] ^= 1;
        assert!(agent.opener.open(nonce, &tampered).is_err());
        // Nonces are never reused.
        let Envelope::Record { nonce: second, .. } = viewer.sealer.seal(&key(1)) else {
            unreachable!()
        };
        assert_eq!(second, nonce + 1);
    }

    #[test]
    fn the_viewer_rejects_an_agent_that_is_not_the_one_it_asked_for() {
        let certs = common::devcerts::generate("unused").unwrap();
        // A genuine certificate, but for another agent.
        let other = agent_identity(&certs, "agt-2");
        match handshake(&certs, &other, "agt-1") {
            Err(E2eError::WrongAgent { expected, found }) => {
                assert_eq!((expected.as_str(), found.as_str()), ("agt-1", "agt-2"));
            }
            other => panic!("expected WrongAgent, got {:?}", other.err()),
        }
        // A certificate from a CA the viewer does not trust.
        let rogue_ca = common::devcerts::generate("unused").unwrap();
        let rogue = agent_identity(&rogue_ca, "agt-1");
        assert!(matches!(
            handshake(&certs, &rogue, "agt-1"),
            Err(E2eError::Certificate(_))
        ));
    }

    #[test]
    fn a_proof_for_another_key_does_not_verify() {
        let certs = common::devcerts::generate("unused").unwrap();
        let identity = agent_identity(&certs, "agt-1");
        let key = StaticKey::generate();
        let proof = sign_proof(&identity, "agt-1", &key.public()).unwrap();
        let ca = certs.ca().unwrap();
        verify_proof(&proof, "agt-1", &key.public(), &ca).unwrap();
        assert!(matches!(
            verify_proof(&proof, "agt-1", &StaticKey::generate().public(), &ca),
            Err(E2eError::BadSignature)
        ));
    }

    #[test]
    fn the_agent_rejects_a_viewer_the_server_did_not_announce() {
        let certs = common::devcerts::generate("unused").unwrap();
        let identity = agent_identity(&certs, "agt-1");
        let ca = certs.ca().unwrap();
        let agent_key = StaticKey::generate();
        let proof = sign_proof(&identity, "agt-1", &agent_key.public()).unwrap();
        let announced = StaticKey::generate();
        let intruder = StaticKey::generate();
        let mut responder =
            Responder::new(&agent_key, "agt-1", announced.public(), &proof).unwrap();
        let (initiator, first) = Initiator::start(&intruder, "agt-1").unwrap();
        let reply = responder.reply(&first).unwrap();
        let (last, _) = initiator.finish(&reply, &ca).unwrap();
        assert!(matches!(
            responder.finish(&last),
            Err(E2eError::WrongViewer)
        ));
    }

    #[test]
    fn the_inbox_restores_order_and_drops_replays() {
        let t0 = Instant::now();
        let mut inbox = Inbox::default();
        assert_eq!(inbox.accept(0, key(0), t0), [key(0)]);
        // 2 and 3 overtake 1 (they came over the faster path).
        assert_eq!(inbox.accept(2, key(2), t0), []);
        assert_eq!(inbox.accept(3, key(3), t0), []);
        assert_eq!(inbox.accept(1, key(1), t0), [key(1), key(2), key(3)]);
        // Replays of anything delivered are dropped.
        assert!(!inbox.wants(2));
        assert_eq!(inbox.accept(2, key(2), t0), []);
        assert_eq!(inbox.deadline(), None);
    }

    #[test]
    fn the_inbox_gives_up_on_a_gap_after_a_while() {
        let t0 = Instant::now();
        let mut inbox = Inbox::default();
        assert_eq!(inbox.accept(1, key(1), t0), []);
        assert_eq!(inbox.accept(2, key(2), t0 + REORDER_WAIT / 2), []);
        assert_eq!(inbox.deadline(), Some(t0 + REORDER_WAIT));
        assert_eq!(inbox.expire(t0 + REORDER_WAIT / 2), []);
        assert_eq!(inbox.expire(t0 + REORDER_WAIT), [key(1), key(2)]);
        // Record 0 turning up late is dropped rather than delivered out of
        // order.
        assert_eq!(inbox.accept(0, key(0), t0 + REORDER_WAIT), []);
        assert_eq!(inbox.accept(3, key(3), t0 + REORDER_WAIT), [key(3)]);
    }

    #[test]
    fn an_unreadable_record_holds_nothing_up() {
        let t0 = Instant::now();
        let mut inbox = Inbox::default();
        assert_eq!(inbox.accept(1, key(1), t0), []);
        assert_eq!(inbox.unreadable(0, t0), [key(1)]);
        assert!(!inbox.wants(0));
        assert_eq!(inbox.unreadable(2, t0), []);
        assert_eq!(inbox.accept(3, key(3), t0), [key(3)]);
        assert_eq!(inbox.deadline(), None);
    }
}
