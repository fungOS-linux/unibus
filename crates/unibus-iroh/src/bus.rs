//! Gateway metadata uses the existing authenticated control-plane framing.
use crate::{CATALOG, Gateway};
use anyhow::{Result, ensure};
use std::time::{Duration, Instant};
use unibus::{Message, client::Client};

pub fn publish(client: &Client, gateways: &[Gateway]) -> Result<()> {
    ensure!(gateways.len() <= 64, "gateway catalog exceeds capacity");
    let body = serde_json::json!({"schema_version":1, "gateways":gateways});
    ensure!(
        serde_json::to_vec(&body)?.len() <= 64 * 1024,
        "gateway catalog exceeds frame budget"
    );
    let envelope = client.envelope(None, Message::new(CATALOG, body))?;
    client.send(&envelope, &[])?;
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        ensure!(
            Instant::now() < deadline,
            "gateway publication acknowledgment timed out"
        );
        let Some((reply, _)) = client.try_read()? else {
            continue;
        };
        if unibus::kinds::classify(&reply.message.kind) != unibus::kinds::Control::Reply
            || reply
                .message
                .body
                .get("request_id")
                .and_then(|v| v.as_str())
                != Some(envelope.request_id.as_str())
        {
            continue;
        }
        ensure!(
            reply.message.body.get("success").and_then(|v| v.as_bool()) == Some(true)
                || reply.message.body.get("message").and_then(|v| v.as_str())
                    == Some(unibus::kinds::REPLY_NO_ELIGIBLE_NODES),
            "router rejected gateway publication"
        );
        return Ok(());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{io::Write, net::TcpListener};
    use unibus::{Envelope, client::ClientConfig, framing};
    fn round_trip(accepted: bool) -> Result<()> {
        let listener = TcpListener::bind("127.0.0.1:0")?;
        let mut token = tempfile::NamedTempFile::new()?;
        token.write_all(b"gateway-test-only-token")?;
        let config: ClientConfig = serde_json::from_value(serde_json::json!({
            "installation_id":"test", "producer_id":"gateway", "hub_endpoint":listener.local_addr()?.to_string(),
            "token_file":token.path(), "zones":["infra"], "schema_version":2, "read_timeout_ms":100
        }))?;
        let server = std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            socket
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let (registration, _) = framing::read_frame(&mut socket).unwrap();
            let registration_reply = Envelope::new(
                "test",
                "hub",
                Some("gateway"),
                "",
                Message::new(
                    unibus::kinds::V1_REPLY,
                    unibus::kinds::reply_body(true, None, Some(registration.request_id)),
                ),
            );
            framing::write_frame(&mut socket, &registration_reply, &[]).unwrap();
            loop {
                let (request, payload) = framing::read_frame(&mut socket).unwrap();
                if request.message.kind != CATALOG {
                    continue;
                }
                assert!(payload.is_empty());
                assert_eq!(request.schema_version, 2);
                assert_eq!(
                    request.message.body["gateways"][0]["relay_urls"][0],
                    "https://relay.example.net"
                );
                assert!(
                    !serde_json::to_string(&request.message.body)
                        .unwrap()
                        .contains("gateway-test-only-token")
                );
                let reply = Envelope::new(
                    "test",
                    "hub",
                    Some("gateway"),
                    "",
                    Message::new(
                        unibus::kinds::V1_REPLY,
                        unibus::kinds::reply_body(accepted, None, Some(request.request_id)),
                    ),
                );
                framing::write_frame(&mut socket, &reply, &[]).unwrap();
                break;
            }
        });
        let client = Client::connect(config)?;
        let result = publish(
            &client,
            &[Gateway {
                service_id: "agora-relay".into(),
                relay_urls: vec!["https://relay.example.net".into()],
                observed_at_unix_s: 100,
                expires_at_unix_s: 200,
                provenance_provider: "deployment".into(),
                provenance_observer: "agora-one".into(),
            }],
        );
        server.join().unwrap();
        result
    }
    #[test]
    fn authenticated_framing_carries_only_public_metadata() {
        round_trip(true).unwrap();
    }
    #[test]
    fn router_rejection_is_not_a_success() {
        assert!(round_trip(false).is_err());
    }
}
