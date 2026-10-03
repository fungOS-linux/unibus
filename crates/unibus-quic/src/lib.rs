//! Shared QUIC/TLS transport used by both the optional router and direct peers.

use std::net::{SocketAddr, UdpSocket};
use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result, anyhow};
use quinn::{Connection, Endpoint};
use rustls::RootCertStore;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use unibus::{Envelope, MAX_FRAME, Qos, decode_frame, encode_frame};

pub mod direct;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuicDelivery {
    Datagram,
    OneWayStream,
}

pub fn server_config(
    certificates: Vec<CertificateDer<'static>>,
    private_key: PrivateKeyDer<'static>,
) -> Result<quinn::ServerConfig> {
    quinn::ServerConfig::with_single_cert(certificates, private_key)
        .context("build QUIC TLS server configuration")
}

pub fn server_config_from_pem(cert_file: &Path, key_file: &Path) -> Result<quinn::ServerConfig> {
    let cert_bytes = std::fs::read(cert_file)
        .with_context(|| format!("read QUIC certificate {}", cert_file.display()))?;
    let certificates = rustls_pemfile::certs(&mut cert_bytes.as_slice())
        .collect::<std::result::Result<Vec<_>, _>>()
        .with_context(|| format!("parse QUIC certificate {}", cert_file.display()))?;
    if certificates.is_empty() {
        return Err(anyhow!("QUIC certificate file contains no certificates"));
    }
    let key_bytes = std::fs::read(key_file)
        .with_context(|| format!("read QUIC private key {}", key_file.display()))?;
    let private_key = rustls_pemfile::private_key(&mut key_bytes.as_slice())
        .with_context(|| format!("parse QUIC private key {}", key_file.display()))?
        .context("QUIC private key file contains no key")?;
    server_config(certificates, private_key)
}

pub fn client_config(root_certificates: RootCertStore) -> Result<quinn::ClientConfig> {
    quinn::ClientConfig::with_root_certificates(Arc::new(root_certificates))
        .context("build QUIC TLS client configuration")
}

pub fn mutual_server_config(
    certificates: Vec<CertificateDer<'static>>,
    private_key: PrivateKeyDer<'static>,
    client_roots: RootCertStore,
) -> Result<quinn::ServerConfig> {
    let verifier = rustls::server::WebPkiClientVerifier::builder(Arc::new(client_roots))
        .build()
        .context("build QUIC client certificate verifier")?;
    let crypto = rustls::ServerConfig::builder()
        .with_client_cert_verifier(verifier)
        .with_single_cert(certificates, private_key)
        .context("build QUIC mTLS server configuration")?;
    Ok(quinn::ServerConfig::with_crypto(Arc::new(
        quinn::crypto::rustls::QuicServerConfig::try_from(crypto)?,
    )))
}

pub fn mutual_client_config(
    certificates: Vec<CertificateDer<'static>>,
    private_key: PrivateKeyDer<'static>,
    server_roots: RootCertStore,
) -> Result<quinn::ClientConfig> {
    let crypto = rustls::ClientConfig::builder()
        .with_root_certificates(server_roots)
        .with_client_auth_cert(certificates, private_key)
        .context("build QUIC mTLS client configuration")?;
    Ok(quinn::ClientConfig::new(Arc::new(
        quinn::crypto::rustls::QuicClientConfig::try_from(crypto)?,
    )))
}

pub struct QuicEndpoint {
    endpoint: Endpoint,
}

impl QuicEndpoint {
    pub fn server(address: SocketAddr, config: quinn::ServerConfig) -> Result<Self> {
        let socket = UdpSocket::bind(address).context("bind QUIC UDP socket")?;
        let runtime = quinn::default_runtime().ok_or_else(|| anyhow!("no Quinn runtime found"))?;
        let endpoint = Endpoint::new(
            quinn::EndpointConfig::default(),
            Some(config),
            socket,
            runtime,
        )?;
        Ok(Self { endpoint })
    }

    pub fn client(address: SocketAddr, config: quinn::ClientConfig) -> Result<Self> {
        let mut endpoint = Endpoint::client(address)?;
        endpoint.set_default_client_config(config);
        Ok(Self { endpoint })
    }

    pub fn local_addr(&self) -> Result<SocketAddr> {
        self.endpoint
            .local_addr()
            .context("read QUIC local address")
    }

    pub async fn accept(&self) -> Result<QuicPeer> {
        let incoming = self
            .endpoint
            .accept()
            .await
            .context("QUIC endpoint closed")?;
        let connection = incoming.await.context("complete QUIC TLS handshake")?;
        Ok(QuicPeer { connection })
    }

    pub async fn connect(&self, address: SocketAddr, server_name: &str) -> Result<QuicPeer> {
        let connection = self
            .endpoint
            .connect(address, server_name)
            .context("start QUIC connection")?
            .await
            .context("complete QUIC TLS handshake")?;
        Ok(QuicPeer { connection })
    }
}

#[derive(Clone)]
pub struct QuicPeer {
    connection: Connection,
}

impl QuicPeer {
    pub fn connection(&self) -> &Connection {
        &self.connection
    }

    pub fn delivery_for(&self, envelope: &Envelope, encoded_len: usize) -> QuicDelivery {
        if Qos::of_kind(&envelope.message.kind).datagram_capable()
            && self
                .connection
                .max_datagram_size()
                .is_some_and(|maximum| encoded_len <= maximum)
        {
            QuicDelivery::Datagram
        } else {
            QuicDelivery::OneWayStream
        }
    }

    pub async fn send(&self, envelope: &Envelope, payload: &[u8]) -> Result<QuicDelivery> {
        let encoded = encode_frame(envelope, payload)?;
        let delivery = self.delivery_for(envelope, encoded.len());
        match delivery {
            QuicDelivery::Datagram => self.connection.send_datagram(encoded.into())?,
            QuicDelivery::OneWayStream => {
                let mut stream = self.connection.open_uni().await?;
                stream.write_all(&encoded).await?;
                stream.finish()?;
                stream.stopped().await?;
            }
        }
        Ok(delivery)
    }

    pub async fn recv(&self) -> Result<(Envelope, Vec<u8>)> {
        let encoded = tokio::select! {
            datagram = self.connection.read_datagram() => datagram?.to_vec(),
            stream = self.connection.accept_uni() => {
                stream?.read_to_end(MAX_FRAME as usize).await?
            }
        };
        decode_frame(&encoded).map_err(Into::into)
    }

    pub fn peer_certificate(&self) -> Result<CertificateDer<'static>> {
        let identity = self
            .connection
            .peer_identity()
            .context("peer supplied no TLS identity")?;
        let certificates = identity
            .downcast::<Vec<CertificateDer<'static>>>()
            .map_err(|_| anyhow!("unexpected QUIC peer identity type"))?;
        certificates
            .first()
            .cloned()
            .context("peer supplied no TLS certificate")
    }
}

pub fn certificate_key_binding(certificate: &CertificateDer<'_>) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(certificate.as_ref());
    let mut output = String::from("sha256:");
    for byte in digest {
        use std::fmt::Write as _;
        write!(output, "{byte:02x}").expect("write to string");
    }
    output
}
