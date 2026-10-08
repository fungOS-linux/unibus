//! Isolated provider sessions. Slow or unavailable MCP servers cannot hold up
//! other providers' leases or the publisher's authorization/event loop.
//!
//! Reconnects are bounded exponential backoff (1–30 seconds). Invocations are
//! never replayed: a failed call may already have changed the remote system.
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use serde_json::Value;
use unibus_mcp::http::{McpClient, ProviderConfig};
use unibus_mcp::{Invocation, ProviderAdvertisement, ToolDescriptor};

pub enum Event {
    Catalog {
        index: usize,
        tools: Vec<ToolDescriptor>,
        observed_at: u64,
    },
    Failed {
        index: usize,
        error: String,
        retry: Duration,
    },
    Completed {
        invocation: Invocation,
        producer: String,
        result: Result<Value>,
    },
}

pub struct Call {
    pub invocation: Invocation,
    pub producer: String,
}

pub type Authorizer = Arc<dyn Fn(&Call, &ToolDescriptor) -> Result<()> + Send + Sync>;

pub struct Worker {
    pub config: ProviderConfig,
    pub ready: bool,
    calls: SyncSender<Call>,
}

impl Worker {
    pub fn start(
        index: usize,
        config: ProviderConfig,
        refresh: Duration,
        events: SyncSender<Event>,
        authorize: Authorizer,
    ) -> Result<Self> {
        let (calls, receiver) = mpsc::sync_channel(16);
        let provider = config.clone();
        thread::Builder::new()
            .name(format!("mcp-provider-{index}"))
            .spawn(move || {
                run(index, refresh, receiver, events, || {
                    Ok(Box::new(Session {
                        client: McpClient::connect(&provider)?,
                        config: provider.clone(),
                        authorize: authorize.clone(),
                        tools: Vec::new(),
                        observed_at: 0,
                        lease_ms: refresh.as_millis() as u64 * 2,
                    }) as Box<dyn Connection>)
                });
            })
            .context("start MCP provider worker")?;
        Ok(Self {
            config,
            ready: false,
            calls,
        })
    }

    pub fn submit(&self, call: Call) -> Result<()> {
        anyhow::ensure!(self.ready, "provider is unavailable");
        self.calls
            .try_send(call)
            .context("provider invocation queue unavailable")
    }
}

trait Connection {
    fn catalog(&mut self) -> Result<Vec<ToolDescriptor>>;
    fn call(&mut self, call: &Call) -> Result<Value>;
}

struct Session {
    client: McpClient,
    config: ProviderConfig,
    authorize: Authorizer,
    tools: Vec<ToolDescriptor>,
    observed_at: u64,
    lease_ms: u64,
}
impl Connection for Session {
    fn catalog(&mut self) -> Result<Vec<ToolDescriptor>> {
        let tools = self.client.list_tools(&self.config.mutation_overrides)?;
        let now = unibus::envelope::now_ms();
        // Invalid/empty/oversized catalogs are provider failures too, not a
        // reason to tear down every other provider's publication.
        let validated = ProviderAdvertisement::new(
            &self.config.provider_id,
            &self.config.node_id,
            &self.config.producer_id,
            "catalog-check",
            now,
            now + 1,
            tools,
        )?;
        self.tools = validated.tools;
        self.observed_at = now;
        Ok(self.tools.clone())
    }
    fn call(&mut self, call: &Call) -> Result<Value> {
        authorize_execution(
            &self.tools,
            self.observed_at,
            self.lease_ms,
            unibus::envelope::now_ms(),
            &self.authorize,
            call,
        )?;
        let invocation = &call.invocation;
        self.client
            .call_tool(&invocation.tool_name, invocation.arguments.clone())
    }
}

fn authorize_execution(
    tools: &[ToolDescriptor],
    observed_at: u64,
    lease_ms: u64,
    now: u64,
    authorize: &Authorizer,
    call: &Call,
) -> Result<()> {
    anyhow::ensure!(
        observed_at.saturating_add(lease_ms) > now,
        "provider catalog expired while invocation was queued"
    );
    let tool = tools
        .iter()
        .find(|tool| tool.name == call.invocation.tool_name)
        .context("tool no longer advertised")?;
    // Recheck live policy/ACL at execution, not only queue admission.
    authorize(call, tool)
}

