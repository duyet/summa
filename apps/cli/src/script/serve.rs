//! `summa serve` — telemetry ingest + analytics HTTP.

use std::sync::Arc;
use std::time::Instant;

use axum::extract::{Query, State};
use axum::http::{header, HeaderMap, HeaderValue, Method, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use clap::Parser;
use serde::Deserialize;
use tokio::sync::RwLock;

use crate::config::Config;
use crate::model::{DataSink, EventRow};
use crate::script::import_all::apply_config_to_env;
use crate::sink::clickhouse::ClickHouseSink;
use crate::sink::duckdb::DuckDbSink;
use crate::telemetry::{
    analytics_sql, analytics_window, bearer_ok, clickhouse_analytics_query, cors_allow_origin,
    ingest_status_code, ping_ok, prepare_events, sidebar_html, summarize_points, AnalyticsBody,
    AnalyticsPoint, HealthBody, IngestBody, IngestResponse, PingBody, PingSample, SinkAck,
    StatusBody,
};
use crate::util::date::ch_now;

#[derive(Parser, Debug, Clone)]
pub struct ServeArgs {
    #[arg(short, long)]
    pub config: Option<String>,
    #[arg(long)]
    pub bind: Option<String>,
}

struct AppState {
    bind: String,
    token: String,
    duckdb_path: String,
    clickhouse_enabled: bool,
    status: RwLock<StatusBody>,
}

pub async fn run(args: ServeArgs) -> anyhow::Result<()> {
    dotenvy::dotenv().ok();
    let cfg = Config::load(args.config.as_deref())?;
    apply_config_to_env(&cfg);
    let endpoint = cfg
        .telemetry
        .endpoint
        .clone()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| crate::telemetry::DEFAULT_ENDPOINT.to_string());
    eprintln!("summa serve is replaced by the cloud hub at {endpoint}");
    eprintln!("Clients POST /v1/ingest with credentials.toml telemetry_token.");
    eprintln!("Configure:");
    eprintln!("  [telemetry]");
    eprintln!("  endpoint = \"{endpoint}\"");
    let url = format!("{}/health", endpoint.trim_end_matches('/'));
    match reqwest::Client::new().get(&url).send().await {
        Ok(resp) => {
            println!("hub {} {}", url, resp.status());
            if let Ok(body) = resp.text().await {
                println!("{body}");
            }
        }
        Err(e) => eprintln!("hub unreachable: {e}"),
    }
    let _ = args.bind;
    Ok(())
}

fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/", get(sidebar))
        .route("/health", get(health))
        .route("/ping", get(ping))
        .route("/status", get(status))
        .route("/v1/ingest", post(ingest))
        .route("/v1/analytics", get(analytics))
        .route("/v1/analytics/summary", get(analytics_summary))
        .fallback(options_or_404)
        .layer(axum::middleware::from_fn(cors_layer))
        .with_state(state)
}

async fn cors_layer(
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    let origin = req
        .headers()
        .get(header::ORIGIN)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let path = req.uri().path().to_string();
    let public = path == "/health";
    if req.method() == Method::OPTIONS {
        return cors_response(
            origin.as_deref(),
            public,
            StatusCode::NO_CONTENT.into_response(),
        );
    }
    let resp = next.run(req).await;
    cors_response(origin.as_deref(), public, resp)
}

fn cors_response(origin: Option<&str>, public: bool, mut resp: Response) -> Response {
    let allow = cors_allow_origin(origin).or_else(|| public.then(|| "*".into()));
    if let Some(o) = allow {
        if let Ok(v) = HeaderValue::from_str(&o) {
            resp.headers_mut()
                .insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, v);
        }
        resp.headers_mut().insert(
            header::ACCESS_CONTROL_ALLOW_HEADERS,
            HeaderValue::from_static("Authorization, Content-Type, X-Summa-Token"),
        );
        resp.headers_mut().insert(
            header::ACCESS_CONTROL_ALLOW_METHODS,
            HeaderValue::from_static("GET, POST, OPTIONS"),
        );
    }
    resp
}

async fn options_or_404(method: Method) -> Response {
    if method == Method::OPTIONS {
        StatusCode::NO_CONTENT.into_response()
    } else {
        (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": "not found"})),
        )
            .into_response()
    }
}

