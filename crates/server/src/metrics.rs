//! Prometheus metrics.
//!
//! One process-wide registry ([`get`]), fed from wherever things happen
//! (connections, the relay, logins, access checks, the audit log, HTTP
//! requests), and rendered in the OpenMetrics text format by
//! [`router`]. Serve it on its own listener (`serve --metrics-listen`),
//! reachable only by the monitoring system: it needs no credentials, and it
//! reveals activity levels but never agent ids, user names or content.
//!
//! Label values are always from small fixed sets (transport, outcome,
//! capability, route template, ...), so the series count stays bounded
//! however many agents connect.
//!
//! Some values are read at scrape time rather than counted: agents in the
//! registry, and the database pool.

use std::sync::{Arc, LazyLock};
use std::time::Instant;

use axum::extract::{MatchedPath, Request, State};
use axum::http::{header, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::routing;
use axum::Router;
use prometheus_client::encoding::EncodeLabelSet;
use prometheus_client::metrics::counter::Counter;
use prometheus_client::metrics::family::Family;
use prometheus_client::metrics::gauge::Gauge;
use prometheus_client::metrics::histogram::{exponential_buckets, Histogram};
use prometheus_client::registry::Registry;
use sqlx::PgPool;

#[derive(Debug, Clone, Hash, PartialEq, Eq, EncodeLabelSet)]
pub struct TransportLabels {
    pub transport: &'static str,
}

#[derive(Debug, Clone, Hash, PartialEq, Eq, EncodeLabelSet)]
pub struct ReasonLabels {
    pub reason: &'static str,
}

#[derive(Debug, Clone, Hash, PartialEq, Eq, EncodeLabelSet)]
pub struct ResultLabels {
    pub result: &'static str,
}

#[derive(Debug, Clone, Hash, PartialEq, Eq, EncodeLabelSet)]
pub struct SessionLabels {
    pub mode: &'static str,
    pub outcome: &'static str,
}

#[derive(Debug, Clone, Hash, PartialEq, Eq, EncodeLabelSet)]
pub struct StatusLabels {
    pub status: &'static str,
}

#[derive(Debug, Clone, Hash, PartialEq, Eq, EncodeLabelSet)]
pub struct DirectionLabels {
    pub direction: &'static str,
}

#[derive(Debug, Clone, Hash, PartialEq, Eq, EncodeLabelSet)]
pub struct DeniedLabels {
    /// A capability (`desktop`, `shell`, ...), or `admin` for the admin API.
    pub capability: &'static str,
}

#[derive(Debug, Clone, Hash, PartialEq, Eq, EncodeLabelSet)]
pub struct HttpLabels {
    pub method: String,
    /// The route template (`/api/agents/{id}/shell`), never the raw path.
    pub route: String,
    pub status: u16,
}

#[derive(Debug, Clone, Hash, PartialEq, Eq, EncodeLabelSet)]
pub struct RouteLabels {
    pub method: String,
    pub route: String,
}

#[derive(Debug, Clone, Hash, PartialEq, Eq, EncodeLabelSet)]
pub struct VersionLabels {
    pub version: &'static str,
}

pub struct Metrics {
    registry: Registry,
    // Connections.
    pub agents_connected: Family<TransportLabels, Gauge>,
    pub viewers_connected: Family<TransportLabels, Gauge>,
    pub connections_rejected: Family<ReasonLabels, Counter>,
    pub heartbeats: Counter,
    pub enrollments: Family<ResultLabels, Counter>,
    // Relay.
    pub relay_frames_in: Counter,
    pub relay_bytes_in: Counter,
    pub relay_frames_out: Counter,
    pub relay_bytes_out: Counter,
    pub relay_frames_dropped: Counter,
    pub viewer_delay: Histogram,
    // Sessions and remote operations.
    pub sessions: Family<SessionLabels, Counter>,
    pub user_terminations: Counter,
    pub shells_open: Gauge,
    pub script_runs: Counter,
    pub script_results: Family<StatusLabels, Counter>,
    pub file_bytes: Family<DirectionLabels, Counter>,
    // Security.
    pub logins: Family<ResultLabels, Counter>,
    pub permission_denied: Family<DeniedLabels, Counter>,
    pub audit_entries: Counter,
    // HTTP API.
    pub http_requests: Family<HttpLabels, Counter>,
    pub http_duration: Family<RouteLabels, Histogram>,
    // Read at scrape time.
    agents_registered: Gauge,
    db_connections: Gauge,
    db_connections_idle: Gauge,
}

fn http_histogram() -> Histogram {
    // 5 ms .. ~10 s.
    Histogram::new(exponential_buckets(0.005, 2.0, 12))
}

impl Metrics {
    fn new() -> Self {
        let mut registry = Registry::with_prefix("rmm");
        let m = Metrics {
            agents_connected: Family::default(),
            viewers_connected: Family::default(),
            connections_rejected: Family::default(),
            heartbeats: Counter::default(),
            enrollments: Family::default(),
            relay_frames_in: Counter::default(),
            relay_bytes_in: Counter::default(),
            relay_frames_out: Counter::default(),
            relay_bytes_out: Counter::default(),
            relay_frames_dropped: Counter::default(),
            // 10 ms .. ~20 s.
            viewer_delay: Histogram::new(exponential_buckets(0.01, 2.0, 12)),
            sessions: Family::default(),
            user_terminations: Counter::default(),
            shells_open: Gauge::default(),
            script_runs: Counter::default(),
            script_results: Family::default(),
            file_bytes: Family::default(),
            logins: Family::default(),
            permission_denied: Family::default(),
            audit_entries: Counter::default(),
            http_requests: Family::default(),
            http_duration: Family::new_with_constructor(http_histogram),
            agents_registered: Gauge::default(),
            db_connections: Gauge::default(),
            db_connections_idle: Gauge::default(),
            registry: Registry::default(),
        };
        let r = &mut registry;
        r.register(
            "agents_connected",
            "Agents connected now, by transport (quic, websocket)",
            m.agents_connected.clone(),
        );
        r.register(
            "viewers_connected",
            "Remote-desktop viewers in a session now, by transport",
            m.viewers_connected.clone(),
        );
        r.register(
            "connections_rejected",
            "Agent and viewer connections refused, by reason",
            m.connections_rejected.clone(),
        );
        r.register(
            "agent_heartbeats",
            "Agent heartbeats received",
            m.heartbeats.clone(),
        );
        r.register(
            "enrollments",
            "Enrollment attempts, by result",
            m.enrollments.clone(),
        );
        r.register(
            "relay_frames_received",
            "Video frames received from agents",
            m.relay_frames_in.clone(),
        );
        r.register(
            "relay_received_bytes",
            "Video payload bytes received from agents",
            m.relay_bytes_in.clone(),
        );
        r.register(
            "relay_frames_sent",
            "Video frames sent to viewers",
            m.relay_frames_out.clone(),
        );
        r.register(
            "relay_sent_bytes",
            "Video payload bytes sent to viewers",
            m.relay_bytes_out.clone(),
        );
        r.register(
            "relay_frames_dropped",
            "Video frames skipped for a viewer that fell behind",
            m.relay_frames_dropped.clone(),
        );
        r.register(
            "stream_viewer_delay_seconds",
            "Worst viewer queueing delay reported to agents (adaptive bitrate)",
            m.viewer_delay.clone(),
        );
        r.register(
            "sessions",
            "Remote-desktop session requests, by consent mode and outcome",
            m.sessions.clone(),
        );
        r.register(
            "user_terminations",
            "Times a user pressed Ctrl+F12 to end remote sessions",
            m.user_terminations.clone(),
        );
        r.register(
            "shells_open",
            "Interactive shells open now",
            m.shells_open.clone(),
        );
        r.register("script_runs", "Script runs started", m.script_runs.clone());
        r.register(
            "script_agent_results",
            "Per-agent script results, by status",
            m.script_results.clone(),
        );
        r.register(
            "file_transfer_bytes",
            "File transfer bytes, by direction (upload, download)",
            m.file_bytes.clone(),
        );
        r.register("logins", "Login attempts, by result", m.logins.clone());
        r.register(
            "permission_denied",
            "Refused actions, by capability (or admin)",
            m.permission_denied.clone(),
        );
        r.register(
            "audit_entries",
            "Entries appended to the audit log",
            m.audit_entries.clone(),
        );
        r.register(
            "http_requests",
            "HTTPS API requests, by method, route and status",
            m.http_requests.clone(),
        );
        r.register(
            "http_request_duration_seconds",
            "HTTPS API request latency, by method and route",
            m.http_duration.clone(),
        );
        r.register(
            "agents_registered",
            "Enrolled agents in the registry",
            m.agents_registered.clone(),
        );
        r.register(
            "db_connections",
            "Database connections in the pool",
            m.db_connections.clone(),
        );
        r.register(
            "db_connections_idle",
            "Idle database connections in the pool",
            m.db_connections_idle.clone(),
        );
        let build = Family::<VersionLabels, Gauge>::default();
        build
            .get_or_create(&VersionLabels {
                version: env!("CARGO_PKG_VERSION"),
            })
            .set(1);
        r.register("build_info", "Server version", build);
        Metrics { registry, ..m }
    }

    /// The OpenMetrics text exposition.
    pub fn render(&self) -> String {
        let mut out = String::new();
        prometheus_client::encoding::text::encode(&mut out, &self.registry)
            .expect("writing to a String cannot fail");
        out
    }

    /// Refresh the values read at scrape time.
    async fn refresh(&self, pool: &PgPool) {
        if let Ok(n) = sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM agents WHERE enrollment_state = 'enrolled'",
        )
        .fetch_one(pool)
        .await
        {
            self.agents_registered.set(n);
        }
        self.db_connections.set(pool.size().into());
        self.db_connections_idle
            .set(i64::try_from(pool.num_idle()).unwrap_or(i64::MAX));
    }

    pub fn transport(&self, kind: transport::TransportKind) -> TransportLabels {
        TransportLabels {
            transport: kind.as_str(),
        }
    }
}

