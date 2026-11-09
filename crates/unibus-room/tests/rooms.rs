//! End-to-end room service over a real loopback hub: registration, grants,
//! join, presence, presenter switch, directory and leave.

use std::{
    net::SocketAddr,
    sync::Arc,
    thread::sleep,
    time::{Duration, Instant},
};

use serde_json::json;
use unibus::{
    Envelope, Message,
    client::{Client, ClientConfig},
};
use unibus_room::{
    ADDRESS_KIND, ADDRESSED_KIND, Addressed, BusClass, ERROR_KIND, JOIN_KIND, Join, LEAVE_KIND,
    LIST_KIND, LIST_REPLY_KIND, ListReply, MEMBER_KIND, MESSAGE_KIND, MemberChange, MemberEvent,
    Presence, Presenter, Role, RoomMessage, SAY_KIND, SELF_KIND, WELCOME_KIND, Welcome,
    plugin::RoomPlugin,
};
use unibus_router::{Hub, config::HubConfig};

const TOKEN: &str = "0123456789abcdef0123456789abcdef";

struct TestHub {
    addr: SocketAddr,
    _hub: Arc<Hub>,
    dir: tempfile::TempDir,
}

fn start() -> TestHub {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("token"), TOKEN).unwrap();
    let config_json = r#"{
        "installation_id": "home",
        "listen": "127.0.0.1:0",
        "nodes": [
            {"producer_id":"avery","token_file":"token","zones":["default"],"capabilities":["room.member","room.owner"],"publishes":["room.join.v1","room.say.v1","room.address.v1","room.leave.v1","room.self.v1","room.list.v1"],"publish_zones":["default"]},
            {"producer_id":"james","token_file":"token","zones":["default"],"capabilities":["room.member"],"publishes":["room.join.v1","room.say.v1","room.address.v1","room.leave.v1","room.self.v1","room.list.v1"],"publish_zones":["default"]},
            {"producer_id":"intruder","token_file":"token","zones":["default"],"capabilities":["room.observer"],"publishes":["room.join.v1"],"publish_zones":["default"]},
            {"producer_id":"jarvis","token_file":"token","zones":["default"],"capabilities":["room.member","room.agent"],"publishes":["room.join.v1","room.leave.v1","room.self.v1"],"publish_zones":["default"]}
        ]
    }"#;
    std::fs::write(dir.path().join("hub.json"), config_json).unwrap();
    let config = HubConfig::read(&dir.path().join("hub.json")).unwrap();
    let hub = Arc::new(
        Hub::new(config)
            .unwrap()
            .with_plugin(Arc::new(RoomPlugin::new())),
    );
    let addr = hub.start().unwrap();
    TestHub {
        addr,
        _hub: hub,
        dir,
    }
}

fn client(hub: &TestHub, producer: &str, capabilities: &[&str]) -> Client {
    let config: ClientConfig = serde_json::from_value(json!({
        "installation_id": "home",
        "producer_id": producer,
        "hub_endpoint": hub.addr.to_string(),
        "token_file": hub.dir.path().join("token").to_string_lossy().to_string(),
        "zones": ["default"],
        "capabilities": capabilities,
        "schema_version": 2,
        "read_timeout_ms": 50
    }))
    .unwrap();
    Client::connect(config).unwrap()
}

fn send(client: &Client, kind: &str, body: serde_json::Value) {
    let envelope = client.envelope(None, Message::new(kind, body)).unwrap();
    client.send(&envelope, &[]).unwrap();
}

fn recv_kind(client: &Client, kind: &str) -> Envelope {
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        match client.try_read() {
            Ok(Some((envelope, _))) if envelope.message.kind == kind => return envelope,
            Ok(Some(_)) => {}
            Ok(None) => sleep(Duration::from_millis(10)),
            Err(error) => panic!("connection lost: {error:#}"),
        }
    }
    panic!("never saw {kind}");
}

