//! Subscribe-only MQTT/TLS client for embedded appliances. No publish API.
//! An independently enrolled leaf-certificate pin is the trust anchor, not
//! discovery, DNS, or trust-on-first-use. TLS signatures remain verified.
use anyhow::{Context, Result, ensure};
use rumqttc::{Client, Connection, Event, Incoming, MqttOptions, QoS, RecvTimeoutError, Transport};
use rustls::{
    DigitallySignedStruct, Error, SignatureScheme,
    client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier},
    pki_types::{CertificateDer, ServerName, UnixTime},
};
use sha2::{Digest, Sha256};
use std::{sync::Arc, time::Duration};

pub const MAX_REPORT_BYTES: usize = 64 * 1024;
#[derive(Debug)]
struct PinnedCertificate {
    digest: [u8; 32],
    provider: Arc<rustls::crypto::CryptoProvider>,
}
impl ServerCertVerifier for PinnedCertificate {
    fn verify_server_cert(
        &self,
        cert: &CertificateDer<'_>,
        _: &[CertificateDer<'_>],
        _: &ServerName<'_>,
        _: &[u8],
        _: UnixTime,
    ) -> std::result::Result<ServerCertVerified, Error> {
        let actual: [u8; 32] = Sha256::digest(cert.as_ref()).into();
        if actual != self.digest {
            return Err(Error::General(
                "enrolled MQTT certificate pin mismatch".into(),
            ));
        }
        Ok(ServerCertVerified::assertion())
    }
    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        signature: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            signature,
            &self.provider.signature_verification_algorithms,
        )
    }
    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        signature: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            signature,
            &self.provider.signature_verification_algorithms,
        )
    }
    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}
