//! A TLS client over bytes: rustls (no_std, unbuffered) with RustCrypto — the handshake, the certificate
//! checks (against [`crate::now`]'s time) and a client certificate if you have one. No sockets: send what
//! `outgoing()` gives with tarae's `send`, `feed()` what comes back (`data`), and `read()` the plaintext.
//!
//! ChaCha20 comes first among the ciphers: without AES instructions (WebAssembly has none) it's the faster.

use std::sync::Arc;

use rustls::client::UnbufferedClientConnection;
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName, UnixTime};
use rustls::time_provider::TimeProvider;
use rustls::unbuffered::{ConnectionState, EncodeError, EncryptError, UnbufferedStatus};
use rustls::{ClientConfig, RootCertStore};

pub use rustls::ClientConfig as Config;

#[derive(Debug)]
struct Clock;

impl TimeProvider for Clock {
    fn current_time(&self) -> Option<UnixTime> {
        let ms = crate::now::millis();
        (ms > 0).then(|| UnixTime::since_unix_epoch(std::time::Duration::from_millis(ms)))
    }
}

/// A client configuration: the CAs to trust (PEM), and a client certificate with its key (PEM) if any.
pub fn config(ca_pem: &[u8], client: Option<(&[u8], &[u8])>) -> Result<Arc<ClientConfig>, String> {
    let mut roots = RootCertStore::empty();
    for c in CertificateDer::pem_slice_iter(ca_pem) {
        roots.add(c.map_err(|e| format!("a CA certificate: {e}"))?).map_err(|e| e.to_string())?;
    }
    with_roots(roots, client)
}

/// A client configuration trusting the public CAs — Mozilla's list, compiled in (webpki-roots) — for servers
/// whose certificate a public CA signed (a cloud database's endpoint, often).
#[cfg(feature = "public-roots")]
pub fn config_public(client: Option<(&[u8], &[u8])>) -> Result<Arc<ClientConfig>, String> {
    let roots = RootCertStore { roots: webpki_roots::TLS_SERVER_ROOTS.to_vec() };
    with_roots(roots, client)
}

/// Encryption without checking who's at the other end: **the server's certificate isn't verified** — anyone in
/// the way can stand in for it. Its handshake signature is still checked against the certificate it shows. Only
/// for a configuration that asks for exactly this, in so many words.
pub fn config_unverified() -> Result<Arc<ClientConfig>, String> {
    let provider = provider();
    let verifier = Arc::new(Unverified(provider.clone()));
    let config = ClientConfig::builder_with_details(provider, Arc::new(Clock))
        .with_safe_default_protocol_versions()
        .map_err(|e| e.to_string())?
        .dangerous()
        .with_custom_certificate_verifier(verifier)
        .with_no_client_auth();
    Ok(Arc::new(config))
}

fn provider() -> Arc<rustls::crypto::CryptoProvider> {
    let mut provider = rustls_rustcrypto::provider();
    provider.cipher_suites.sort_by_key(|s| !format!("{:?}", s.suite()).contains("CHACHA20"));
    Arc::new(provider)
}

/// Takes any certificate; checks only that the handshake is signed by the key in it.
#[derive(Debug)]
struct Unverified(Arc<rustls::crypto::CryptoProvider>);

impl rustls::client::danger::ServerCertVerifier for Unverified {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(message, cert, dss, &self.0.signature_verification_algorithms)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(message, cert, dss, &self.0.signature_verification_algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.0.signature_verification_algorithms.supported_schemes()
    }
}

fn with_roots(roots: RootCertStore, client: Option<(&[u8], &[u8])>) -> Result<Arc<ClientConfig>, String> {
    let builder = ClientConfig::builder_with_details(provider(), Arc::new(Clock))
        .with_safe_default_protocol_versions()
        .map_err(|e| e.to_string())?
        .with_root_certificates(roots);
    let config = match client {
        Some((cert, key)) => {
            let certs = CertificateDer::pem_slice_iter(cert)
                .collect::<Result<Vec<_>, _>>()
                .map_err(|e| format!("the client certificate: {e}"))?;
            let key = PrivateKeyDer::from_pem_slice(key).map_err(|e| format!("the client key: {e}"))?;
            builder.with_client_auth_cert(certs, key).map_err(|e| e.to_string())?
        }
        None => builder.with_no_client_auth(),
    };
    Ok(Arc::new(config))
}

