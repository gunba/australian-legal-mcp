use anyhow::{anyhow, Result};
use chrono::Utc;
use serde_json::json;
use std::sync::{Condvar, Mutex};
use std::time::{Duration, Instant};

/// One evidence-backed ceiling for every source. The active request count starts
/// at one and adapts below this ceiling; the value matches the Federal Court
/// scraper limit used by the reference OALCC implementation.
pub(crate) const SOURCE_WORKER_CEILING: usize = 10;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RequestOutcome {
    Success,
    Neutral,
    Transient,
    Congestion,
}

impl RequestOutcome {
    fn label(self) -> &'static str {
        match self {
            Self::Success => "success",
            Self::Neutral => "neutral",
            Self::Transient => "transient",
            Self::Congestion => "congestion",
        }
    }
}

#[derive(Debug)]
struct AdaptiveState {
    active: usize,
    limit: usize,
    learned_ceiling: usize,
    successes_since_increase: usize,
    congestion_until: Option<Instant>,
}

#[derive(Debug)]
pub(crate) struct AdaptiveConcurrency {
    source: &'static str,
    state: Mutex<AdaptiveState>,
    changed: Condvar,
}

impl AdaptiveConcurrency {
    pub(crate) fn new(source: &'static str) -> Self {
        Self {
            source,
            state: Mutex::new(AdaptiveState {
                active: 0,
                limit: 1,
                learned_ceiling: SOURCE_WORKER_CEILING,
                successes_since_increase: 0,
                congestion_until: None,
            }),
            changed: Condvar::new(),
        }
    }

    pub(crate) fn acquire(&self) -> Result<AdaptiveRequest<'_>> {
        let queued_at = Instant::now();
        let mut state = self
            .state
            .lock()
            .map_err(|_| anyhow!("adaptive HTTP concurrency lock is poisoned"))?;
        while state.active >= state.limit {
            state = self
                .changed
                .wait(state)
                .map_err(|_| anyhow!("adaptive HTTP concurrency lock is poisoned"))?;
        }
        state.active += 1;
        let limit = state.limit;
        let active = state.active;
        drop(state);
        Ok(AdaptiveRequest {
            controller: self,
            started_at: Instant::now(),
            queue_wait: queued_at.elapsed(),
            admitted_limit: limit,
            admitted_active: active,
            finished: false,
        })
    }

    fn finish(&self, event: Completion<'_>) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.active = state.active.saturating_sub(1);
        let limit_before = state.limit;
        let ceiling_before = state.learned_ceiling;
        match event.outcome {
            RequestOutcome::Success => {
                state.successes_since_increase += 1;
                if state.limit < state.learned_ceiling
                    && state.successes_since_increase >= state.limit
                {
                    state.limit += 1;
                    state.successes_since_increase = 0;
                }
            }
            RequestOutcome::Neutral => {}
            RequestOutcome::Transient => {
                state.limit = (state.limit / 2).max(1);
                state.successes_since_increase = 0;
            }
            RequestOutcome::Congestion => {
                let now = Instant::now();
                let begins_episode = state
                    .congestion_until
                    .is_none_or(|congestion_until| now >= congestion_until);
                if begins_episode {
                    state.learned_ceiling = state.learned_ceiling.min((limit_before / 2).max(1));
                }
                if let Some(delay) = event.retry_delay {
                    let candidate = now + delay;
                    if state
                        .congestion_until
                        .is_none_or(|congestion_until| candidate > congestion_until)
                    {
                        state.congestion_until = Some(candidate);
                    }
                }
                state.limit = state.limit.min(state.learned_ceiling);
                state.successes_since_increase = 0;
            }
        }
        let limit_after = state.limit;
        let ceiling_after = state.learned_ceiling;
        let active_after = state.active;
        self.changed.notify_all();
        drop(state);

        eprintln!(
            "legal-mcp http-audit {}",
            json!({
                "at": Utc::now().to_rfc3339(),
                "source": self.source,
                "url": event.url,
                "status": event.status,
                "bytes": event.bytes,
                "attempt": event.attempt,
                "outcome": event.outcome.label(),
                "queue_wait_ms": event.queue_wait.as_millis(),
                "pacing_wait_ms": event.pacing_wait.as_millis(),
                "request_ms": event.request_elapsed.as_millis(),
                "retry_delay_ms": event.retry_delay.map(|delay| delay.as_millis()),
                "admitted_limit": event.admitted_limit,
                "admitted_active": event.admitted_active,
                "limit_before": limit_before,
                "limit_after": limit_after,
                "ceiling_before": ceiling_before,
                "ceiling_after": ceiling_after,
                "active_after": active_after,
            })
        );
    }

    fn release_unfinished(&self) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.active = state.active.saturating_sub(1);
        self.changed.notify_all();
    }

    #[cfg(test)]
    fn limit(&self) -> usize {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .limit
    }
}

