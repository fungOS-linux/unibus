//! Room membership, presence and audio-bus contracts for Unibus.
//!
//! A room is one shared space with two presenters: the flat Canvas view and
//! Holodeck. Membership, presence, the `space` id and the audio buses are
//! shared; the presenter is not. [`Rooms`] is the pure state machine; the hub
//! plugin in [`plugin`] owns it for a self-hosted room service.
//!
//! Identity is the authenticated Unibus producer id. A client never names
//! another member and cannot assert a role: roles come from the node grant.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

pub mod plugin;

pub const JOIN_KIND: &str = "room.join.v1";
pub const LEAVE_KIND: &str = "room.leave.v1";
pub const SELF_KIND: &str = "room.self.v1";
pub const LIST_KIND: &str = "room.list.v1";
pub const WELCOME_KIND: &str = "room.welcome.v1";
pub const MEMBER_KIND: &str = "room.member.v1";
pub const LIST_REPLY_KIND: &str = "room.list.reply.v1";
pub const ERROR_KIND: &str = "room.error.v1";

/// Every kind this service owns. A client publishes only the four request
/// kinds; the rest are service-authored.
pub const REQUEST_KINDS: &[&str] = &[JOIN_KIND, LEAVE_KIND, SELF_KIND, LIST_KIND];

pub const CAP_MEMBER: &str = "room.member";
pub const CAP_OWNER: &str = "room.owner";
pub const CAP_AGENT: &str = "room.agent";

pub const SCHEMA_VERSION: u32 = 1;
pub const MAX_ROOMS: usize = 64;
pub const MAX_MEMBERS: usize = 32;
pub const MAX_BUSES: usize = 4;
pub const MAX_ID: usize = 64;
pub const MAX_NAME: usize = 64;
pub const MAX_ERROR: usize = 160;
pub const MIN_BUFFER_MS: u32 = 10;
pub const MAX_BUFFER_MS: u32 = 2_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Presenter {
    #[default]
    Canvas,
    Holodeck,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Presence {
    #[default]
    Active,
    Idle,
    Away,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Mic {
    #[default]
    On,
    Off,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    Owner,
    #[default]
    Member,
    Agent,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BusClass {
    #[default]
    Speech,
    Program,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemberChange {
    Joined,
    Left,
    Updated,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    Invalid,
    NotJoined,
    RoomFull,
    TooManyRooms,
    Unauthorized,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoomError {
    pub code: ErrorCode,
    pub message: String,
}

impl RoomError {
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        let message: String = message.into().chars().take(MAX_ERROR).collect();
        Self { code, message }
    }

    pub fn invalid(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::Invalid, message)
    }

    pub fn to_value(&self) -> Value {
        json!({ "code": self.code, "message": self.message })
    }
}

impl fmt::Display for RoomError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}: {}", self.code, self.message)
    }
}

