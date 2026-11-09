//! Projection from the room service's typed state to the flat Canvas snapshot.
//!
//! The room service carries membership, presence and bus declarations. This
//! module is the one place that decides how those facts become the Rooms Spore
//! input shape, so the service and the UI cannot drift independently.

use serde_json::{Value, json};

use crate::{
    Bus, BusClass, ListReply, Member, MemberChange, MemberEvent, Mic, Presence, Presenter, Role,
    RoomMessage, RoomSummary, Welcome,
};

#[derive(Debug, Default)]
pub struct Projection {
    welcome: Option<Welcome>,
    rooms: Vec<RoomSummary>,
    error: Option<String>,
    selected: Option<String>,
    revision: u64,
    messages: Vec<Value>,
}

impl Projection {
    pub fn welcome(&self) -> Option<&Welcome> {
        self.welcome.as_ref()
    }

    pub fn revision(&self) -> u64 {
        self.revision
    }

    pub fn room(&self) -> Option<&str> {
        self.welcome.as_ref().map(|welcome| welcome.room.as_str())
    }

    pub fn set_selected(&mut self, selected: Option<String>) {
        self.selected = selected;
    }

    pub fn set_error(&mut self, error: Option<String>) {
        self.error = error;
    }

    /// Add one locally-authored transcript message. The room transcript is
    /// opt-in and lives outside the room service; the projection only carries
    /// what the current room may render.
    pub fn add_message(&mut self, room: &str, from: &str, text: &str, mine: bool, id: &str) {
        self.push_message(id, room, from, text, mine);
    }

    /// Apply one service-authored room message. The service already stamped
    /// the sender, room and id; the presenter only decides whether it is mine.
    pub fn apply_message(&mut self, message: RoomMessage) {
        self.revision = self.revision.max(message.revision);
        let Some(welcome) = &self.welcome else {
            return;
        };
        if welcome.room != message.room {
            return;
        }
        let mine = message.from == welcome.me.id;
        self.push_message(
            &message.id,
            &message.room,
            &message.display_name,
            &message.text,
            mine,
        );
    }

    fn push_message(&mut self, id: &str, room: &str, from: &str, text: &str, mine: bool) {
        if self
            .messages
            .iter()
            .any(|message| message.get("id").and_then(Value::as_str) == Some(id))
        {
            return;
        }
        self.messages.push(json!({
            "id": id,
            "room": room,
            "from": from,
            "text": text,
            "mine": mine,
        }));
        let excess = self.messages.len().saturating_sub(200);
        if excess > 0 {
            self.messages.drain(..excess);
        }
    }

    pub fn apply_welcome(&mut self, mut welcome: Welcome) {
        self.revision = self.revision.max(welcome.revision);
        refresh_bus_publishers(&mut welcome);
        if self.welcome.as_ref().map(|current| current.room.as_str()) != Some(welcome.room.as_str())
        {
            self.messages.clear();
        }
        let self_id = welcome.me.id.clone();
        let history = welcome.messages.clone();
        for message in history {
            self.push_message(
                &message.id,
                &message.room,
                &message.display_name,
                &message.text,
                message.from == self_id,
            );
        }
        if let Some(room) = self.rooms.iter_mut().find(|room| room.room == welcome.room) {
            room.members = welcome.members.len() as u32;
            room.revision = welcome.revision;
            room.space.clone_from(&welcome.space);
        } else {
            self.rooms.push(RoomSummary {
                room: welcome.room.clone(),
                space: welcome.space.clone(),
                members: welcome.members.len() as u32,
                revision: welcome.revision,
            });
            self.rooms.sort_by(|left, right| left.room.cmp(&right.room));
        }
        self.welcome = Some(welcome);
        self.error = None;
    }