pub fn parse_pin(value: &str) -> Result<[u8; 32]> {
    ensure!(
        value.len() == 64 && value.bytes().all(|b| b.is_ascii_hexdigit()),
        "certificate pin must be 64 hexadecimal SHA-256 characters"
    );
    let mut digest = [0; 32];
    for (index, byte) in digest.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&value[index * 2..index * 2 + 2], 16)?;
    }
    Ok(digest)
}
fn tls_config(pin: [u8; 32]) -> Result<rustls::ClientConfig> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    Ok(
        rustls::ClientConfig::builder_with_provider(provider.clone())
            .with_safe_default_protocol_versions()?
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(PinnedCertificate {
                digest: pin,
                provider,
            }))
            .with_no_client_auth(),
    )
}
pub struct ReportSubscriber {
    _client: Client,
    connection: Connection,
}
pub enum ReportEvent {
    Connected,
    Subscribed,
    Report {
        topic: String,
        payload: Vec<u8>,
        retained: bool,
        duplicate: bool,
    },
}
impl ReportSubscriber {
    pub fn connect(
        endpoint: std::net::SocketAddr,
        client_id: &str,
        topic: &str,
        username: &str,
        password: &str,
        pin: [u8; 32],
    ) -> Result<Self> {
        ensure!(
            endpoint.port() != 0
                && !endpoint.ip().is_unspecified()
                && !endpoint.ip().is_multicast(),
            "invalid MQTT endpoint"
        );
        ensure!(
            super::valid_bridge_identifier(client_id),
            "invalid MQTT client identity"
        );
        ensure!(
            !topic.is_empty() && topic.len() <= 256 && !topic.contains(['#', '+', '\0']),
            "report subscription must be an exact topic"
        );
        ensure!(
            !username.is_empty() && !password.is_empty(),
            "MQTT credentials are required"
        );
        let mut options = MqttOptions::new(client_id, endpoint.ip().to_string(), endpoint.port());
        options
            .set_transport(Transport::tls_with_config(tls_config(pin)?.into()))
            .set_credentials(username, password)
            .set_clean_session(true)
            .set_keep_alive(Duration::from_secs(15))
            .set_max_packet_size(MAX_REPORT_BYTES + 512, 1024);
        let (client, connection) = Client::new(options, 8);
        client.subscribe(topic, QoS::AtLeastOnce)?;
        Ok(Self {
            _client: client,
            connection,
        })
    }
    pub fn poll(&mut self, timeout: Duration) -> Result<Option<ReportEvent>> {
        let event = match self.connection.recv_timeout(timeout) {
            Ok(event) => event.context("MQTT report connection failed")?,
            Err(RecvTimeoutError::Timeout) => return Ok(None),
            Err(RecvTimeoutError::Disconnected) => {
                anyhow::bail!("MQTT report subscription disconnected")
            }
        };
        Ok(match event {
            Event::Incoming(Incoming::ConnAck(_)) => Some(ReportEvent::Connected),
            Event::Incoming(Incoming::SubAck(ack)) => {
                ensure!(
                    !ack.return_codes.is_empty()
                        && ack
                            .return_codes
                            .iter()
                            .all(|qos| !matches!(qos, rumqttc::SubscribeReasonCode::Failure)),
                    "MQTT report subscription rejected"
                );
                Some(ReportEvent::Subscribed)
            }
            Event::Incoming(Incoming::Publish(p)) => {
                ensure!(p.payload.len() <= MAX_REPORT_BYTES, "MQTT report too large");
                Some(ReportEvent::Report {
                    topic: p.topic,
                    payload: p.payload.to_vec(),
                    retained: p.retain,
                    duplicate: p.dup,
                })
            }
            _ => None,
        })
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn real_tls_handshake_requires_enrolled_certificate() {
        use std::{
            io::{Read, Write},
            net::{TcpListener, TcpStream},
            thread,
        };
        let generated = rcgen::generate_simple_self_signed(vec!["printer.test".into()]).unwrap();
        let cert = generated.cert.der().clone();
        let pin: [u8; 32] = Sha256::digest(cert.as_ref()).into();
        let key =
            rustls::pki_types::PrivatePkcs8KeyDer::from(generated.signing_key.serialize_der())
                .into();
        let server = rustls::ServerConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(vec![cert], key)
        .unwrap();
        for enrolled in [pin, [0; 32]] {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let addr = listener.local_addr().unwrap();
            let config = server.clone();
            let worker = thread::spawn(move || {
                let (socket, _) = listener.accept().unwrap();
                socket
                    .set_read_timeout(Some(Duration::from_secs(3)))
                    .unwrap();
                let mut tls = rustls::StreamOwned::new(
                    rustls::ServerConnection::new(Arc::new(config)).unwrap(),
                    socket,
                );
                let mut byte = [0];
                match tls.read_exact(&mut byte) {
                    Ok(()) => {
                        assert_eq!(byte, [42]);
                        tls.write_all(&[43]).unwrap();
                        true
                    }
                    Err(_) => false,
                }
            });
            let socket = TcpStream::connect(addr).unwrap();
            socket
                .set_read_timeout(Some(Duration::from_secs(3)))
                .unwrap();
            let client = rustls::ClientConnection::new(
                Arc::new(tls_config(enrolled).unwrap()),
                ServerName::try_from("127.0.0.1".to_owned()).unwrap(),
            )
            .unwrap();
            let mut tls = rustls::StreamOwned::new(client, socket);
            let result = tls.write_all(&[42]).and_then(|_| {
                let mut b = [0];
                tls.read_exact(&mut b)?;
                assert_eq!(b, [43]);
                Ok(())
            });
            assert_eq!(result.is_ok(), enrolled == pin);
            drop(tls);
            assert_eq!(worker.join().unwrap(), enrolled == pin);
        }
    }
    #[test]
    fn exact_pins_reject_substitution() {
        let cert = CertificateDer::from(vec![1, 2, 3]);
        let verifier = PinnedCertificate {
            digest: Sha256::digest(cert.as_ref()).into(),
            provider: Arc::new(rustls::crypto::ring::default_provider()),
        };
        let name = ServerName::try_from("192.0.2.1".to_owned()).unwrap();
        verifier
            .verify_server_cert(
                &cert,
                &[],
                &name,
                &[],
                UnixTime::since_unix_epoch(Duration::ZERO),
            )
            .unwrap();
        assert!(
            verifier
                .verify_server_cert(
                    &CertificateDer::from(vec![1, 2, 4]),
                    &[],
                    &name,
                    &[],
                    UnixTime::since_unix_epoch(Duration::ZERO)
                )
                .is_err()
        );
        assert!(parse_pin("bad").is_err());
        assert!(parse_pin(&"é".repeat(32)).is_err());
    }
}