impl std::error::Error for RoomError {}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Member {
    /// Authenticated producer id. Never accepted from the client body.
    pub id: String,
    pub display_name: String,
    pub role: Role,
    pub presenter: Presenter,
    pub presence: Presence,
    pub mic: Mic,
    pub speaking: bool,
    pub audio: BusClass,
    pub joined_at_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Join {
    pub room: String,
    /// Omitted means the room id is the space id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub space: Option<String>,
    pub display_name: String,
    #[serde(default)]
    pub presenter: Presenter,
    #[serde(default)]
    pub presence: Presence,
    #[serde(default)]
    pub audio: BusClass,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SelfState {
    pub presence: Presence,
    pub mic: Mic,
    pub speaking: bool,
    /// A member may switch presenters without changing membership.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub presenter: Option<Presenter>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Bus {
    pub class: BusClass,
    pub buffer_ms: u32,
    pub publishers: u32,
    pub lossless: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Welcome {
    pub revision: u64,
    pub room: String,
    pub space: String,
    #[serde(rename = "self")]
    pub me: Member,
    /// Every member, including `self`.
    pub members: Vec<Member>,
    pub buses: Vec<Bus>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MemberEvent {
    pub revision: u64,
    pub room: String,
    pub change: MemberChange,
    pub member: Member,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RoomSummary {
    pub room: String,
    pub space: String,
    pub members: u32,
    pub revision: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ListReply {
    pub rooms: Vec<RoomSummary>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BusSpec {
    pub class: BusClass,
    pub buffer_ms: u32,
    pub lossless: bool,
}

#[derive(Debug, Clone)]
pub struct RoomConfig {
    pub max_rooms: usize,
    pub max_members: usize,
    pub buses: Vec<BusSpec>,
}

impl Default for RoomConfig {
    fn default() -> Self {
        Self {
            max_rooms: MAX_ROOMS,
            max_members: MAX_MEMBERS,
            buses: vec![
                BusSpec {
                    class: BusClass::Speech,
                    buffer_ms: 40,
                    lossless: false,
                },
                BusSpec {
                    class: BusClass::Program,
                    buffer_ms: 250,
                    lossless: true,
                },
            ],
        }
    }
}

impl RoomConfig {
    pub fn validate(&self) -> Result<(), RoomError> {
        if !(1..=MAX_ROOMS).contains(&self.max_rooms) {
            return Err(RoomError::invalid(format!(
                "max_rooms must be 1..={MAX_ROOMS}"
            )));
        }
        if !(1..=MAX_MEMBERS).contains(&self.max_members) {
            return Err(RoomError::invalid(format!(
                "max_members must be 1..={MAX_MEMBERS}"
            )));
        }
        if self.buses.is_empty() || self.buses.len() > MAX_BUSES {
            return Err(RoomError::invalid(format!(
                "buses must contain 1..={MAX_BUSES} entries"
            )));
        }
        let mut seen = BTreeSet::new();
        for bus in &self.buses {
            if !seen.insert(bus.class) {
                return Err(RoomError::invalid("duplicate bus class"));
            }
            if !(MIN_BUFFER_MS..=MAX_BUFFER_MS).contains(&bus.buffer_ms) {
                return Err(RoomError::invalid(format!(
                    "buffer_ms must be {MIN_BUFFER_MS}..={MAX_BUFFER_MS}"
                )));
            }
        }
        if !seen.contains(&BusClass::Speech) {
            return Err(RoomError::invalid(
                "a room needs the interactive speech bus",
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone)]
pub struct JoinOutcome {
    pub welcome: Welcome,
    /// Member left the previous room because a member belongs to one room.
    pub left: Option<MemberEvent>,
    pub joined: Option<MemberEvent>,
}

#[derive(Debug, Clone)]
struct RoomState {
    room: String,
    space: String,
    revision: u64,
    members: BTreeMap<String, Member>,
}

/// Ephemeral rooms. A room disappears when its last member leaves.
#[derive(Debug, Clone)]
pub struct Rooms {
    config: RoomConfig,
    rooms: BTreeMap<String, RoomState>,
}

impl Rooms {
    pub fn new(config: RoomConfig) -> Result<Self, RoomError> {
        config.validate()?;
        Ok(Self {
            config,
            rooms: BTreeMap::new(),
        })
    }

    pub fn config(&self) -> &RoomConfig {
        &self.config
    }

    /// Join one room. A member belongs to one room at a time; joining another
    /// room leaves the first. Rejoining the same room is idempotent and keeps
    /// the revision unchanged.
    pub fn join(
        &mut self,
        id: &str,
        join: Join,
        role: Role,
        now: u64,
    ) -> Result<JoinOutcome, RoomError> {
        validate_id(id, "member")?;
        validate_id(&join.room, "room")?;
        let space = join.space.clone().unwrap_or_else(|| join.room.clone());
        validate_id(&space, "space")?;
        let display_name = validate_name(&join.display_name)?;

        let current = self.room_of(id).map(str::to_owned);
        if current.as_deref() == Some(join.room.as_str()) {
            let welcome = self.welcome(id).ok_or_else(|| {
                RoomError::new(ErrorCode::NotJoined, "member vanished from the room")
            })?;
            return Ok(JoinOutcome {
                welcome,
                left: None,
                joined: None,
            });
        }

        // Capacity and space consistency are checked before any mutation, so a
        // rejected join cannot strand the member outside a room.
        if let Some(room) = self.rooms.get(&join.room) {
            if room.space != space {
                return Err(RoomError::invalid("room space does not match"));
            }
            if room.members.len() >= self.config.max_members {
                return Err(RoomError::new(ErrorCode::RoomFull, "room is full"));
            }
        } else {
            let frees_slot = current
                .as_deref()
                .and_then(|room| self.rooms.get(room))
                .is_some_and(|room| room.members.len() == 1);
            if self.rooms.len() + if frees_slot { 0 } else { 1 } > self.config.max_rooms {
                return Err(RoomError::new(ErrorCode::TooManyRooms, "too many rooms"));
            }
        }

        let left = current
            .as_deref()
            .and_then(|room| self.remove_member(id, room));

        let member = Member {
            id: id.to_owned(),
            display_name,
            role,
            presenter: join.presenter,
            presence: join.presence,
            mic: Mic::On,
            speaking: false,
            audio: join.audio,
            joined_at_ms: now,
        };
        let revision = {
            let room = self
                .rooms
                .entry(join.room.clone())
                .or_insert_with(|| RoomState {
                    room: join.room.clone(),
                    space: space.clone(),
                    revision: 0,
                    members: BTreeMap::new(),
                });
            room.members.insert(id.to_owned(), member.clone());
            room.revision += 1;
            room.revision
        };
        let joined = MemberEvent {
            revision,
            room: join.room.clone(),
            change: MemberChange::Joined,
            member,
        };
        let welcome = self
            .welcome(id)
            .ok_or_else(|| RoomError::new(ErrorCode::NotJoined, "member vanished after join"))?;
        Ok(JoinOutcome {
            welcome,
            left,
            joined: Some(joined),
        })
    }

    pub fn leave(&mut self, id: &str) -> Option<MemberEvent> {
        let room = self.room_of(id)?.to_owned();
        self.remove_member(id, &room)
    }

    pub fn update(&mut self, id: &str, state: SelfState) -> Result<Option<MemberEvent>, RoomError> {
        let room_id = self
            .room_of(id)
            .map(str::to_owned)
            .ok_or_else(|| RoomError::new(ErrorCode::NotJoined, "join a room first"))?;
        let (revision, member) = {
            let room = self
                .rooms
                .get_mut(&room_id)
                .ok_or_else(|| RoomError::new(ErrorCode::NotJoined, "room disappeared"))?;
            let member = room
                .members
                .get_mut(id)
                .ok_or_else(|| RoomError::new(ErrorCode::NotJoined, "member disappeared"))?;
            let presenter = state.presenter.unwrap_or(member.presenter);
            if member.presence == state.presence
                && member.mic == state.mic
                && member.speaking == state.speaking
                && member.presenter == presenter
            {
                return Ok(None);
            }
            member.presence = state.presence;
            member.mic = state.mic;
            member.speaking = state.speaking;
            member.presenter = presenter;
            room.revision += 1;
            (room.revision, member.clone())
        };
        Ok(Some(MemberEvent {
            revision,
            room: room_id,
            change: MemberChange::Updated,
            member,
        }))
    }

    pub fn welcome(&self, id: &str) -> Option<Welcome> {
        let room = self.rooms.get(self.room_of(id)?)?;
        let me = room.members.get(id)?.clone();
        Some(Welcome {
            revision: room.revision,
            room: room.room.clone(),
            space: room.space.clone(),
            me,
            members: room.members.values().cloned().collect(),
            buses: self.buses(room),
        })
    }

    pub fn list(&self) -> ListReply {
        ListReply {
            rooms: self
                .rooms
                .values()
                .map(|room| RoomSummary {
                    room: room.room.clone(),
                    space: room.space.clone(),
                    members: room.members.len() as u32,
                    revision: room.revision,
                })
                .collect(),
        }
    }

    /// Remove members whose producer is no longer connected. Returns the left
    /// events so the plugin can tell the remaining members.
    pub fn prune(&mut self, live: &BTreeSet<String>) -> Vec<MemberEvent> {
        let dead: Vec<(String, String)> = self
            .rooms
            .values()
            .flat_map(|room| {
                room.members
                    .keys()
                    .filter(|id| !live.contains(*id))
                    .map(|id| (room.room.clone(), id.clone()))
                    .collect::<Vec<_>>()
            })
            .collect();
        let mut events = Vec::new();
        for (room, id) in dead {
            if let Some(event) = self.remove_member(&id, &room) {
                events.push(event);
            }
        }
        events
    }

    pub fn members(&self, room: &str) -> Vec<String> {
        self.rooms
            .get(room)
            .map(|room| room.members.keys().cloned().collect())
            .unwrap_or_default()
    }

    pub fn room_of(&self, id: &str) -> Option<&str> {
        self.rooms
            .values()
            .find(|room| room.members.contains_key(id))
            .map(|room| room.room.as_str())
    }

    fn remove_member(&mut self, id: &str, room: &str) -> Option<MemberEvent> {
        let event = {
            let state = self.rooms.get_mut(room)?;
            let member = state.members.remove(id)?;
            state.revision += 1;
            MemberEvent {
                revision: state.revision,
                room: room.to_owned(),
                change: MemberChange::Left,
                member,
            }
        };
        if self
            .rooms
            .get(room)
            .is_some_and(|state| state.members.is_empty())
        {
            self.rooms.remove(room);
        }
        Some(event)
    }

    fn buses(&self, room: &RoomState) -> Vec<Bus> {
        self.config
            .buses
            .iter()
            .map(|spec| Bus {
                class: spec.class,
                buffer_ms: spec.buffer_ms,
                lossless: spec.lossless,
                publishers: room
                    .members
                    .values()
                    .filter(|member| member.audio == spec.class)
                    .count() as u32,
            })
            .collect()
    }
}

/// Room role from the node grant. Capabilities, not client bodies, decide.
pub fn role_for(grant: &unibus::grants::NodeGrant) -> Role {
    if grant
        .capabilities
        .iter()
        .any(|capability| capability == CAP_OWNER)
    {
        Role::Owner
    } else if grant
        .capabilities
        .iter()
        .any(|capability| capability == CAP_AGENT)
    {
        Role::Agent
    } else {
        Role::Member
    }
}

fn validate_id(value: &str, field: &str) -> Result<(), RoomError> {
    if value.len() <= MAX_ID && unibus::grants::is_valid_slug(value) {
        Ok(())
    } else {
        Err(RoomError::invalid(format!(
            "{field} must be a lowercase slug of at most {MAX_ID} bytes"
        )))
    }
}

fn validate_name(value: &str) -> Result<String, RoomError> {
    let value = value.trim();
    if value.is_empty() || value.chars().count() > MAX_NAME || value.chars().any(char::is_control) {
        return Err(RoomError::invalid(format!(
            "display_name must be 1..={MAX_NAME} printable characters"
        )));
    }
    Ok(value.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn join(room: &str, name: &str) -> Join {
        Join {
            room: room.into(),
            space: None,
            display_name: name.into(),
            presenter: Presenter::Canvas,
            presence: Presence::Active,
            audio: BusClass::Speech,
        }
    }

    fn rooms() -> Rooms {
        Rooms::new(RoomConfig::default()).unwrap()
    }

    #[test]
    fn join_is_idempotent_and_defaults_space_to_room() {
        let mut rooms = rooms();
        let first = rooms
            .join("avery", join("studio", "Avery"), Role::Owner, 10)
            .unwrap();
        assert_eq!(first.welcome.space, "studio");
        assert_eq!(first.welcome.revision, 1);
        assert_eq!(first.welcome.me.display_name, "Avery");
        assert_eq!(first.welcome.me.role, Role::Owner);
        let second = rooms
            .join("avery", join("studio", "Changed"), Role::Owner, 20)
            .unwrap();
        assert_eq!(second.welcome.revision, 1);
        assert_eq!(second.welcome.me.display_name, "Avery");
        assert!(second.joined.is_none());
    }

    #[test]
    fn joining_another_room_leaves_the_first() {
        let mut rooms = rooms();
        rooms
            .join("avery", join("studio", "Avery"), Role::Owner, 10)
            .unwrap();
        rooms
            .join("james", join("studio", "James"), Role::Member, 11)
            .unwrap();
        let moved = rooms
            .join("avery", join("workshop", "Avery"), Role::Owner, 12)
            .unwrap();
        assert_eq!(moved.left.as_ref().unwrap().room, "studio");
        assert_eq!(moved.joined.as_ref().unwrap().room, "workshop");
        assert_eq!(rooms.members("studio"), vec!["james"]);
        assert_eq!(rooms.room_of("avery"), Some("workshop"));
    }

    #[test]
    fn capacity_and_validation_fail_before_mutating() {
        let mut rooms = Rooms::new(RoomConfig {
            max_members: 1,
            ..RoomConfig::default()
        })
        .unwrap();
        rooms
            .join("avery", join("studio", "Avery"), Role::Owner, 10)
            .unwrap();
        let full = rooms
            .join("james", join("studio", "James"), Role::Member, 11)
            .unwrap_err();
        assert_eq!(full.code, ErrorCode::RoomFull);
        assert_eq!(rooms.room_of("james"), None);
        assert!(
            rooms
                .join("james", join("Studio", "James"), Role::Member, 12)
                .is_err()
        );
        assert!(
            rooms
                .join(
                    "james",
                    Join {
                        display_name: " ".into(),
                        ..join("workshop", "x")
                    },
                    Role::Member,
                    13
                )
                .is_err()
        );
    }

    #[test]
    fn update_is_revisioned_only_on_change() {
        let mut rooms = rooms();
        rooms
            .join("avery", join("studio", "Avery"), Role::Owner, 10)
            .unwrap();
        let changed = rooms
            .update(
                "avery",
                SelfState {
                    presence: Presence::Idle,
                    mic: Mic::Off,
                    speaking: false,
                    presenter: Some(Presenter::Holodeck),
                },
            )
            .unwrap()
            .unwrap();
        assert_eq!(changed.change, MemberChange::Updated);
        assert_eq!(changed.revision, 2);
        assert_eq!(changed.member.presenter, Presenter::Holodeck);
        let same = rooms
            .update(
                "avery",
                SelfState {
                    presence: Presence::Idle,
                    mic: Mic::Off,
                    speaking: false,
                    presenter: Some(Presenter::Holodeck),
                },
            )
            .unwrap();
        assert!(same.is_none());
        assert_eq!(rooms.welcome("avery").unwrap().revision, 2);
    }

    #[test]
    fn buses_count_publishers_by_class() {
        let mut rooms = rooms();
        rooms
            .join("avery", join("studio", "Avery"), Role::Owner, 10)
            .unwrap();
        rooms
            .join(
                "hifi",
                Join {
                    room: "studio".into(),
                    space: None,
                    display_name: "Hi-Fi".into(),
                    presenter: Presenter::Canvas,
                    presence: Presence::Active,
                    audio: BusClass::Program,
                },
                Role::Member,
                11,
            )
            .unwrap();
        let welcome = rooms.welcome("avery").unwrap();
        let speech = welcome
            .buses
            .iter()
            .find(|bus| bus.class == BusClass::Speech)
            .unwrap();
        let program = welcome
            .buses
            .iter()
            .find(|bus| bus.class == BusClass::Program)
            .unwrap();
        assert_eq!(speech.publishers, 1);
        assert_eq!(program.publishers, 1);
        assert!(program.lossless);
    }

    #[test]
    fn prune_removes_dead_members_and_reports_them() {
        let mut rooms = rooms();
        rooms
            .join("avery", join("studio", "Avery"), Role::Owner, 10)
            .unwrap();
        rooms
            .join("james", join("studio", "James"), Role::Member, 11)
            .unwrap();
        let events = rooms.prune(&BTreeSet::from(["avery".to_owned()]));
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].member.id, "james");
        assert_eq!(events[0].change, MemberChange::Left);
        assert_eq!(rooms.welcome("avery").unwrap().members.len(), 1);
    }

    #[test]
    fn config_requires_the_speech_bus() {
        let config = RoomConfig {
            buses: vec![BusSpec {
                class: BusClass::Program,
                buffer_ms: 250,
                lossless: true,
            }],
            ..RoomConfig::default()
        };
        assert!(config.validate().is_err());
        let duplicate = RoomConfig {
            buses: vec![
                BusSpec {
                    class: BusClass::Speech,
                    buffer_ms: 40,
                    lossless: false,
                },
                BusSpec {
                    class: BusClass::Speech,
                    buffer_ms: 80,
                    lossless: false,
                },
            ],
            ..RoomConfig::default()
        };
        assert!(duplicate.validate().is_err());
    }
}
