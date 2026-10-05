//! Hub plugin for renewable, stream-id-based fan-out.

use std::collections::BTreeMap;
use std::sync::Mutex;

use anyhow::{Context, Result, ensure};
use unibus::{
    Envelope, STREAM_EVENT_KIND, STREAM_INTEREST_KIND, STREAM_INTEREST_RELEASE_KIND, StreamEvent,
    StreamInterest, StreamInterestRelease, envelope,
};

use crate::plugin::{HubHandle, HubPlugin};

#[derive(Debug, Default)]
pub struct StreamRouter {
    interests: Mutex<BTreeMap<String, StreamInterest>>,
}

impl HubPlugin for StreamRouter {
    fn handle(
        &self,
        hub: &HubHandle,
        source: &str,
        envelope: &Envelope,
        payload: &[u8],
    ) -> Result<bool> {
        match envelope.message.kind.as_str() {
            STREAM_INTEREST_KIND => {
                ensure!(
                    payload.is_empty(),
                    "stream interest cannot carry binary payload"
                );
                let interest: StreamInterest = serde_json::from_value(serde_json::Value::Object(
                    envelope.message.body.clone(),
                ))
                .context("decode stream interest")?;
                interest.validate()?;
                ensure!(
                    interest.consumer_id == source,
                    "stream interest consumer must match authenticated producer"
                );
                self.interests
                    .lock()
                    .map_err(|_| anyhow::anyhow!("stream interest mutex poisoned"))?
                    .insert(interest.lease_id.clone(), interest);
                Ok(true)
            }
            STREAM_INTEREST_RELEASE_KIND => {
                ensure!(
                    payload.is_empty(),
                    "stream interest release cannot carry binary payload"
                );
                let release: StreamInterestRelease = serde_json::from_value(
                    serde_json::Value::Object(envelope.message.body.clone()),
                )
                .context("decode stream interest release")?;
                release.validate()?;
                ensure!(
                    release.consumer_id == source,
                    "stream interest release consumer must match authenticated producer"
                );
                let mut interests = self
                    .interests
                    .lock()
                    .map_err(|_| anyhow::anyhow!("stream interest mutex poisoned"))?;
                if interests
                    .get(&release.lease_id)
                    .is_some_and(|interest| interest.consumer_id == source)
                {
                    interests.remove(&release.lease_id);
                }
                Ok(true)
            }
            STREAM_EVENT_KIND => {
                let event: StreamEvent = serde_json::from_value(serde_json::Value::Object(
                    envelope.message.body.clone(),
                ))
                .context("decode stream event")?;
                event.validate()?;
                let now = envelope::now_ms();
                let targets = {
                    let mut interests = self
                        .interests
                        .lock()
                        .map_err(|_| anyhow::anyhow!("stream interest mutex poisoned"))?;
                    interests.retain(|_, interest| interest.active_at(now));
                    interests
                        .values()
                        .filter(|interest| {
                            interest.consumer_id != source
                                && interest.stream_ids.binary_search(&event.stream_id).is_ok()
                        })
                        .map(|interest| interest.consumer_id.clone())
                        .collect::<std::collections::BTreeSet<_>>()
                };
                for target in targets {
                    // A stale lease must not turn one disconnected consumer into
                    // a publication failure for every other consumer.
                    let _ = hub.send_to_producer(&target, envelope, payload);
                }
                Ok(true)
            }
            _ => Ok(false),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_router_starts_without_interests() {
        assert!(StreamRouter::default().interests.lock().unwrap().is_empty());
    }

    #[test]
    fn a_consumer_can_release_only_its_own_lease() {
        let router = StreamRouter::default();
        router.interests.lock().unwrap().insert(
            "canvas:mix".into(),
            StreamInterest {
                lease_id: "canvas:mix".into(),
                consumer_id: "canvas-neo".into(),
                stream_ids: vec!["synesthesia/mix".into()],
                issued_at_unix_ms: 1,
                expires_at_unix_ms: 2,
            },
        );
        let release = StreamInterestRelease {
            lease_id: "canvas:mix".into(),
            consumer_id: "canvas-neo".into(),
        };
        release.validate().unwrap();
        let mut interests = router.interests.lock().unwrap();
        if interests
            .get(&release.lease_id)
            .is_some_and(|interest| interest.consumer_id == release.consumer_id)
        {
            interests.remove(&release.lease_id);
        }
        assert!(interests.is_empty());
    }
}