fn unauthorized() -> Response {
    (
        StatusCode::UNAUTHORIZED,
        Json(serde_json::json!({"error": "unauthorized"})),
    )
        .into_response()
}

fn require_auth(state: &AppState, headers: &HeaderMap) -> Result<(), Response> {
    let auth = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok());
    let alt = headers
        .get("x-summa-token")
        .and_then(|v| v.to_str().ok());
    if bearer_ok(&state.token, auth, alt) {
        Ok(())
    } else {
        Err(unauthorized())
    }
}

async fn sidebar(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    Html(sidebar_html(&state.bind, env!("CARGO_PKG_VERSION")))
}

async fn health() -> Json<HealthBody> {
    Json(HealthBody {
        ok: true,
        service: "summa",
        version: env!("CARGO_PKG_VERSION"),
    })
}

async fn ping(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    if let Err(resp) = require_auth(&state, &headers) {
        return resp;
    }
    let samples = collect_pings(&state).await;
    let body = PingBody {
        ok: ping_ok(&samples),
        samples: samples.clone(),
    };
    {
        let mut st = state.status.write().await;
        st.ping = samples;
        st.ok = body.ok;
    }
    Json(body).into_response()
}

async fn status(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    if let Err(resp) = require_auth(&state, &headers) {
        return resp;
    }
    let body = state.status.read().await.clone();
    Json(body).into_response()
}

async fn ingest(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(body): Json<IngestBody>,
) -> Response {
    if let Err(resp) = require_auth(&state, &headers) {
        return resp;
    }
    let events = prepare_events(body.events);
    let accepted = events.len();
    let sinks = fanout_write(&state, events).await;
    let code = ingest_status_code(&sinks);
    {
        let mut st = state.status.write().await;
        st.last_ingest_at = Some(ch_now());
        st.last_accepted = accepted as u64;
        st.sinks = sinks.clone();
        st.ok = code == 200;
    }
    let status = StatusCode::from_u16(code).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    (status, Json(IngestResponse { accepted, sinks })).into_response()
}

#[derive(Debug, Deserialize)]
struct AnalyticsQuery {
    since: Option<String>,
    until: Option<String>,
    group: Option<String>,
    days: Option<i64>,
}

async fn analytics(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(q): Query<AnalyticsQuery>,
) -> Response {
    if let Err(resp) = require_auth(&state, &headers) {
        return resp;
    }
    let group = q.group.as_deref().unwrap_or("source");
    let (since, until) = match analytics_window(q.since.as_deref(), q.until.as_deref(), q.days) {
        Ok(w) => w,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"error": e.to_string()})),
            )
                .into_response();
        }
    };
    match load_points(&state, group, &since, &until).await {
        Ok(points) => Json(AnalyticsBody {
            since,
            until,
            group: group.to_string(),
            points,
        })
        .into_response(),
        Err(e) => (
            StatusCode::BAD_GATEWAY,
            Json(serde_json::json!({"error": e.to_string()})),
        )
            .into_response(),
    }
}

async fn analytics_summary(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(q): Query<AnalyticsQuery>,
) -> Response {
    if let Err(resp) = require_auth(&state, &headers) {
        return resp;
    }
    let (since, until) = match analytics_window(q.since.as_deref(), q.until.as_deref(), q.days.or(Some(7)))
    {
        Ok(w) => w,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"error": e.to_string()})),
            )
                .into_response();
        }
    };
    match load_points(&state, "source", &since, &until).await {
        Ok(points) => Json(summarize_points(&since, &until, &points)).into_response(),
        Err(e) => (
            StatusCode::BAD_GATEWAY,
            Json(serde_json::json!({"error": e.to_string()})),
        )
            .into_response(),
    }
}

