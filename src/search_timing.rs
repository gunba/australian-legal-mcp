use crate::SearchMode;
use anyhow::Result;
use serde_json::{json, Value as JsonValue};
use std::time::{Duration, Instant};

/// Monotonic request timestamps carried from HTTP admission into search.
///
/// The queue interval is fixed when the bounded HTTP worker starts. Search
/// timing therefore reports the real admission-to-worker-start interval rather
/// than deriving a proxy from other phase durations.
#[derive(Clone, Copy, Debug)]
pub(crate) struct SearchRequestTiming {
    request_id: u64,
    admitted_at: Instant,
    worker_started_at: Instant,
}

impl SearchRequestTiming {
    pub(crate) fn new(request_id: u64, admitted_at: Instant, worker_started_at: Instant) -> Self {
        Self {
            request_id,
            admitted_at,
            worker_started_at,
        }
    }

    pub(crate) fn request_id(self) -> u64 {
        self.request_id
    }

    pub(crate) fn queue_duration(self) -> Duration {
        self.worker_started_at
            .saturating_duration_since(self.admitted_at)
    }

    fn total_duration_at(self, completed_at: Instant) -> Duration {
        completed_at.saturating_duration_since(self.admitted_at)
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum SearchPhase {
    LexicalIndex,
    Embedding,
    VectorScan,
    Fusion,
    PayloadHydration,
}

impl SearchPhase {
    const COUNT: usize = 5;

    const fn index(self) -> usize {
        match self {
            Self::LexicalIndex => 0,
            Self::Embedding => 1,
            Self::VectorScan => 2,
            Self::Fusion => 3,
            Self::PayloadHydration => 4,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PhaseStatus {
    NotRun,
    Completed,
    Failed,
}

impl PhaseStatus {
    const fn as_str(self) -> &'static str {
        match self {
            Self::NotRun => "not_run",
            Self::Completed => "completed",
            Self::Failed => "failed",
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct PhaseMeasurement {
    status: PhaseStatus,
    duration: Duration,
}

impl Default for PhaseMeasurement {
    fn default() -> Self {
        Self {
            status: PhaseStatus::NotRun,
            duration: Duration::ZERO,
        }
    }
}

/// Request-local search phase measurements. This type has no global state and
/// its log record is constructed from a closed set of non-sensitive fields.
pub(crate) struct SearchTimings {
    request: Option<SearchRequestTiming>,
    mode: &'static str,
    phases: [PhaseMeasurement; SearchPhase::COUNT],
}

impl SearchTimings {
    pub(crate) fn new(request: Option<SearchRequestTiming>, mode: SearchMode) -> Self {
        Self {
            request,
            mode: mode.as_str(),
            phases: [PhaseMeasurement::default(); SearchPhase::COUNT],
        }
    }

    /// Build a closed-schema failure record for a call that named `search`
    /// but failed argument validation. No rejected argument is inspected or
    /// copied into the event.
    pub(crate) fn rejected_validation(request: Option<SearchRequestTiming>) -> Option<JsonValue> {
        Self {
            request,
            mode: "unvalidated",
            phases: [PhaseMeasurement::default(); SearchPhase::COUNT],
        }
        .finish(false)
    }

    pub(crate) fn measure<T>(
        &mut self,
        phase: SearchPhase,
        operation: impl FnOnce() -> Result<T>,
    ) -> Result<T> {
        let started = Instant::now();
        let result = operation();
        self.record(phase, started.elapsed(), result.is_ok());
        result
    }

    pub(crate) fn measure_value<T>(
        &mut self,
        phase: SearchPhase,
        operation: impl FnOnce() -> T,
    ) -> T {
        let started = Instant::now();
        let value = operation();
        self.record(phase, started.elapsed(), true);
        value
    }

    fn record(&mut self, phase: SearchPhase, duration: Duration, succeeded: bool) {
        let measurement = &mut self.phases[phase.index()];
        measurement.duration = measurement.duration.saturating_add(duration);
        measurement.status = if succeeded {
            if measurement.status == PhaseStatus::Failed {
                PhaseStatus::Failed
            } else {
                PhaseStatus::Completed
            }
        } else {
            PhaseStatus::Failed
        };
    }

    pub(crate) fn finish(self, succeeded: bool) -> Option<JsonValue> {
        self.finish_at(succeeded, Instant::now())
    }

    fn finish_at(self, succeeded: bool, completed_at: Instant) -> Option<JsonValue> {
        let request = self.request?;
        let lexical = self.phases[SearchPhase::LexicalIndex.index()];
        let embedding = self.phases[SearchPhase::Embedding.index()];
        let vector_scan = self.phases[SearchPhase::VectorScan.index()];
        let fusion = self.phases[SearchPhase::Fusion.index()];
        let payload_hydration = self.phases[SearchPhase::PayloadHydration.index()];
        Some(json!({
            "event": "search-timing",
            "request_id": request.request_id(),
            "mode": self.mode,
            "status": if succeeded { "ok" } else { "error" },
            "phase_status": {
                "queue": "completed",
                "lexical_index": lexical.status.as_str(),
                "embedding": embedding.status.as_str(),
                "vector_scan": vector_scan.status.as_str(),
                "fusion": fusion.status.as_str(),
                "payload_hydration": payload_hydration.status.as_str(),
            },
            "duration_us": {
                "queue": duration_micros(request.queue_duration()),
                "lexical_index": duration_micros(lexical.duration),
                "embedding": duration_micros(embedding.duration),
                "vector_scan": duration_micros(vector_scan.duration),
                "fusion": duration_micros(fusion.duration),
                "payload_hydration": duration_micros(payload_hydration.duration),
                "total": duration_micros(request.total_duration_at(completed_at)),
            },
        }))
    }
}

fn duration_micros(duration: Duration) -> u64 {
    u64::try_from(duration.as_micros()).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::bail;
    use std::collections::BTreeSet;

    fn object_keys(value: &JsonValue) -> BTreeSet<&str> {
        value
            .as_object()
            .expect("timing field must be an object")
            .keys()
            .map(String::as_str)
            .collect()
    }

    #[test]
    fn queue_uses_admission_and_worker_instants_exactly() {
        let admitted_at = Instant::now();
        let worker_started_at = admitted_at + Duration::from_micros(37);
        let completed_at = admitted_at + Duration::from_micros(101);
        let request = SearchRequestTiming::new(73, admitted_at, worker_started_at);
        let record = SearchTimings::new(Some(request), SearchMode::Keyword)
            .finish_at(true, completed_at)
            .expect("request timing record");

        assert_eq!(record["request_id"], 73);
        assert_eq!(record["duration_us"]["queue"], 37);
        assert_eq!(record["duration_us"]["total"], 101);
    }

    #[test]
    fn log_schema_is_correlated_allowlisted_and_omits_error_details() {
        let admitted_at = Instant::now();
        let request = SearchRequestTiming::new(904, admitted_at, admitted_at);
        let mut timings = SearchTimings::new(Some(request), SearchMode::Hybrid);
        let error = timings
            .measure(SearchPhase::Embedding, || -> Result<()> {
                bail!(
                    "PRIVATE_QUERY_CREDENTIAL_FILTER_NATIVE_ID_SCORE_MODEL_CANDIDATE_COUNT_DOCUMENT"
                )
            })
            .expect_err("injected embedding failure");
        assert!(error.to_string().contains("PRIVATE_QUERY"));
        let record = timings
            .finish_at(false, admitted_at + Duration::from_micros(9))
            .expect("failed request timing record");

        assert_eq!(
            object_keys(&record),
            BTreeSet::from([
                "duration_us",
                "event",
                "mode",
                "phase_status",
                "request_id",
                "status",
            ])
        );
        assert_eq!(
            object_keys(&record["phase_status"]),
            BTreeSet::from([
                "embedding",
                "fusion",
                "lexical_index",
                "payload_hydration",
                "queue",
                "vector_scan",
            ])
        );
        assert_eq!(
            object_keys(&record["duration_us"]),
            BTreeSet::from([
                "embedding",
                "fusion",
                "lexical_index",
                "payload_hydration",
                "queue",
                "total",
                "vector_scan",
            ])
        );
        assert_eq!(record["mode"], "hybrid");
        assert_eq!(record["status"], "error");
        assert_eq!(record["phase_status"]["embedding"], "failed");
        assert_eq!(record["phase_status"]["vector_scan"], "not_run");
        assert_eq!(record["duration_us"]["vector_scan"], 0);
        assert!(!record.to_string().contains("PRIVATE_QUERY"));
    }

    #[test]
    fn keyword_not_run_phases_are_explicit_and_zero() {
        let admitted_at = Instant::now();
        let request = SearchRequestTiming::new(1, admitted_at, admitted_at);
        let mut timings = SearchTimings::new(Some(request), SearchMode::Keyword);
        timings
            .measure(SearchPhase::LexicalIndex, || Ok(()))
            .expect("lexical phase");
        timings
            .measure(SearchPhase::PayloadHydration, || Ok(()))
            .expect("hydration phase");
        let record = timings
            .finish_at(true, admitted_at + Duration::from_micros(20))
            .expect("keyword timing record");

        for phase in ["embedding", "vector_scan", "fusion"] {
            assert_eq!(record["phase_status"][phase], "not_run");
            assert_eq!(record["duration_us"][phase], 0);
        }
        assert_eq!(record["status"], "ok");
    }

    #[test]
    fn rejected_validation_record_is_allowlisted_and_argument_free() {
        let admitted_at = Instant::now();
        let request = SearchRequestTiming::new(18, admitted_at, admitted_at);
        let record = SearchTimings::rejected_validation(Some(request))
            .expect("rejected search timing record");
        assert_eq!(record["request_id"], 18);
        assert_eq!(record["mode"], "unvalidated");
        assert_eq!(record["status"], "error");
        for phase in [
            "lexical_index",
            "embedding",
            "vector_scan",
            "fusion",
            "payload_hydration",
        ] {
            assert_eq!(record["phase_status"][phase], "not_run");
            assert_eq!(record["duration_us"][phase], 0);
        }
    }
}
