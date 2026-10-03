//! Deterministic reconnect pacing shared by long-running Unibus edges.

use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReconnectPolicy {
    pub initial_delay: Duration,
    pub maximum_delay: Duration,
    /// A session surviving this long resets the failure history.
    pub stable_after: Duration,
}

impl ReconnectPolicy {
    pub fn validate(self) -> Result<Self, &'static str> {
        if self.initial_delay.is_zero() {
            return Err("initial reconnect delay must be positive");
        }
        if self.maximum_delay < self.initial_delay {
            return Err("maximum reconnect delay must not be less than initial delay");
        }
        if self.stable_after.is_zero() {
            return Err("stable reconnect duration must be positive");
        }
        Ok(self)
    }
}

impl Default for ReconnectPolicy {
    fn default() -> Self {
        Self {
            initial_delay: Duration::from_millis(250),
            maximum_delay: Duration::from_secs(30),
            stable_after: Duration::from_secs(30),
        }
    }
}

#[derive(Debug, Clone)]
pub struct ReconnectBackoff {
    policy: ReconnectPolicy,
    consecutive_failures: u32,
}

impl ReconnectBackoff {
    pub fn new(policy: ReconnectPolicy) -> Result<Self, &'static str> {
        Ok(Self {
            policy: policy.validate()?,
            consecutive_failures: 0,
        })
    }

    /// Record a failed session and return the delay before the next attempt.
    /// A session that lived through `stable_after` starts again at the minimum.
    pub fn failed_after(&mut self, session_uptime: Duration) -> Duration {
        if session_uptime >= self.policy.stable_after {
            self.consecutive_failures = 0;
        }
        let exponent = self.consecutive_failures.min(31);
        self.consecutive_failures = self.consecutive_failures.saturating_add(1);
        self.policy
            .initial_delay
            .saturating_mul(1_u32 << exponent)
            .min(self.policy.maximum_delay)
    }

    pub fn consecutive_failures(&self) -> u32 {
        self.consecutive_failures
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_is_bounded_and_resets_after_a_stable_session() {
        let policy = ReconnectPolicy {
            initial_delay: Duration::from_millis(100),
            maximum_delay: Duration::from_millis(500),
            stable_after: Duration::from_secs(10),
        };
        let mut backoff = ReconnectBackoff::new(policy).unwrap();
        assert_eq!(
            backoff.failed_after(Duration::ZERO),
            Duration::from_millis(100)
        );
        assert_eq!(
            backoff.failed_after(Duration::ZERO),
            Duration::from_millis(200)
        );
        assert_eq!(
            backoff.failed_after(Duration::ZERO),
            Duration::from_millis(400)
        );
        assert_eq!(
            backoff.failed_after(Duration::ZERO),
            Duration::from_millis(500)
        );
        assert_eq!(
            backoff.failed_after(Duration::ZERO),
            Duration::from_millis(500)
        );
        assert_eq!(
            backoff.failed_after(Duration::from_secs(10)),
            Duration::from_millis(100)
        );
        assert_eq!(backoff.consecutive_failures(), 1);
    }

    #[test]
    fn invalid_policy_fails_before_a_retry_loop_starts() {
        assert!(
            ReconnectBackoff::new(ReconnectPolicy {
                initial_delay: Duration::ZERO,
                ..ReconnectPolicy::default()
            })
            .is_err()
        );
    }
}