async fn collect_pings(state: &AppState) -> Vec<PingSample> {
    let duck_path = state.duckdb_path.clone();
    let duck_fut = async move {
        let start = Instant::now();
        let name = if duck_path.starts_with("md:") {
            "motherduck"
        } else {
            "duckdb"
        };
        let path = duck_path;
        let res = tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
            let conn = DuckDbSink::new(path).open_for_query()?;
            conn.execute("SELECT 1", [])?;
            Ok(())
        })
        .await;
        ping_from_join(name, start, res)
    };

    if state.clickhouse_enabled {
        let ch_fut = async {
            let start = Instant::now();
            let res = ping_clickhouse().await;
            match res {
                Ok(()) => PingSample {
                    name: "clickhouse".into(),
                    ok: true,
                    latency_ms: start.elapsed().as_millis() as u64,
                    error: None,
                },
                Err(e) => PingSample {
                    name: "clickhouse".into(),
                    ok: false,
                    latency_ms: start.elapsed().as_millis() as u64,
                    error: Some(e.to_string()),
                },
            }
        };
        let (a, b) = tokio::join!(duck_fut, ch_fut);
        vec![a, b]
    } else {
        vec![duck_fut.await]
    }
}

fn ping_from_join(
    name: &str,
    start: Instant,
    res: Result<anyhow::Result<()>, tokio::task::JoinError>,
) -> PingSample {
    let latency_ms = start.elapsed().as_millis() as u64;
    match res {
        Ok(Ok(())) => PingSample {
            name: name.into(),
            ok: true,
            latency_ms,
            error: None,
        },
        Ok(Err(e)) => PingSample {
            name: name.into(),
            ok: false,
            latency_ms,
            error: Some(e.to_string()),
        },
        Err(e) => PingSample {
            name: name.into(),
            ok: false,
            latency_ms,
            error: Some(e.to_string()),
        },
    }
}

async fn ping_clickhouse() -> anyhow::Result<()> {
    let mut sink = ClickHouseSink::new();
    sink.connect().await?;
    sink.ping().await
}

async fn fanout_write(state: &AppState, events: Vec<EventRow>) -> Vec<SinkAck> {
    let duck_configured = !state.duckdb_path.is_empty();
    if !duck_configured && !state.clickhouse_enabled {
        return Vec::new();
    }
    if events.is_empty() {
        let mut acks = Vec::new();
        if duck_configured {
            acks.push(SinkAck {
                name: if state.duckdb_path.starts_with("md:") {
                    "motherduck".into()
                } else {
                    "duckdb".into()
                },
                rows: 0,
                duration_ms: 0,
                error: None,
            });
        }
        if state.clickhouse_enabled {
            acks.push(SinkAck {
                name: "clickhouse".into(),
                rows: 0,
                duration_ms: 0,
                error: None,
            });
        }
        return acks;
    }
    let duck_path = state.duckdb_path.clone();
    let duck_name = if duck_path.starts_with("md:") {
        "motherduck"
    } else {
        "duckdb"
    }
    .to_string();
    let duck_rows = events.clone();
    let duck_fut = async move {
        if duck_path.is_empty() {
            return None;
        }
        let start = Instant::now();
        let res = tokio::task::spawn_blocking(move || {
            let mut sink = DuckDbSink::new(duck_path);
            sink.write_events_by_dedup_key(&duck_rows)
        })
        .await;
        Some(match res {
            Ok(Ok(n)) => SinkAck {
                name: duck_name,
                rows: n as u64,
                duration_ms: start.elapsed().as_millis() as u64,
                error: None,
            },
            Ok(Err(e)) => SinkAck {
                name: duck_name,
                rows: 0,
                duration_ms: start.elapsed().as_millis() as u64,
                error: Some(e.to_string()),
            },
            Err(e) => SinkAck {
                name: duck_name,
                rows: 0,
                duration_ms: start.elapsed().as_millis() as u64,
                error: Some(e.to_string()),
            },
        })
    };

    let ch_enabled = state.clickhouse_enabled;
    let ch_fut = async move {
        if !ch_enabled {
            return None;
        }
        let start = Instant::now();
        Some(match write_clickhouse(events).await {
            Ok(n) => SinkAck {
                name: "clickhouse".into(),
                rows: n,
                duration_ms: start.elapsed().as_millis() as u64,
                error: None,
            },
            Err(e) => SinkAck {
                name: "clickhouse".into(),
                rows: 0,
                duration_ms: start.elapsed().as_millis() as u64,
                error: Some(e.to_string()),
            },
        })
    };
    let (a, b) = tokio::join!(duck_fut, ch_fut);
    [a, b].into_iter().flatten().collect()
}

async fn write_clickhouse(rows: Vec<EventRow>) -> anyhow::Result<u64> {
    let mut sink = ClickHouseSink::new();
    sink.connect().await?;
    // Insert-only: ReplacingMergeTree(updated_at) collapses the same ORDER BY
    // key. Deleting first would open a crash window where rows are gone.
    sink.insert_events(&rows).await?;
    Ok(rows.len() as u64)
}