fn decode<T: serde::de::DeserializeOwned>(envelope: &Envelope) -> T {
    serde_json::from_value(serde_json::Value::Object(envelope.message.body.clone())).unwrap()
}

fn join(room: &str, name: &str, presenter: Presenter, audio: BusClass) -> serde_json::Value {
    serde_json::to_value(Join {
        room: room.into(),
        space: None,
        display_name: name.into(),
        presenter,
        presence: Presence::Active,
        audio,
    })
    .unwrap()
}

#[test]
fn two_clients_join_and_see_each_other() {
    let hub = start();
    let avery = client(&hub, "avery", &["room.member"]);
    let james = client(&hub, "james", &["room.member"]);

    send(
        &avery,
        JOIN_KIND,
        join("studio", "Avery", Presenter::Canvas, BusClass::Speech),
    );
    let first: Welcome = decode(&recv_kind(&avery, WELCOME_KIND));
    assert_eq!(first.room, "studio");
    assert_eq!(first.space, "studio", "space defaults to the room id");
    assert_eq!(first.me.role, Role::Owner);
    assert_eq!(first.members.len(), 1);
    assert!(
        first
            .buses
            .iter()
            .any(|bus| bus.class == BusClass::Speech && bus.publishers == 1),
        "speech publisher count"
    );

    send(
        &james,
        JOIN_KIND,
        join("studio", "James", Presenter::Holodeck, BusClass::Program),
    );
    let second: Welcome = decode(&recv_kind(&james, WELCOME_KIND));
    assert_eq!(second.members.len(), 2);
    assert_eq!(second.me.role, Role::Member);
    assert_eq!(second.me.presenter, Presenter::Holodeck);
    assert!(
        second
            .buses
            .iter()
            .any(|bus| bus.class == BusClass::Program && bus.publishers == 1),
        "program publisher count"
    );

    let joined: MemberEvent = decode(&recv_kind(&avery, MEMBER_KIND));
    assert_eq!(joined.change, MemberChange::Joined);
    assert_eq!(joined.member.id, "james");
    assert_eq!(joined.member.presenter, Presenter::Holodeck);

    send(
        &james,
        SELF_KIND,
        json!({"presence":"idle","mic":"on","speaking":true}),
    );
    let updated: MemberEvent = decode(&recv_kind(&avery, MEMBER_KIND));
    assert_eq!(updated.change, MemberChange::Updated);
    assert!(updated.member.speaking);
    assert_eq!(updated.member.presence, Presence::Idle);

    send(&james, LIST_KIND, json!({}));
    let list: ListReply = decode(&recv_kind(&james, LIST_REPLY_KIND));
    assert_eq!(list.rooms.len(), 1);
    assert_eq!(list.rooms[0].room, "studio");
    assert_eq!(list.rooms[0].members, 2);

    send(&james, LEAVE_KIND, json!({}));
    let left: MemberEvent = decode(&recv_kind(&avery, MEMBER_KIND));
    assert_eq!(left.change, MemberChange::Left);
    assert_eq!(left.member.id, "james");
}

#[test]
fn room_messages_reach_every_member_and_late_joiners() {
    let hub = start();
    let avery = client(&hub, "avery", &["room.member"]);
    let james = client(&hub, "james", &["room.member"]);

    send(
        &avery,
        JOIN_KIND,
        join("studio", "Avery", Presenter::Canvas, BusClass::Speech),
    );
    let _: Welcome = decode(&recv_kind(&avery, WELCOME_KIND));
    send(
        &james,
        JOIN_KIND,
        join("studio", "James", Presenter::Canvas, BusClass::Speech),
    );
    let _: Welcome = decode(&recv_kind(&james, WELCOME_KIND));
    let _: MemberEvent = decode(&recv_kind(&avery, MEMBER_KIND));

    send(&avery, SAY_KIND, json!({"text": "hello room"}));
    let from_avery: RoomMessage = decode(&recv_kind(&avery, MESSAGE_KIND));
    let at_james: RoomMessage = decode(&recv_kind(&james, MESSAGE_KIND));
    assert_eq!(from_avery, at_james);
    assert_eq!(at_james.room, "studio");
    assert_eq!(at_james.from, "avery");
    assert_eq!(at_james.display_name, "Avery");
    assert_eq!(at_james.text, "hello room");
    assert!(at_james.revision > 0);

    let jarvis = client(&hub, "jarvis", &["room.member", "room.agent"]);
    send(
        &jarvis,
        JOIN_KIND,
        join("studio", "JARVIS", Presenter::Canvas, BusClass::Speech),
    );
    let welcome: Welcome = decode(&recv_kind(&jarvis, WELCOME_KIND));
    assert_eq!(welcome.messages, vec![at_james]);
}

