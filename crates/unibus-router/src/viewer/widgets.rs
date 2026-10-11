//! Optional presentation plugin: source binding is explicit, never discovered.
use super::VIEWER_CAPABILITY;
use crate::{HubHandle, HubPlugin};
use anyhow::{Context, Result, ensure};
use coherence_widget_contract::{MAX_BYTES, MESSAGE_KIND, RetainedFeed, Snapshot, identifier};
use serde::Deserialize;
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Mutex,
};
use unibus::{Envelope, grants::NodeGrant};

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Definition {
    pub id: String,
    pub title: String,
    pub producer_id: String,
    pub zone: String,
    pub capability: String,
}
impl Definition {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            identifier(&self.id)
                && self.id.as_bytes()[0].is_ascii_alphanumeric()
                && !self.title.is_empty()
                && self.title.len() <= 128
                && !self.title.chars().any(char::is_control),
            "invalid widget identity or title"
        );
        ensure!(
            [&self.producer_id, &self.zone, &self.capability]
                .iter()
                .all(|v| !v.is_empty() && v.len() <= 128 && !v.chars().any(char::is_control)),
            "invalid widget source or scope"
        );
        Ok(())
    }
    pub fn authorized(&self, grant: &NodeGrant) -> bool {
        grant.capabilities.iter().any(|c| c == VIEWER_CAPABILITY)
            && grant.capabilities.contains(&self.capability)
            && grant.zones.contains(&self.zone)
    }
}

#[derive(Default)]
pub struct Store {
    feeds: Mutex<BTreeMap<(String, String, String), RetainedFeed>>,
    allowed: Mutex<BTreeSet<(String, String, String)>>,
}
impl Store {
    pub fn configure(&self, definitions: &[Definition]) -> Result<()> {
        ensure!(definitions.len() <= 32, "too many widget definitions");
        let mut keys = BTreeSet::new();
        for definition in definitions {
            definition.validate()?;
            keys.insert((
                definition.producer_id.clone(),
                definition.zone.clone(),
                definition.id.clone(),
            ));
        }
        *self
            .allowed
            .lock()
            .map_err(|_| anyhow::anyhow!("widget policy unavailable"))? = keys.clone();
        self.feeds
            .lock()
            .map_err(|_| anyhow::anyhow!("widget store unavailable"))?
            .retain(|key, _| keys.contains(key));
        Ok(())
    }
    fn retain(&self, source: &str, zone: &str, snapshot: Snapshot, now: u64) -> Result<()> {
        let key = (source.into(), zone.into(), snapshot.widget_id.clone());
        ensure!(
            self.allowed
                .lock()
                .map_err(|_| anyhow::anyhow!("widget policy unavailable"))?
                .contains(&key),
            "widget source is not configured"
        );
        let mut feeds = self
            .feeds
            .lock()
            .map_err(|_| anyhow::anyhow!("widget store unavailable"))?;
        ensure!(
            feeds.contains_key(&key) || feeds.len() < 128,
            "widget retention capacity exceeded"
        );
        // Validate before adding a slot, so malformed packets cannot exhaust it.
        snapshot.validate().map_err(anyhow::Error::msg)?;
        ensure!(snapshot.fresh_at(now), "expired or future widget snapshot");
        feeds
            .entry(key)
            .or_default()
            .update(snapshot, now)
            .map_err(anyhow::Error::msg)
    }
    pub fn get(&self, definition: &Definition, grant: &NodeGrant) -> Result<Option<Snapshot>> {
        ensure!(definition.authorized(grant), "widget access denied");
        let feeds = self
            .feeds
            .lock()
            .map_err(|_| anyhow::anyhow!("widget store unavailable"))?;
        Ok(feeds
            .get(&(
                definition.producer_id.clone(),
                definition.zone.clone(),
                definition.id.clone(),
            ))
            .and_then(RetainedFeed::snapshot)
            .cloned())
    }
}
impl HubPlugin for Store {
    fn handle(
        &self,
        hub: &HubHandle,
        source: &str,
        envelope: &Envelope,
        payload: &[u8],
    ) -> Result<bool> {
        if envelope.message.kind != MESSAGE_KIND {
            return Ok(false);
        }
        // Unconfigured feeds remain opaque traffic under the normal relay ACLs.
        let Some(id) = envelope
            .message
            .body
            .get("widget_id")
            .and_then(|v| v.as_str())
        else {
            return Ok(false);
        };
        let Some(zone) = envelope.destination.as_deref() else {
            return Ok(false);
        };
        let key = (source.to_owned(), zone.to_owned(), id.to_owned());
        if !self
            .allowed
            .lock()
            .map_err(|_| anyhow::anyhow!("widget policy unavailable"))?
            .contains(&key)
        {
            return Ok(false);
        }
        let grant = hub.grant_of(source).context("widget source has no grant")?;
        ensure!(
            grant.capabilities.iter().any(|c| c == "widget.source")
                && grant.publish_zones.iter().any(|z| z == zone)
                && grant.may_publish(MESSAGE_KIND),
            "widget source scope denied"
        );
        ensure!(
            payload.is_empty(),
            "widget snapshots cannot contain binary payload"
        );
        let bytes = serde_json::to_vec(&envelope.message.body)?;
        ensure!(
            bytes.len() <= MAX_BYTES,
            "widget snapshot exceeds byte budget"
        );
        self.retain(
            source,
            zone,
            serde_json::from_slice(&bytes)?,
            unibus::envelope::now_ms(),
        )?;
        // Retention supplements normal relay delivery, never steals fan-out.
        Ok(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn configured_producer_and_zone_are_isolated() {
        let store = Store::default();
        let mut definition = Definition {
            id: "umie".into(),
            title: "UMIE".into(),
            producer_id: "source".into(),
            zone: "lab".into(),
            capability: "umie.telemetry".into(),
        };
        store.configure(&[definition.clone()]).unwrap();
        let snapshot = Snapshot {
            schema_version: 1,
            widget_id: "umie".into(),
            observed_at_unix_ms: 1000,
            expires_at_unix_ms: 31000,
            metrics: vec![],
        };
        store.retain("source", "lab", snapshot, 1000).unwrap();
        let mut grant: NodeGrant =
            serde_json::from_value(serde_json::json!({"producer_id":"reader"})).unwrap();
        grant.capabilities = vec![VIEWER_CAPABILITY.into(), "umie.telemetry".into()];
        grant.zones = vec!["lab".into()];
        assert!(store.get(&definition, &grant).unwrap().is_some());
        definition.producer_id = "other".into();
        assert!(store.get(&definition, &grant).unwrap().is_none());
        definition.zone = "home".into();
        assert!(store.get(&definition, &grant).is_err());
        grant.capabilities.clear();
        assert!(store.get(&definition, &grant).is_err());
    }
    #[test]
    fn unconfigured_sources_cannot_exhaust_retention() {
        let store = Store::default();
        let snapshot = Snapshot {
            schema_version: 1,
            widget_id: "unknown".into(),
            observed_at_unix_ms: 1000,
            expires_at_unix_ms: 31000,
            metrics: vec![],
        };
        assert!(store.retain("source", "lab", snapshot, 1000).is_err());
        assert!(store.feeds.lock().unwrap().is_empty());
    }
}