async fn load_points(
    state: &AppState,
    group: &str,
    since: &str,
    until: &str,
) -> anyhow::Result<Vec<AnalyticsPoint>> {
    let path = state.duckdb_path.clone();
    let group_s = group.to_string();
    let since_s = since.to_string();
    let until_s = until.to_string();
    let duck = tokio::task::spawn_blocking(move || {
        analytics_from_duckdb(&path, &group_s, &since_s, &until_s)
    })
    .await?;
    match duck {
        Ok(points) => Ok(points),
        Err(e) if state.clickhouse_enabled => {
            match analytics_from_clickhouse(group, since, until).await {
                Ok(points) => Ok(points),
                Err(ch) => Err(anyhow::anyhow!("duckdb: {e}; clickhouse: {ch}")),
            }
        }
        Err(e) => Err(e),
    }
}

fn analytics_from_duckdb(
    db: &str,
    group: &str,
    since: &str,
    until: &str,
) -> anyhow::Result<Vec<AnalyticsPoint>> {
    let conn = if db.starts_with("md:") {
        DuckDbSink::new(db).open_for_query()?
    } else {
        duckdb::Connection::open(db)?
    };
    let sql = analytics_sql(group);
    let mut stmt = conn.prepare(sql)?;
    let mut rows = stmt.query(duckdb::params![since, until])?;
    let mut out = Vec::new();
    while let Some(row) = rows.next()? {
        out.push(AnalyticsPoint {
            date: row.get::<_, String>(0).unwrap_or_else(|_| String::new()),
            source: row.get::<_, String>(1).unwrap_or_else(|_| String::new()),
            model_name: row.get::<_, String>(2).unwrap_or_else(|_| String::new()),
            cost: row_f64(row, 3),
            total_tokens: row_u64(row, 4),
            entries: row_u64(row, 5),
        });
    }
    Ok(out)
}

fn row_f64(row: &duckdb::Row<'_>, idx: usize) -> f64 {
    row.get::<_, f64>(idx)
        .or_else(|_| row.get::<_, i64>(idx).map(|n| n as f64))
        .unwrap_or(0.0)
}

fn row_u64(row: &duckdb::Row<'_>, idx: usize) -> u64 {
    row.get::<_, i64>(idx)
        .map(|n| n.max(0) as u64)
        .or_else(|_| row.get::<_, f64>(idx).map(|n| n.max(0.0) as u64))
        .unwrap_or(0)
}

async fn analytics_from_clickhouse(
    group: &str,
    since: &str,
    until: &str,
) -> anyhow::Result<Vec<AnalyticsPoint>> {
    let mut sink = ClickHouseSink::new();
    sink.connect().await?;
    let sql = clickhouse_analytics_query(group, since, until)?;
    let text = sink.query_text(&sql).await?;
    parse_json_each_row(&text)
}

fn parse_json_each_row(text: &str) -> anyhow::Result<Vec<AnalyticsPoint>> {
    let mut out = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let v: serde_json::Value = serde_json::from_str(line)?;
        out.push(AnalyticsPoint {
            date: json_string(&v, "date"),
            source: json_string(&v, "source"),
            model_name: json_string(&v, "model_name"),
            cost: v.get("cost").and_then(|x| x.as_f64()).unwrap_or(0.0),
            total_tokens: json_u64(&v, "total_tokens"),
            entries: json_u64(&v, "entries"),
        });
    }
    Ok(out)
}

fn json_string(v: &serde_json::Value, key: &str) -> String {
    v.get(key)
        .and_then(|x| x.as_str())
        .unwrap_or("")
        .to_string()
}