    pub fn set_self(&mut self, mic: Mic, speaking: bool, presenter: Option<Presenter>) {
        let Some(welcome) = self.welcome.as_mut() else {
            return;
        };
        welcome.me.mic = mic;
        welcome.me.speaking = speaking;
        if let Some(presenter) = presenter {
            welcome.me.presenter = presenter;
        }
        let self_id = welcome.me.id.clone();
        if let Some(member) = welcome
            .members
            .iter_mut()
            .find(|member| member.id == self_id)
        {
            member.mic = mic;
            member.speaking = speaking;
            if let Some(presenter) = presenter {
                member.presenter = presenter;
            }
        }
        refresh_bus_publishers(welcome);
    }

    pub fn clear_room(&mut self) {
        if let Some(welcome) = self.welcome.take() {
            self.revision = self.revision.max(welcome.revision);
        }
        self.messages.clear();
    }

    pub fn apply_member(&mut self, event: MemberEvent) {
        self.revision = self.revision.max(event.revision);
        let Some(welcome) = self.welcome.as_mut() else {
            return;
        };
        if welcome.room != event.room {
            return;
        }
        match event.change {
            MemberChange::Joined | MemberChange::Updated => {
                if let Some(member) = welcome
                    .members
                    .iter_mut()
                    .find(|member| member.id == event.member.id)
                {
                    *member = event.member.clone();
                } else {
                    welcome.members.push(event.member.clone());
                }
                if welcome.me.id == event.member.id {
                    welcome.me = event.member;
                }
            }
            MemberChange::Left => {
                welcome
                    .members
                    .retain(|member| member.id != event.member.id);
            }
        }
        refresh_bus_publishers(welcome);
        if let Some(summary) = self.rooms.iter_mut().find(|room| room.room == welcome.room) {
            summary.members = welcome.members.len() as u32;
            summary.revision = welcome.revision;
        }
    }

    pub fn apply_list(&mut self, list: ListReply) {
        self.rooms = list.rooms;
        if let Some(welcome) = &self.welcome
            && !self.rooms.iter().any(|room| room.room == welcome.room)
        {
            self.rooms.push(RoomSummary {
                room: welcome.room.clone(),
                space: welcome.space.clone(),
                members: welcome.members.len() as u32,
                revision: welcome.revision,
            });
        }
        self.rooms.sort_by(|left, right| left.room.cmp(&right.room));
    }

    /// The complete module input. `call` is host-owned and has the same shape
    /// whether the local harness is idle, starting, connected, muted or failed.
    pub fn snapshot(
        &self,
        workspace: &str,
        self_name: &str,
        connected: bool,
        call: Value,
    ) -> Value {
        let rooms = self.rooms.iter().map(room_summary).collect::<Vec<_>>();
        let (room, me, self_room, self_id) = match &self.welcome {
            Some(welcome) => (
                Some(room_value(welcome, &self.messages)),
                Some(member_value(&welcome.me)),
                Value::String(welcome.room.clone()),
                Value::String(welcome.me.id.clone()),
            ),
            None => (None, None, Value::Null, Value::String("self".into())),
        };
        let me = me.unwrap_or_else(|| {
            json!({
                "id": "self",
                "name": self_name,
                "kind": "human",
                "presence": "active",
                "speaking": false,
                "mic": "on"
            })
        });
        let self_value = json!({
            "member": self_id,
            "name": self_name,
            "room": self_room,
            "mic": me.get("mic").cloned().unwrap_or_else(|| json!("on"))
        });
        let mut out = json!({
            "revision": self.revision,
            "live": true,
            "workspace": {
                "name": workspace,
                "presence": if connected { "connected" } else { "offline" }
            },
            "call": call,
            "rooms": rooms,
            "self": self_value,
            "room": room,
            "view": { "selected": self.selected }
        });
        if let Some(error) = &self.error {
            out["error"] = Value::String(error.clone());
        }
        out
    }
}

fn refresh_bus_publishers(welcome: &mut Welcome) {
    for bus in &mut welcome.buses {
        bus.publishers = welcome
            .members
            .iter()
            .filter(|member| member.audio == bus.class)
            .count() as u32;
    }
}