/// A TLS connection, driven by the bytes it's fed.
pub struct Tls {
    conn: UnbufferedClientConnection,
    incoming: Vec<u8>,
    outgoing: Vec<u8>,
    plain: Vec<u8>,
    /// Plaintext waiting for the handshake to end.
    queued: Vec<Vec<u8>>,
    handshaking: bool,
    closed: bool,
}

fn grow<E>(out: &mut Vec<u8>, mut f: impl FnMut(&mut [u8]) -> Result<usize, E>, need: impl Fn(&E) -> Option<usize>) -> Result<(), E> {
    let mut buf = vec![0u8; 16 * 1024 + 256];
    loop {
        match f(&mut buf) {
            Ok(n) => {
                out.extend_from_slice(&buf[..n]);
                return Ok(());
            }
            Err(e) => match need(&e) {
                Some(n) => buf.resize(n, 0),
                None => return Err(e),
            },
        }
    }
}

impl Tls {
    /// Starts a connection to `server` (a name, or an IP address, as its certificate says). The hello to
    /// send is in `outgoing()` at once.
    pub fn client(config: Arc<ClientConfig>, server: &str) -> Result<Tls, String> {
        let name = ServerName::try_from(server.to_string()).map_err(|e| format!("{server}: {e}"))?;
        let conn = UnbufferedClientConnection::new(config, name).map_err(|e| e.to_string())?;
        let mut t = Tls {
            conn,
            incoming: Vec::new(),
            outgoing: Vec::new(),
            plain: Vec::new(),
            queued: Vec::new(),
            handshaking: true,
            closed: false,
        };
        t.drive()?;
        Ok(t)
    }

    /// Plaintext to send (queued until the handshake is done).
    pub fn write(&mut self, plain: &[u8]) -> Result<(), String> {
        self.queued.push(plain.to_vec());
        self.drive()
    }

    /// Bytes that came from the other end.
    pub fn feed(&mut self, bytes: &[u8]) -> Result<(), String> {
        self.incoming.extend_from_slice(bytes);
        self.drive()
    }

    /// What to send now.
    pub fn outgoing(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.outgoing)
    }

    /// Plaintext that came in.
    pub fn read(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.plain)
    }

    pub fn handshaking(&self) -> bool {
        self.handshaking
    }

    /// Whether the other end closed it (TLS close_notify).
    pub fn closed(&self) -> bool {
        self.closed
    }

    fn drive(&mut self) -> Result<(), String> {
        loop {
            let UnbufferedStatus { mut discard, state } = self.conn.process_tls_records(&mut self.incoming);
            let mut wait = false;
            match state.map_err(|e| format!("tls: {e}"))? {
                ConnectionState::ReadTraffic(mut r) => {
                    while let Some(rec) = r.next_record() {
                        let rec = rec.map_err(|e| format!("tls: {e}"))?;
                        discard += rec.discard;
                        self.plain.extend_from_slice(rec.payload);
                    }
                }
                ConnectionState::EncodeTlsData(mut e) => {
                    grow(&mut self.outgoing, |b| e.encode(b), |e| match e {
                        EncodeError::InsufficientSize(s) => Some(s.required_size),
                        _ => None,
                    })
                    .map_err(|e| format!("tls: {e:?}"))?;
                }
                ConnectionState::TransmitTlsData(t) => t.done(),
                ConnectionState::BlockedHandshake => wait = true,
                ConnectionState::WriteTraffic(mut w) => {
                    self.handshaking = false;
                    if self.queued.is_empty() {
                        wait = true;
                    }
                    for plain in std::mem::take(&mut self.queued) {
                        grow(&mut self.outgoing, |b| w.encrypt(&plain, b), |e| match e {
                            EncryptError::InsufficientSize(s) => Some(s.required_size),
                            _ => None,
                        })
                        .map_err(|e| format!("tls: {e:?}"))?;
                    }
                }
                ConnectionState::PeerClosed | ConnectionState::Closed => {
                    self.closed = true;
                    wait = true;
                }
                _ => wait = true,
            }
            self.incoming.drain(..discard);
            if wait {
                return Ok(());
            }
        }
    }
}