fn json_u64(v: &serde_json::Value, key: &str) -> u64 {
    v.get(key)
        .and_then(|x| x.as_u64().or_else(|| x.as_i64().map(|n| n.max(0) as u64)))
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::EventRow;
    use tower::ServiceExt;

    fn test_state(db: &str, token: &str) -> Arc<AppState> {
        Arc::new(AppState {
            bind: "127.0.0.1:0".into(),
            token: token.into(),
            duckdb_path: db.into(),
            clickhouse_enabled: false,
            status: RwLock::new(StatusBody {
                ok: true,
                bind: "127.0.0.1:0".into(),
                ..StatusBody::default()
            }),
        })
    }

    #[tokio::test]
    async fn health_is_public() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("t.duckdb").display().to_string();
        let app = router(test_state(&db, "secret"));
        let resp = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/health")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(resp.into_body(), 64 * 1024)
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["service"], "summa");
        assert_eq!(v["ok"], true);
    }

    #[tokio::test]
    async fn ingest_requires_token() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("t.duckdb").display().to_string();
        let app = router(test_state(&db, "secret"));
        let resp = app
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/v1/ingest")
                    .header("content-type", "application/json")
                    .body(axum::body::Body::from(r#"{"events":[]}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn ingest_fans_out_to_duckdb_and_status() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("t.duckdb").display().to_string();
        let state = test_state(&db, "secret");
        let app = router(state.clone());
        let mut row = EventRow::default();
        row.date = "2026-08-20".into();
        row.record_type = "daily".into();
        row.record_key = "2026-08-20".into();
        row.source = "cursor".into();
        row.machine_name = "account".into();
        row.model_name = "grok-4.5".into();
        row.cost = 1.25;
        row.total_tokens = 100;
        row.entries = 1;
        let body = serde_json::json!({"events": [row]});
        let resp = app
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/v1/ingest")
                    .header("content-type", "application/json")
                    .header("authorization", "Bearer secret")
                    .body(axum::body::Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(resp.into_body(), 64 * 1024)
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["accepted"], 1);

        let app = router(state);
        let resp = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/v1/analytics?since=2026-08-20&until=2026-08-20")
                    .header("authorization", "Bearer secret")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(resp.into_body(), 64 * 1024)
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["points"][0]["source"], "cursor");
        assert!((v["points"][0]["cost"].as_f64().unwrap() - 1.25).abs() < 1e-9);
    }

    #[test]
    fn parse_ch_json_each_row() {
        let text = r#"{"date":"2026-08-20","source":"cursor","model_name":"","cost":1.5,"total_tokens":10,"entries":2}
"#;
        let pts = parse_json_each_row(text).unwrap();
        assert_eq!(pts.len(), 1);
        assert_eq!(pts[0].source, "cursor");
        assert!((pts[0].cost - 1.5).abs() < 1e-9);
    }

    #[tokio::test]
    async fn ping_duckdb_tempfile() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("t.duckdb").display().to_string();
        let app = router(test_state(&db, "secret"));
        let resp = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/ping")
                    .header("authorization", "Bearer secret")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(resp.into_body(), 64 * 1024)
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["ok"], true);
        assert_eq!(v["samples"][0]["name"], "duckdb");
        assert_eq!(v["samples"][0]["ok"], true);
    }

    #[tokio::test]
    async fn http_smoke_bind() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("t.duckdb").display().to_string();
        let state = test_state(&db, "secret");
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, router(state)).await.ok();
        });
        let client = reqwest::Client::new();
        let base = format!("http://{addr}");
        let mut health = None;
        for _ in 0..50 {
            match client.get(format!("{base}/health")).send().await {
                Ok(r) => {
                    health = Some(r);
                    break;
                }
                Err(_) => {
                    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                }
            }
        }
        let health = health.expect("server should accept /health");
        assert_eq!(health.status(), reqwest::StatusCode::OK);
        let body: serde_json::Value = health.json().await.unwrap();
        assert_eq!(body["service"], "summa");

        let unauthorized = client
            .post(format!("{base}/v1/ingest"))
            .header("content-type", "application/json")
            .body(r#"{"events":[]}"#)
            .send()
            .await
            .unwrap();
        assert_eq!(unauthorized.status(), reqwest::StatusCode::UNAUTHORIZED);

        let ping = client.get(format!("{base}/ping")).send().await.unwrap();
        assert_eq!(ping.status(), reqwest::StatusCode::UNAUTHORIZED);
        let ping = client
            .get(format!("{base}/ping"))
            .bearer_auth("secret")
            .send()
            .await
            .unwrap();
        assert_eq!(ping.status(), reqwest::StatusCode::OK);
        let body: serde_json::Value = ping.json().await.unwrap();
        assert_eq!(body["ok"], true);
    }
}
