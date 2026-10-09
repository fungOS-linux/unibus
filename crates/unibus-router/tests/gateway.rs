//! Actual local TCP daemon/adapter flow; verifier is a test-only fixture.
use serde_json::json;
use std::{net::TcpStream, sync::Arc, time::Duration};
use unibus::{
    AuthError, Envelope, Message, NodeClaims, OfflineVerifier,
    gateway::{
        GATEWAY_INVOKE, GATEWAY_REGISTER, GATEWAY_RELEASE, GATEWAY_RESULT, GatewayInvoke,
        GatewayRegistration, GatewayRelease,
    },
    read_frame, write_frame,
};
use unibus_router::{
    Hub, HubConfig,
    gateway::{GatewayConfig, GatewayDispatch, GatewayResult, GatewayRouter},
};

struct FixtureVerifier;
impl OfflineVerifier for FixtureVerifier {
    fn verify(&self, assertion: &str, now: u64) -> Result<NodeClaims, AuthError> {
        if !matches!(assertion, "caller" | "backend") {
            return Err(AuthError::Malformed);
        }
        Ok(NodeClaims {
            iss: "test-authority".into(),
            sub: format!("test/{assertion}"),
            aud: "unibus:test".into(),
            iat: now,
            exp: now + 60,
            jti: format!("test-{assertion}"),
            unibus_installation_id: "test".into(),
            unibus_producer_id: assertion.into(),
            unibus_zones: vec![],
            unibus_capabilities: vec![],
            unibus_screen_anchors: vec![],
        })
    }
}
fn receive(stream: &mut TcpStream, kind: &str) -> Envelope {
    for _ in 0..8 {
        let (frame, _) = read_frame(stream).unwrap();
        if frame.message.kind == kind {
            return frame;
        }
    }
    panic!("expected {kind}");
}
fn connect(addr: std::net::SocketAddr, producer: &str) -> TcpStream {
    let mut stream = TcpStream::connect(addr).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(3)))
        .unwrap();
    let mut register = Envelope::new(
        "test",
        producer,
        None,
        producer,
        Message::new(
            "register",
            json!({"capabilities":[],"zones":[],"screen_anchors":[]}),
        ),
    );
    register.schema_version = 2;
    write_frame(&mut stream, &register, &[]).unwrap();
    let reply = receive(&mut stream, "reply");
    assert_eq!(reply.message.body["success"], true);
    stream
}
#[test]
fn authenticated_daemon_routes_only_explicit_resource_and_correlates_reply() {
    let config:HubConfig=serde_json::from_value(json!({"installation_id":"test","listen":"127.0.0.1:0","nodes":[
        {"producer_id":"caller","auth_subject":"test/caller","capabilities":[],"publishes":[GATEWAY_INVOKE]},
        {"producer_id":"backend","auth_subject":"test/backend","capabilities":[],"publishes":[GATEWAY_RESULT,GATEWAY_REGISTER,GATEWAY_RELEASE]}
    ]})).unwrap();
    let now = unibus::envelope::now_ms() / 1000;
    let resource = json!({"scope":{"kind":"peer","id":"test-host"},"key":"test-app"});
    let gateway:GatewayConfig=serde_json::from_value(json!({"gateway_peer_id":"test-host","policy_expires_at_unix_s":now+60,
        "routes":[{"backend_producer":"backend","lease":{"schema_version":1,"resource":resource,"gateway_peer_id":"test-host","source_service_id":"test/app",
            "endpoint":{"peer_id":"unibus-local","endpoints":[{"transport":"quic","host":"gateway.test","port":9999}],"expires_at_unix_s":now+60},
            "generation":1,"observed_at_unix_s":now,"expires_at_unix_s":now+60,"operations":["app.read"],
            "native_security":{"encryption":"absent","authentication":"present"}}}],
        "grants":[{"caller_producer":"caller","auth_subject":"test/caller","resource":resource,"operations":["app.read"],"expires_at_unix_s":now+60}]
    })).unwrap();
    let initial_lease = gateway.routes[0].lease.clone();
    let hub = Arc::new(
        Hub::new(config)
            .unwrap()
            .with_offline_verifier(Arc::new(FixtureVerifier))
            .with_plugin(Arc::new(GatewayRouter::new(gateway, now).unwrap())),
    );
    let addr = hub.start().unwrap();
    let mut backend = connect(addr, "backend");
    let mut registration = Envelope::new(
        "test",
        "backend",
        None,
        "",
        Message::new(
            GATEWAY_REGISTER,
            serde_json::to_value(GatewayRegistration {
                lease: initial_lease.clone(),
            })
            .unwrap(),
        ),
    );
    registration.schema_version = 2;
    write_frame(&mut backend, &registration, &[]).unwrap();
    assert_eq!(receive(&mut backend, "reply").message.body["success"], true);
    let mut caller = connect(addr, "caller");
    let invoke:GatewayInvoke=serde_json::from_value(json!({"schema_version":1,"request_id":"request-1","resource":resource,"generation":1,"operation":"app.read","input":{}})).unwrap();
    let mut request = Envelope::new(
        "test",
        "caller",
        None,
        "",
        Message::new(GATEWAY_INVOKE, serde_json::to_value(&invoke).unwrap()),
    );
    request.schema_version = 2;
    write_frame(&mut caller, &request, &[]).unwrap();
    let dispatched = receive(&mut backend, GATEWAY_INVOKE);
    assert_eq!(dispatched.producer_id, "caller");
    assert!(dispatched.token.is_empty());
    let dispatch: GatewayDispatch =
        serde_json::from_value(serde_json::Value::Object(dispatched.message.body)).unwrap();
    assert_eq!(dispatch.caller_producer, "caller");
    let result = GatewayResult {
        caller_producer: dispatch.caller_producer,
        request_id: dispatch.invocation.request_id,
        resource: dispatch.invocation.resource,
        generation: 1,
        output: json!({"value":42}),
    };
    let mut reply = Envelope::new(
        "test",
        "backend",
        None,
        "",
        Message::new(GATEWAY_RESULT, serde_json::to_value(result).unwrap()),
    );
    reply.schema_version = 2;
    write_frame(&mut backend, &reply, &[]).unwrap();
    assert_eq!(
        receive(&mut caller, GATEWAY_RESULT).message.body["output"]["value"],
        42
    );
    let mut denied = invoke;
    denied.request_id = "request-2".into();
    denied.operation = "app.admin".into();
    request.message = Message::new(GATEWAY_INVOKE, serde_json::to_value(denied).unwrap());
    write_frame(&mut caller, &request, &[]).unwrap();
    // Ignore earlier successful invoke acknowledgements, then require a denial.
    let mut denied = false;
    for _ in 0..4 {
        let ack = receive(&mut caller, "reply");
        if ack.message.body["success"] == false {
            denied = true;
            break;
        }
    }
    assert!(denied);
    assert_eq!(receive(&mut backend, "reply").message.body["success"], true);
    registration.message = Message::new(
        GATEWAY_RELEASE,
        serde_json::to_value(GatewayRelease {
            resource: initial_lease.resource,
            generation: 1,
        })
        .unwrap(),
    );
    write_frame(&mut backend, &registration, &[]).unwrap();
    assert_eq!(receive(&mut backend, "reply").message.body["success"], true);
    request
        .message
        .body
        .insert("request_id".into(), "request-3".into());
    request
        .message
        .body
        .insert("operation".into(), "app.read".into());
    write_frame(&mut caller, &request, &[]).unwrap();
    assert_eq!(receive(&mut caller, "reply").message.body["success"], false);
}