fn member_value(member: &Member) -> Value {
    let kind = match member.role {
        Role::Agent => "agent",
        Role::Owner | Role::Member if member.audio == BusClass::Program => "bot",
        Role::Owner | Role::Member => "human",
    };
    let mut out = json!({
        "id": member.id,
        "name": member.display_name,
        "kind": kind,
        "presence": presence(member.presence),
        "speaking": member.speaking,
        "mic": mic(member.mic),
        "presenter": presenter(member.presenter),
        "audio": bus_class(member.audio)
    });
    if member.role == Role::Agent {
        out["agent_state"] = Value::String("addressed".into());
    }
    if member.audio == BusClass::Program {
        out["buffered"] = Value::Bool(true);
        out["state"] = Value::String("idle".into());
    }
    out
}

fn room_value(welcome: &Welcome, messages: &[Value]) -> Value {
    json!({
        "id": welcome.room,
        "name": display_name(&welcome.room),
        "joined": true,
        "space": welcome.space,
        "buses": welcome.buses.iter().map(bus_value).collect::<Vec<_>>(),
        "members": welcome.members.iter().map(member_value).collect::<Vec<_>>(),
        "screens": Vec::<Value>::new(),
        "messages": messages
            .iter()
            .filter(|message| message["room"] == welcome.room)
            .cloned()
            .collect::<Vec<_>>()
    })
}

fn room_summary(room: &RoomSummary) -> Value {
    json!({
        "id": room.room,
        "name": display_name(&room.room),
        "kind": "voice",
        "members": room.members,
        "unread": 0,
        "vr": 0
    })
}

fn bus_value(bus: &Bus) -> Value {
    json!({
        "class": bus_class(bus.class),
        "buffer_ms": bus.buffer_ms,
        "publishers": bus.publishers,
        "lossless": bus.lossless
    })
}

