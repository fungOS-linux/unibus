//! Hub plugin: the self-hosted room service.
//!
//! The hub routes every `room.*` kind here before default routing. The plugin
//! validates the source's grant, prunes disconnected members, updates the pure
//! [`Rooms`] state machine, and addresses the resulting events directly to the
//! members of that room. No media bytes, addresses or tokens cross this seam.

use std::collections::BTreeSet;
use std::sync::Mutex;

use anyhow::{Context, Result};
use serde::de::DeserializeOwned;
use serde_json::Value;
use unibus::{Envelope, Message};
use unibus_router::plugin::{HubHandle, HubPlugin};

use crate::{
    CAP_MEMBER, ERROR_KIND, ErrorCode, JOIN_KIND, Join, LEAVE_KIND, LIST_KIND, LIST_REPLY_KIND,
    MEMBER_KIND, MemberEvent, RoomConfig, RoomError, Rooms, SELF_KIND, SelfState, WELCOME_KIND,
    role_for,
};

pub const SERVICE_ID: &str = "unibus-room";

pub struct RoomPlugin {
    rooms: Mutex<Rooms>,
    service: String,
}

impl RoomPlugin {
    pub fn new() -> Self {
        Self::with_config(RoomConfig::default()).expect("default room config is valid")
    }

    pub fn with_config(config: RoomConfig) -> Result<Self> {
        Ok(Self {
            rooms: Mutex::new(Rooms::new(config)?),
            service: SERVICE_ID.into(),
        })
    }

    fn send(&self, hub: &HubHandle, destination: &str, kind: &str, body: Value) -> Result<()> {
        let envelope = Envelope::new(
            hub.installation_id(),
            &self.service,
            Some(destination),
            "",
            Message::new(kind, body),
        );
        hub.send_to_producer(destination, &envelope, &[])
    }

    fn send_error(&self, hub: &HubHandle, destination: &str, error: RoomError) {
        let _ = self.send(hub, destination, ERROR_KIND, error.to_value());
    }

    fn broadcast(&self, hub: &HubHandle, rooms: &Rooms, event: &MemberEvent, exclude: &str) {
        let Ok(body) = serde_json::to_value(event) else {
            return;
        };
        for member in rooms.members(&event.room) {
            if member != exclude {
                let _ = self.send(hub, &member, MEMBER_KIND, body.clone());
            }
        }
    }
}

impl Default for RoomPlugin {
    fn default() -> Self {
        Self::new()
    }
}

impl HubPlugin for RoomPlugin {
    fn handle(
        &self,
        hub: &HubHandle,
        source: &str,
        envelope: &Envelope,
        _payload: &[u8],
    ) -> Result<bool> {
        let kind = envelope.message.kind.as_str();
        if !kind.starts_with("room.") {
            return Ok(false);
        }

        let Some(grant) = hub.grant_of(source) else {
            self.send_error(
                hub,
                source,
                RoomError::new(ErrorCode::Unauthorized, "no node grant"),
            );
            return Ok(true);
        };
        if !grant
            .capabilities
            .iter()
            .any(|capability| capability == CAP_MEMBER)
        {
            self.send_error(
                hub,
                source,
                RoomError::new(ErrorCode::Unauthorized, "room.member capability required"),
            );
            return Ok(true);
        }

        let live: BTreeSet<String> = hub
            .nodes_with(CAP_MEMBER)
            .into_iter()
            .map(|delivery| delivery.producer_id)
            .collect();
        let mut rooms = self
            .rooms
            .lock()
            .map_err(|_| anyhow::anyhow!("room state poisoned"))?;
        for event in rooms.prune(&live) {
            self.broadcast(hub, &rooms, &event, source);
        }

        match kind {
            JOIN_KIND => {
                let join = match parse::<Join>(envelope) {
                    Ok(join) => join,
                    Err(error) => {
                        self.send_error(hub, source, error);
                        return Ok(true);
                    }
                };
                match rooms.join(source, join, role_for(&grant), unibus::envelope::now_ms()) {
                    Ok(outcome) => {
                        if let Some(event) = &outcome.left {
                            self.broadcast(hub, &rooms, event, source);
                        }
                        if let Some(event) = &outcome.joined {
                            self.broadcast(hub, &rooms, event, source);
                        }
                        let body = serde_json::to_value(&outcome.welcome)
                            .context("serialize room welcome")?;
                        let _ = self.send(hub, source, WELCOME_KIND, body);
                    }
                    Err(error) => self.send_error(hub, source, error),
                }
            }
            LEAVE_KIND => {
                if let Some(event) = rooms.leave(source) {
                    self.broadcast(hub, &rooms, &event, source);
                }
            }
            SELF_KIND => {
                let state = match parse::<SelfState>(envelope) {
                    Ok(state) => state,
                    Err(error) => {
                        self.send_error(hub, source, error);
                        return Ok(true);
                    }
                };
                match rooms.update(source, state) {
                    Ok(Some(event)) => self.broadcast(hub, &rooms, &event, source),
                    Ok(None) => {}
                    Err(error) => self.send_error(hub, source, error),
                }
            }
            LIST_KIND => {
                let body = serde_json::to_value(rooms.list()).context("serialize room list")?;
                let _ = self.send(hub, source, LIST_REPLY_KIND, body);
            }
            _ => self.send_error(
                hub,
                source,
                RoomError::invalid(format!("unknown room kind {kind}")),
            ),
        }
        Ok(true)
    }
}

fn parse<T: DeserializeOwned>(envelope: &Envelope) -> Result<T, RoomError> {
    serde_json::from_value(Value::Object(envelope.message.body.clone())).map_err(|error| {
        RoomError::invalid(format!("invalid {} body: {error}", envelope.message.kind))
    })
}