#[derive(Debug)]
pub(crate) struct AdaptiveRequest<'a> {
    controller: &'a AdaptiveConcurrency,
    started_at: Instant,
    queue_wait: Duration,
    admitted_limit: usize,
    admitted_active: usize,
    finished: bool,
}

impl AdaptiveRequest<'_> {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn finish(
        mut self,
        url: &str,
        status: Option<u16>,
        bytes: usize,
        attempt: usize,
        outcome: RequestOutcome,
        pacing_wait: Duration,
        retry_delay: Option<Duration>,
    ) {
        self.finished = true;
        self.controller.finish(Completion {
            url,
            status,
            bytes,
            attempt,
            outcome,
            queue_wait: self.queue_wait,
            pacing_wait,
            request_elapsed: self.started_at.elapsed(),
            retry_delay,
            admitted_limit: self.admitted_limit,
            admitted_active: self.admitted_active,
        });
    }
}

impl Drop for AdaptiveRequest<'_> {
    fn drop(&mut self) {
        if self.finished {
            return;
        }
        self.controller.release_unfinished();
    }
}

struct Completion<'a> {
    url: &'a str,
    status: Option<u16>,
    bytes: usize,
    attempt: usize,
    outcome: RequestOutcome,
    queue_wait: Duration,
    pacing_wait: Duration,
    request_elapsed: Duration,
    retry_delay: Option<Duration>,
    admitted_limit: usize,
    admitted_active: usize,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn finish_success(controller: &AdaptiveConcurrency) -> Result<()> {
        controller.acquire()?.finish(
            "https://example.test/document",
            Some(200),
            10,
            1,
            RequestOutcome::Success,
            Duration::ZERO,
            None,
        );
        Ok(())
    }

    #[test]
    fn clean_responses_increase_concurrency_to_the_shared_ceiling() -> Result<()> {
        let controller = AdaptiveConcurrency::new("test");
        while controller.limit() < SOURCE_WORKER_CEILING {
            let limit = controller.limit();
            for _ in 0..limit {
                finish_success(&controller)?;
            }
        }
        assert_eq!(controller.limit(), SOURCE_WORKER_CEILING);
        Ok(())
    }

    #[test]
    fn congestion_halves_the_current_limit() -> Result<()> {
        let controller = AdaptiveConcurrency::new("test");
        for _ in 0..6 {
            let limit = controller.limit();
            for _ in 0..limit {
                finish_success(&controller)?;
            }
        }
        let before = controller.limit();
        controller.acquire()?.finish(
            "https://example.test/document",
            Some(429),
            0,
            1,
            RequestOutcome::Congestion,
            Duration::ZERO,
            Some(Duration::from_secs(1)),
        );
        assert_eq!(controller.limit(), (before / 2).max(1));
        Ok(())
    }

    #[test]
    fn one_congestion_episode_cannot_cascade_the_limit_to_one() -> Result<()> {
        let controller = AdaptiveConcurrency::new("test");
        while controller.limit() < SOURCE_WORKER_CEILING {
            let limit = controller.limit();
            for _ in 0..limit {
                finish_success(&controller)?;
            }
        }
        controller.acquire()?.finish(
            "https://example.test/document",
            Some(403),
            0,
            1,
            RequestOutcome::Congestion,
            Duration::ZERO,
            Some(Duration::from_secs(1)),
        );
        let reduced = controller.limit();
        controller.acquire()?.finish(
            "https://example.test/document",
            Some(403),
            0,
            1,
            RequestOutcome::Congestion,
            Duration::ZERO,
            Some(Duration::from_secs(1)),
        );
        assert_eq!(controller.limit(), reduced);
        Ok(())
    }

    #[test]
    fn transient_failure_reduces_only_the_active_limit() -> Result<()> {
        let controller = AdaptiveConcurrency::new("test");
        while controller.limit() < SOURCE_WORKER_CEILING {
            let limit = controller.limit();
            for _ in 0..limit {
                finish_success(&controller)?;
            }
        }
        controller.acquire()?.finish(
            "https://example.test/document",
            Some(502),
            0,
            1,
            RequestOutcome::Transient,
            Duration::ZERO,
            Some(Duration::from_millis(500)),
        );
        assert_eq!(controller.limit(), SOURCE_WORKER_CEILING / 2);
        for _ in 0..controller.limit() {
            finish_success(&controller)?;
        }
        assert_eq!(controller.limit(), SOURCE_WORKER_CEILING / 2 + 1);
        Ok(())
    }

    #[test]
    fn unfinished_request_releases_capacity_without_learning_congestion() -> Result<()> {
        let controller = AdaptiveConcurrency::new("test");
        drop(controller.acquire()?);
        let state = controller
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert_eq!(state.active, 0);
        assert_eq!(state.limit, 1);
        assert_eq!(state.learned_ceiling, SOURCE_WORKER_CEILING);
        Ok(())
    }
}