fn display_name(room: &str) -> String {
    room.split(['-', '_'])
        .filter(|part| !part.is_empty())
        .map(|part| {
            let mut chars = part.chars();
            match chars.next() {
                Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn bus_class(class: BusClass) -> &'static str {
    match class {
        BusClass::Speech => "speech",
        BusClass::Program => "program",
    }
}

fn presence(presence: Presence) -> &'static str {
    match presence {
        Presence::Active => "active",
        Presence::Idle => "idle",
        Presence::Away => "away",
    }
}

fn presenter(presenter: Presenter) -> &'static str {
    match presenter {
        Presenter::Canvas => "canvas",
        Presenter::Holodeck => "holodeck",
    }
}

fn mic(mic: Mic) -> &'static str {
    match mic {
        Mic::On => "on",
        Mic::Off => "off",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Member, Presence, Presenter, Role, Welcome};

    fn member(id: &str, name: &str, role: Role, audio: BusClass) -> Member {
        Member {
            id: id.into(),
            display_name: name.into(),
            role,
            presenter: Presenter::Canvas,
            presence: Presence::Active,
            mic: Mic::On,
            speaking: false,
            audio,
            joined_at_ms: 1,
        }
    }

    fn welcome() -> Welcome {
        let me = member("avery", "Avery", Role::Owner, BusClass::Speech);
        Welcome {
            revision: 7,
            room: "listening-room".into(),
            space: "listening-room".into(),
            me: me.clone(),
            members: vec![
                me,
                member("jarvis", "JARVIS", Role::Agent, BusClass::Speech),
            ],
            buses: vec![
                Bus {
                    class: BusClass::Speech,
                    buffer_ms: 40,
                    publishers: 2,
                    lossless: false,
                },
                Bus {
                    class: BusClass::Program,
                    buffer_ms: 250,
                    publishers: 0,
                    lossless: true,
                },
            ],
            messages: Vec::new(),
        }
    }

    #[test]
    fn snapshot_is_the_flat_rooms_shape() {
        let mut projection = Projection::default();
        projection.apply_welcome(welcome());
        projection.apply_list(ListReply {
            rooms: vec![RoomSummary {
                room: "listening-room".into(),
                space: "listening-room".into(),
                members: 2,
                revision: 7,
            }],
        });
        let snapshot = projection.snapshot("FPL Rooms", "Avery", true, json!({"state":"idle"}));
        assert_eq!(snapshot["live"], true);
        assert_eq!(snapshot["room"]["name"], "Listening Room");
        assert_eq!(snapshot["room"]["space"], "listening-room");
        assert_eq!(snapshot["room"]["members"].as_array().unwrap().len(), 2);
        assert_eq!(snapshot["room"]["members"][1]["kind"], "agent");
        assert_eq!(snapshot["room"]["members"][1]["agent_state"], "addressed");
        assert_eq!(snapshot["rooms"][0]["id"], "listening-room");
        assert_eq!(snapshot["self"]["room"], "listening-room");
    }

    #[test]
    fn member_leave_and_self_update_keep_the_room_consistent() {
        let mut projection = Projection::default();
        projection.apply_welcome(welcome());
        let mut jarvis = member("jarvis", "JARVIS", Role::Agent, BusClass::Speech);
        jarvis.speaking = true;
        projection.apply_member(MemberEvent {
            revision: 8,
            room: "listening-room".into(),
            change: MemberChange::Updated,
            member: jarvis,
        });
        assert!(projection.welcome().unwrap().members[1].speaking);
        projection.apply_member(MemberEvent {
            revision: 9,
            room: "listening-room".into(),
            change: MemberChange::Left,
            member: member("jarvis", "JARVIS", Role::Agent, BusClass::Speech),
        });
        assert_eq!(projection.welcome().unwrap().members.len(), 1);
        projection.clear_room();
        let snapshot = projection.snapshot("FPL Rooms", "Avery", true, json!({"state":"idle"}));
        assert!(snapshot["room"].is_null());
        assert_eq!(snapshot["self"]["room"], Value::Null);
    }

    #[test]
    fn transcript_messages_are_scoped_to_the_current_room() {
        let mut projection = Projection::default();
        projection.apply_welcome(welcome());
        projection.add_message("listening-room", "Avery", "hello", true, "a-1");
        projection.add_message("workshop", "Avery", "other room", true, "a-2");
        let snapshot = projection.snapshot("FPL Rooms", "Avery", true, json!({"state":"idle"}));
        let messages = snapshot["room"]["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0]["text"], "hello");
        assert_eq!(messages[0]["mine"], true);
    }

    #[test]
    fn federated_messages_seed_from_welcome_and_dedupe() {
        let mut projection = Projection::default();
        let mut welcome = welcome();
        welcome.messages = vec![RoomMessage {
            id: "say-listening-room-7".into(),
            revision: 7,
            room: "listening-room".into(),
            from: "avery".into(),
            display_name: "Avery".into(),
            text: "hello".into(),
            sent_at_ms: 1,
        }];
        projection.apply_welcome(welcome);
        let live = RoomMessage {
            id: "say-listening-room-8".into(),
            revision: 8,
            room: "listening-room".into(),
            from: "jarvis".into(),
            display_name: "JARVIS".into(),
            text: "ready".into(),
            sent_at_ms: 2,
        };
        projection.apply_message(live.clone());
        projection.apply_message(live);
        let snapshot = projection.snapshot("FPL Rooms", "Avery", true, json!({"state":"idle"}));
        let messages = snapshot["room"]["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0]["from"], "Avery");
        assert_eq!(messages[0]["mine"], true);
        assert_eq!(messages[1]["from"], "JARVIS");
        assert_eq!(messages[1]["mine"], false);
    }

    #[test]
    fn a_program_publisher_appears_as_a_bot() {
        let mut projection = Projection::default();
        let mut welcome = welcome();
        welcome
            .members
            .push(member("hifi", "Hi-Fi", Role::Member, BusClass::Program));
        projection.apply_welcome(welcome);
        let snapshot = projection.snapshot("FPL Rooms", "Avery", true, json!({"state":"idle"}));
        let bot = snapshot["room"]["members"]
            .as_array()
            .unwrap()
            .iter()
            .find(|member| member["id"] == "hifi")
            .unwrap();
        assert_eq!(bot["kind"], "bot");
        assert_eq!(bot["buffered"], true);
        assert_eq!(snapshot["room"]["buses"][1]["publishers"], 1);
    }
}
