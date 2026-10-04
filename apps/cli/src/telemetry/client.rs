//! Cloud hub client (`https://summa.duyet.net`). Replaces local `summa serve`.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use async_trait::async_trait;

use crate::model::{DataSink, EventsSnapshotData, SinkResult};
use crate::telemetry::{prepare_events, IngestResponse};

pub const DEFAULT_ENDPOINT: &str = "https://summa.duyet.net";
pub const INGEST_CHUNK: usize = 400;

pub struct TelemetrySink {
    endpoint: String,
    token: String,
    client: Option<reqwest::Client>,
}

impl TelemetrySink {
    pub fn new(endpoint: impl Into<String>, token: impl Into<String>) -> Self {
        Self {
            endpoint: endpoint.into().trim_end_matches('/').to_string(),
            token: token.into(),
            client: None,
        }
    }

    pub fn from_parts(endpoint: Option<&str>, token: Option<&str>) -> Option<Self> {
        let token = token.map(str::trim).filter(|s| !s.is_empty())?;
        let endpoint = endpoint
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or(DEFAULT_ENDPOINT);
        Some(Self::new(endpoint, token))
    }
}

#[async_trait]
impl DataSink for TelemetrySink {
    fn name(&self) -> &'static str {
        "summa-cloud"
    }

    async fn connect(&mut self) -> anyhow::Result<()> {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(60))
            .build()?;
        let url = format!("{}/health", self.endpoint);
        let resp = client.get(&url).send().await?.error_for_status()?;
        let _ = resp.bytes().await?;
        self.client = Some(client);
        Ok(())
    }

    async fn write(&mut self, data: EventsSnapshotData) -> anyhow::Result<SinkResult> {
        let start = Instant::now();
        let events = prepare_events(data.events);
        if events.is_empty() {
            return Ok(SinkResult {
                sink_name: self.name().to_string(),
                duration_ms: start.elapsed().as_millis() as u64,
                ..SinkResult::default()
            });
        }
        let client = self
            .client
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("telemetry sink not connected"))?;
        let url = format!("{}/v1/ingest", self.endpoint);
        let mut accepted = 0u64;
        let mut persisted = 0u64;
        let mut errors: Vec<String> = Vec::new();
        for chunk in events.chunks(INGEST_CHUNK) {
            let resp = client
                .post(&url)
                .bearer_auth(&self.token)
                .header("X-Summa-Token", &self.token)
                .json(&serde_json::json!({ "events": chunk }))
                .send()
                .await?
                .error_for_status()?;
            // `accepted` is the hub's *parse* count, not a count of rows that
            // were persisted, so a body we cannot read proves nothing. Guessing
            // `chunk.len()` here reported a fabricated number of written rows.
            let body: IngestResponse = resp.json().await.map_err(|e| {
                anyhow::anyhow!("telemetry ingest returned an unreadable body: {e}")
            })?;
            fold_response(&body, &mut accepted, &mut persisted, &mut errors);
        }
        let failure = cloud_persist_error(accepted, persisted, &errors);
        if failure.is_none() {
            let mut seen = std::collections::HashSet::new();
            for e in &errors {
                if seen.insert(e.clone()) {
                    eprintln!("warning: summa-cloud: {e}");
                }
            }
        }
        let mut rows_written = HashMap::new();
        rows_written.insert("ccusage_events".into(), accepted);
        Ok(SinkResult {
            sink_name: self.name().to_string(),
            tables_written: vec!["ccusage_events".into()],
            rows_written,
            duration_ms: start.elapsed().as_millis() as u64,
            error: failure,
        })
    }

    async fn close(&mut self) -> anyhow::Result<()> {
        self.client = None;
        Ok(())
    }
}

/// Fold one hub response into the running accepted count and error list.
///
/// The hub answers `200` when *any* sink succeeded (`ingest_status_code`), so a
/// total ClickHouse outage behind a healthy MotherDuck is reported only inside
/// `sinks[].error`. Nothing read that field, so the import printed the full
/// row count and exited 0 while half the rows were never stored.
fn fold_response(
    body: &IngestResponse,
    accepted: &mut u64,
    persisted: &mut u64,
    errors: &mut Vec<String>,
) {
    *accepted += body.accepted as u64;
    if body.rejected > 0 {
        errors.push(format!("hub rejected {} event(s)", body.rejected));
    }
    let mut chunk_persisted = 0u64;
    for sink in &body.sinks {
        if let Some(e) = &sink.error {
            errors.push(format!("{}: {} ({} rows written)", sink.name, e, sink.rows));
        } else {
            chunk_persisted = chunk_persisted.max(sink.rows);
        }
    }
    *persisted += chunk_persisted;
}