static METRICS: LazyLock<Metrics> = LazyLock::new(Metrics::new);

/// The process's metrics.
pub fn get() -> &'static Metrics {
    &METRICS
}

/// Counts a connected agent or viewer until dropped.
pub struct ConnectedGuard(&'static Family<TransportLabels, Gauge>, TransportLabels);

impl ConnectedGuard {
    pub fn agent(kind: transport::TransportKind) -> Self {
        Self::new(&get().agents_connected, kind)
    }

    pub fn viewer(kind: transport::TransportKind) -> Self {
        Self::new(&get().viewers_connected, kind)
    }

    fn new(
        family: &'static Family<TransportLabels, Gauge>,
        kind: transport::TransportKind,
    ) -> Self {
        let labels = get().transport(kind);
        family.get_or_create(&labels).inc();
        Self(family, labels)
    }
}

impl Drop for ConnectedGuard {
    fn drop(&mut self) {
        self.0.get_or_create(&self.1).dec();
    }
}

/// Middleware recording each API request's route, status and latency.
pub async fn track_http(request: Request, next: Next) -> Response {
    let started = Instant::now();
    let method = request.method().as_str().to_owned();
    let route = request
        .extensions()
        .get::<MatchedPath>()
        .map_or_else(|| "unmatched".to_owned(), |p| p.as_str().to_owned());
    let response = next.run(request).await;
    let metrics = get();
    metrics
        .http_duration
        .get_or_create(&RouteLabels {
            method: method.clone(),
            route: route.clone(),
        })
        .observe(started.elapsed().as_secs_f64());
    metrics
        .http_requests
        .get_or_create(&HttpLabels {
            method,
            route,
            status: response.status().as_u16(),
        })
        .inc();
    response
}

/// `GET /metrics`.
pub fn router(pool: PgPool) -> Router {
    Router::new()
        .route("/metrics", routing::get(scrape))
        .with_state(Arc::new(pool))
}

async fn scrape(State(pool): State<Arc<PgPool>>) -> Response {
    let metrics = get();
    metrics.refresh(&pool).await;
    (
        StatusCode::OK,
        [(
            header::CONTENT_TYPE,
            "application/openmetrics-text; version=1.0.0; charset=utf-8",
        )],
        metrics.render(),
    )
        .into_response()
}

/// Serve [`router`] over plain HTTP on an already-bound listener.
pub async fn serve(listener: tokio::net::TcpListener, pool: PgPool) -> std::io::Result<()> {
    axum::serve(listener, router(pool)).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metrics_render_in_openmetrics_format_with_the_rmm_prefix() {
        let m = get();
        m.heartbeats.inc();
        m.logins
            .get_or_create(&ResultLabels { result: "success" })
            .inc();
        let _agent = ConnectedGuard::agent(transport::TransportKind::WebSocket);
        let text = m.render();
        assert!(
            text.contains("# TYPE rmm_agent_heartbeats counter"),
            "{text}"
        );
        assert!(
            text.contains("rmm_logins_total{result=\"success\"}"),
            "{text}"
        );
        assert!(
            text.contains("rmm_agents_connected{transport=\"websocket\"} 1"),
            "{text}"
        );
        assert!(text.contains("rmm_build_info{version="), "{text}");
        assert!(text.ends_with("# EOF\n"));
    }
}