#[test]
fn a_node_without_room_member_is_rejected() {
    let hub = start();
    let intruder = client(&hub, "intruder", &[]);
    send(
        &intruder,
        JOIN_KIND,
        join("studio", "Intruder", Presenter::Canvas, BusClass::Speech),
    );
    let error: serde_json::Value = decode(&recv_kind(&intruder, ERROR_KIND));
    assert_eq!(error["code"], "unauthorized");
}

#[test]
fn joining_another_room_moves_the_member() {
    let hub = start();
    let avery = client(&hub, "avery", &["room.member"]);
    let james = client(&hub, "james", &["room.member"]);
    send(
        &avery,
        JOIN_KIND,
        join("studio", "Avery", Presenter::Canvas, BusClass::Speech),
    );
    let _: Welcome = decode(&recv_kind(&avery, WELCOME_KIND));
    send(
        &james,
        JOIN_KIND,
        join("studio", "James", Presenter::Canvas, BusClass::Speech),
    );
    let _: Welcome = decode(&recv_kind(&james, WELCOME_KIND));
    let _: MemberEvent = decode(&recv_kind(&avery, MEMBER_KIND));

    send(
        &james,
        JOIN_KIND,
        join("workshop", "James", Presenter::Canvas, BusClass::Speech),
    );
    let moved: Welcome = decode(&recv_kind(&james, WELCOME_KIND));
    assert_eq!(moved.room, "workshop");
    assert_eq!(moved.members.len(), 1);
    let left: MemberEvent = decode(&recv_kind(&avery, MEMBER_KIND));
    assert_eq!(left.change, MemberChange::Left);
    assert_eq!(left.room, "studio");
}

#[test]
fn addressed_turns_go_only_to_the_target_agent() {
    let hub = start();
    let avery = client(&hub, "avery", &["room.member"]);
    let jarvis = client(&hub, "jarvis", &["room.member", "room.agent"]);

    send(
        &avery,
        JOIN_KIND,
        join("studio", "Avery", Presenter::Canvas, BusClass::Speech),
    );
    let avery_welcome: Welcome = decode(&recv_kind(&avery, WELCOME_KIND));
    assert_eq!(avery_welcome.me.role, Role::Owner);
    send(
        &jarvis,
        JOIN_KIND,
        join("studio", "JARVIS", Presenter::Canvas, BusClass::Speech),
    );
    let jarvis_welcome: Welcome = decode(&recv_kind(&jarvis, WELCOME_KIND));
    assert_eq!(jarvis_welcome.me.role, Role::Agent);

    send(
        &avery,
        ADDRESS_KIND,
        json!({"target":"jarvis","text":"Report the room status."}),
    );
    let addressed: Addressed = decode(&recv_kind(&jarvis, ADDRESSED_KIND));
    assert_eq!(addressed.room, "studio");
    assert_eq!(addressed.target, "jarvis");
    assert_eq!(addressed.from, "avery");
    assert_eq!(addressed.text, "Report the room status.");
    assert!(addressed.turn_id > 0);

    send(
        &avery,
        ADDRESS_KIND,
        json!({"target":"avery","text":"This is not an agent."}),
    );
    let error: serde_json::Value = decode(&recv_kind(&avery, ERROR_KIND));
    assert_eq!(error["code"], "invalid");
}