/// The cloud sink failed when the hub rejected rows or no replica stored the
/// batch. A dead ClickHouse behind a MotherDuck that stored every accepted
/// row is a warning: analytics reads MotherDuck when ClickHouse is down, so
/// the batch was not dropped.
fn cloud_persist_error(accepted: u64, persisted: u64, errors: &[String]) -> Option<String> {
    if errors.is_empty() {
        return None;
    }
    let rejected = errors.iter().any(|e| e.starts_with("hub rejected"));
    if rejected || persisted < accepted {
        return Some(errors.join("; "));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::telemetry::SinkAck;

    fn ack(name: &str, rows: u64, error: Option<&str>) -> SinkAck {
        SinkAck {
            name: name.into(),
            rows,
            duration_ms: 1,
            error: error.map(str::to_string),
        }
    }

    fn fold(responses: &[IngestResponse]) -> (u64, u64, Vec<String>) {
        let (mut accepted, mut persisted, mut errors) = (0u64, 0u64, Vec::new());
        for r in responses {
            fold_response(r, &mut accepted, &mut persisted, &mut errors);
        }
        (accepted, persisted, errors)
    }

    #[test]
    fn from_parts_requires_token() {
        assert!(TelemetrySink::from_parts(Some(DEFAULT_ENDPOINT), None).is_none());
        assert!(TelemetrySink::from_parts(Some(DEFAULT_ENDPOINT), Some("")).is_none());
        let s = TelemetrySink::from_parts(None, Some("summa_abc")).unwrap();
        assert_eq!(s.endpoint, DEFAULT_ENDPOINT);
        assert_eq!(s.name(), "summa-cloud");
    }

    #[test]
    fn strips_trailing_slash() {
        let s = TelemetrySink::new("https://summa.duyet.net/", "t");
        assert_eq!(s.endpoint, "https://summa.duyet.net");
    }

    #[test]
    fn ingest_chunks_stay_under_worker_cap() {
        assert!(INGEST_CHUNK <= 500);
        assert!(INGEST_CHUNK >= 100);
    }

    #[test]
    fn healthy_response_produces_no_error() {
        let (accepted, persisted, errors) = fold(&[IngestResponse {
            accepted: 3,
            rejected: 0,
            sinks: vec![ack("motherduck", 3, None), ack("clickhouse", 3, None)],
        }]);
        assert_eq!(accepted, 3);
        assert_eq!(persisted, 3);
        assert!(errors.is_empty(), "clean write reported {errors:?}");
        assert!(cloud_persist_error(accepted, persisted, &errors).is_none());
    }

    #[test]
    fn accepted_sums_across_chunks() {
        let (accepted, persisted, errors) = fold(&[
            IngestResponse {
                accepted: 2,
                rejected: 0,
                sinks: vec![ack("motherduck", 2, None)],
            },
            IngestResponse {
                accepted: 5,
                rejected: 0,
                sinks: vec![ack("motherduck", 5, None)],
            },
        ]);
        assert_eq!(accepted, 7);
        assert_eq!(persisted, 7);
        assert!(errors.is_empty());
    }

    /// The regression this fold exists for: the hub returns 200 when any sink
    /// succeeded, so a dead ClickHouse behind a healthy MotherDuck used to look
    /// like a clean full write.
    #[test]
    fn sink_error_hidden_behind_a_200_is_reported() {
        let (accepted, persisted, errors) = fold(&[IngestResponse {
            accepted: 4,
            rejected: 0,
            sinks: vec![
                ack("motherduck", 4, None),
                ack("clickhouse", 0, Some("connection refused")),
            ],
        }]);
        assert_eq!(accepted, 4);
        assert_eq!(persisted, 4);
        assert_eq!(errors.len(), 1, "got {errors:?}");
        let e = &errors[0];
        assert!(e.contains("clickhouse"), "sink name missing from {e:?}");
        assert!(e.contains("connection refused"), "cause missing from {e:?}");
        assert!(e.contains('0'), "written-row count missing from {e:?}");
        // The batch is in MotherDuck. ClickHouse missing is a warning, not a
        // failed cloud write — analytics falls back to MotherDuck.
        assert!(cloud_persist_error(accepted, persisted, &errors).is_none());
    }

    #[test]
    fn cloud_write_fails_when_no_replica_stored_the_batch() {
        let (accepted, persisted, errors) = fold(&[IngestResponse {
            accepted: 4,
            rejected: 0,
            sinks: vec![
                ack("motherduck", 0, Some("disk full")),
                ack("clickhouse", 0, Some("connection refused")),
            ],
        }]);
        assert_eq!(persisted, 0);
        let err = cloud_persist_error(accepted, persisted, &errors).expect("batch was dropped");
        assert!(err.contains("disk full"), "got {err}");
        assert!(err.contains("connection refused"), "got {err}");
    }

    #[test]
    fn hub_rejected_rows_are_reported() {
        let (accepted, persisted, errors) = fold(&[IngestResponse {
            accepted: 10,
            rejected: 2,
            sinks: vec![ack("motherduck", 10, None)],
        }]);
        assert_eq!(accepted, 10);
        assert_eq!(persisted, 10);
        assert_eq!(errors.len(), 1, "got {errors:?}");
        assert!(errors[0].contains("rejected 2"), "got {:?}", errors[0]);
        // Rejected rows never reached a sink, even if the rest were stored.
        let err = cloud_persist_error(accepted, persisted, &errors).expect("rejections");
        assert!(err.contains("rejected 2"), "got {err}");
    }

    #[test]
    fn every_failed_sink_and_rejection_is_listed() {
        let (_, _, errors) = fold(&[IngestResponse {
            accepted: 1,
            rejected: 1,
            sinks: vec![
                ack("motherduck", 0, Some("disk full")),
                ack("clickhouse", 0, Some("timeout")),
            ],
        }]);
        assert_eq!(errors.len(), 3, "got {errors:?}");
    }

    /// A hub that predates the field omits it entirely.
    #[test]
    fn ingest_response_without_rejected_reads_as_zero() {
        let r: IngestResponse = serde_json::from_str(
            r#"{"accepted":2,"sinks":[{"name":"motherduck","rows":2,"duration_ms":1}]}"#,
        )
        .unwrap();
        assert_eq!(r.rejected, 0);
        let (accepted, persisted, errors) = fold(&[r]);
        assert_eq!(accepted, 2);
        assert_eq!(persisted, 2);
        assert!(errors.is_empty());
    }
}