fn backoff(failures: u32) -> Duration {
    Duration::from_secs((1_u64 << failures.saturating_sub(1).min(5)).min(30))
}

fn run(
    index: usize,
    refresh: Duration,
    calls: Receiver<Call>,
    events: SyncSender<Event>,
    mut connect: impl FnMut() -> Result<Box<dyn Connection>>,
) {
    let mut connection: Option<Box<dyn Connection>> = None;
    let mut failures = 0_u32;
    let mut next_probe = Instant::now();
    loop {
        if Instant::now() >= next_probe {
            let catalog = (|| {
                if connection.is_none() {
                    connection = Some(connect()?);
                }
                connection.as_mut().expect("connected").catalog()
            })();
            match catalog {
                Ok(tools) => {
                    failures = 0;
                    if events
                        .send(Event::Catalog {
                            index,
                            tools,
                            observed_at: unibus::envelope::now_ms(),
                        })
                        .is_err()
                    {
                        return;
                    }
                    next_probe = Instant::now() + refresh;
                }
                Err(error) => {
                    connection = None;
                    failures = failures.saturating_add(1);
                    let retry = backoff(failures);
                    if events
                        .send(Event::Failed {
                            index,
                            error: format!("{error:#}"),
                            retry,
                        })
                        .is_err()
                    {
                        return;
                    }
                    next_probe = Instant::now() + retry;
                }
            }
        }
        match calls.recv_timeout(next_probe.saturating_duration_since(Instant::now())) {
            Ok(call) => {
                let result = connection
                    .as_mut()
                    .context("provider is unavailable")
                    .and_then(|session| session.call(&call));
                if result.is_err() {
                    connection = None;
                    failures = failures.saturating_add(1);
                    let retry = backoff(failures);
                    if events
                        .send(Event::Failed {
                            index,
                            error: "invocation failed; session will reconnect (call not replayed)"
                                .into(),
                            retry,
                        })
                        .is_err()
                    {
                        return;
                    }
                    next_probe = Instant::now() + retry;
                }
                if events
                    .send(Event::Completed {
                        invocation: call.invocation,
                        producer: call.producer,
                        result,
                    })
                    .is_err()
                {
                    return;
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => return,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    struct Fake;
    impl Connection for Fake {
        fn catalog(&mut self) -> Result<Vec<ToolDescriptor>> {
            Ok(vec![])
        }
        fn call(&mut self, _: &Call) -> Result<Value> {
            anyhow::bail!("failed")
        }
    }

    #[test]
    fn backoff_is_bounded() {
        assert_eq!(backoff(1), Duration::from_secs(1));
        assert_eq!(backoff(2), Duration::from_secs(2));
        assert_eq!(backoff(u32::MAX), Duration::from_secs(30));
    }

    #[test]
    fn expired_catalog_and_revoked_authorization_deny_queued_calls() {
        let call = Call {
            invocation: Invocation {
                schema_version: 1,
                invocation_id: "id".into(),
                provider_id: "provider".into(),
                tool_name: "read".into(),
                arguments: serde_json::json!({}),
                authorization: unibus_mcp::AuthorizationGrant {
                    encoded: "grant".into(),
                },
            },
            producer: "caller".into(),
        };
        let tools = vec![ToolDescriptor {
            name: "read".into(),
            description: String::new(),
            input_schema: serde_json::json!({}),
            output_schema: None,
            mutation: unibus_mcp::MutationClass::ReadOnly,
        }];
        let allow: Authorizer = Arc::new(|_, _| Ok(()));
        let revoked: Authorizer = Arc::new(|_, _| anyhow::bail!("revoked"));
        assert!(authorize_execution(&tools, 100, 100, 199, &allow, &call).is_ok());
        assert!(authorize_execution(&tools, 100, 100, 200, &allow, &call).is_err());
        assert!(authorize_execution(&tools, 100, 100, 199, &revoked, &call).is_err());
        assert!(authorize_execution(&[], 100, 100, 199, &allow, &call).is_err());
    }

    #[test]
    fn slow_initialization_does_not_delay_another_provider() {
        let (events, received) = mpsc::sync_channel(16);
        let (slow_calls, slow_rx) = mpsc::sync_channel(1);
        let (fast_calls, fast_rx) = mpsc::sync_channel(1);
        let slow_events = events.clone();
        let slow = thread::spawn(move || {
            run(0, Duration::from_secs(10), slow_rx, slow_events, || {
                thread::sleep(Duration::from_millis(250));
                Ok(Box::new(Fake))
            })
        });
        let fast = thread::spawn(move || {
            run(1, Duration::from_secs(10), fast_rx, events, || {
                Ok(Box::new(Fake))
            })
        });
        assert!(matches!(
            received.recv_timeout(Duration::from_millis(150)).unwrap(),
            Event::Catalog { index: 1, .. }
        ));
        assert!(matches!(
            received.recv_timeout(Duration::from_secs(1)).unwrap(),
            Event::Catalog { index: 0, .. }
        ));
        drop(slow_calls);
        drop(fast_calls);
        slow.join().unwrap();
        fast.join().unwrap();
    }

    #[test]
    fn failed_provider_does_not_block_healthy_catalog_and_recovers() {
        let (events, received) = mpsc::sync_channel(16);
        let (bad_calls, bad_rx) = mpsc::sync_channel(1);
        let (good_calls, good_rx) = mpsc::sync_channel(1);
        let attempts = Arc::new(AtomicUsize::new(0));
        let count = attempts.clone();
        let bad_events = events.clone();
        let bad = thread::spawn(move || {
            run(0, Duration::from_secs(10), bad_rx, bad_events, || {
                if count.fetch_add(1, Ordering::SeqCst) == 0 {
                    anyhow::bail!("offline");
                }
                Ok(Box::new(Fake))
            })
        });
        let good = thread::spawn(move || {
            run(1, Duration::from_secs(10), good_rx, events, || {
                Ok(Box::new(Fake))
            })
        });
        let mut failed = false;
        let mut healthy = false;
        for _ in 0..2 {
            match received.recv_timeout(Duration::from_millis(500)).unwrap() {
                Event::Failed { index: 0, .. } => failed = true,
                Event::Catalog { index: 1, .. } => healthy = true,
                _ => panic!("unexpected event"),
            }
        }
        assert!(failed && healthy);
        assert!(matches!(
            received.recv_timeout(Duration::from_secs(2)).unwrap(),
            Event::Catalog { index: 0, .. }
        ));
        drop(bad_calls);
        drop(good_calls);
        bad.join().unwrap();
        good.join().unwrap();
    }

    #[test]
    fn failed_invocation_is_not_replayed_and_worker_stops_during_backoff() {
        let (events, received) = mpsc::sync_channel(16);
        let (calls, receiver) = mpsc::sync_channel(1);
        let count = Arc::new(AtomicUsize::new(0));
        struct FailingCall(Arc<AtomicUsize>);
        impl Connection for FailingCall {
            fn catalog(&mut self) -> Result<Vec<ToolDescriptor>> {
                Ok(vec![])
            }
            fn call(&mut self, _: &Call) -> Result<Value> {
                self.0.fetch_add(1, Ordering::SeqCst);
                anyhow::bail!("ambiguous remote failure")
            }
        }
        let attempts = count.clone();
        let worker = thread::spawn(move || {
            run(0, Duration::from_secs(10), receiver, events, || {
                Ok(Box::new(FailingCall(attempts.clone())))
            })
        });
        assert!(matches!(
            received.recv_timeout(Duration::from_secs(1)).unwrap(),
            Event::Catalog { .. }
        ));
        calls
            .send(Call {
                invocation: Invocation {
                    schema_version: 1,
                    invocation_id: "test-call".into(),
                    provider_id: "test-provider".into(),
                    tool_name: "test".into(),
                    arguments: serde_json::json!({}),
                    authorization: unibus_mcp::AuthorizationGrant {
                        encoded: "test".into(),
                    },
                },
                producer: "test-producer".into(),
            })
            .unwrap();
        assert!(matches!(
            received.recv_timeout(Duration::from_secs(1)).unwrap(),
            Event::Failed { .. }
        ));
        assert!(matches!(
            received.recv_timeout(Duration::from_secs(1)).unwrap(),
            Event::Completed { result: Err(_), .. }
        ));
        assert!(matches!(
            received.recv_timeout(Duration::from_secs(2)).unwrap(),
            Event::Catalog { .. }
        ));
        assert_eq!(count.load(Ordering::SeqCst), 1);
        drop(calls);
        worker.join().unwrap();
    }
}
