use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::{Json, Router, routing::get};
use serde_json::json;

use crate::supervisor::Supervisor;

pub struct HealthState {
    controller_connected: AtomicBool,
    supervisor: Arc<Supervisor>,
}

impl HealthState {
    pub fn new(supervisor: Arc<Supervisor>) -> Arc<Self> {
        Arc::new(Self {
            controller_connected: AtomicBool::new(false),
            supervisor,
        })
    }

    pub fn set_controller_connected(&self, connected: bool) {
        self.controller_connected
            .store(connected, Ordering::Release);
    }
}

pub async fn serve(address: std::net::SocketAddr, state: Arc<HealthState>) -> Result<(), String> {
    let app = Router::new()
        .route("/healthz", get(healthz))
        .route("/readyz", get(readyz))
        .route("/metrics", get(metrics))
        .with_state(state);
    let listener = tokio::net::TcpListener::bind(address)
        .await
        .map_err(|error| format!("bind health listener {address}: {error}"))?;
    axum::serve(listener, app)
        .await
        .map_err(|error| format!("serve health listener: {error}"))
}

async fn metrics(State(state): State<Arc<HealthState>>) -> Response {
    let backend = state.supervisor.backend_capability();
    let controller_connected = state.controller_connected.load(Ordering::Acquire);
    let mut body = String::from(
        "# HELP light_runner_controller_connected Whether the controller WebSocket is connected.\n\
# TYPE light_runner_controller_connected gauge\n",
    );
    gauge(
        &mut body,
        "light_runner_controller_connected",
        controller_connected,
    );
    gauge(&mut body, "light_runner_backend_healthy", backend.healthy);
    gauge(
        &mut body,
        "light_runner_journal_healthy",
        state.supervisor.journal_healthy(),
    );
    gauge(
        &mut body,
        "light_runner_watchdog_healthy",
        state.supervisor.watchdog_healthy(),
    );
    gauge(
        &mut body,
        "light_runner_orphan_reconciliation_healthy",
        state.supervisor.orphan_reconciliation_healthy(),
    );
    body.push_str(&format!(
        "light_runner_available_capacity {}\nlight_runner_active_leases {}\nlight_runner_cleanup_backlog {}\n",
        state.supervisor.available_capacity(),
        state.supervisor.active_leases().len(),
        state.supervisor.cleanup_backlog()
    ));
    body.push_str(&format!(
        "light_runner_backend_info{{backend_id=\"{}\",compatibility_digest=\"{}\"}} 1\n",
        prometheus_label(&backend.backend_id),
        prometheus_label(&backend.compatibility_digest)
    ));
    for (journal_state, count) in state.supervisor.journal_state_counts() {
        body.push_str(&format!(
            "light_runner_journal_executions{{state=\"{}\"}} {count}\n",
            prometheus_label(&journal_state)
        ));
    }
    (
        [(
            axum::http::header::CONTENT_TYPE,
            "text/plain; version=0.0.4; charset=utf-8",
        )],
        body,
    )
        .into_response()
}

fn gauge(body: &mut String, name: &str, value: bool) {
    body.push_str(&format!("{name} {}\n", u8::from(value)));
}

fn prometheus_label(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('\n', "\\n")
        .replace('"', "\\\"")
}

async fn healthz(State(state): State<Arc<HealthState>>) -> impl IntoResponse {
    let journal_healthy = state.supervisor.journal_healthy();
    let watchdog_healthy = state.supervisor.watchdog_healthy();
    let orphan_reconciliation_healthy = state.supervisor.orphan_reconciliation_healthy();
    let status = if journal_healthy && watchdog_healthy && orphan_reconciliation_healthy {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };
    (
        status,
        Json(json!({
            "status": if status == StatusCode::OK { "healthy" } else { "unhealthy" },
            "controllerConnected": state.controller_connected.load(Ordering::Acquire),
            "backendHealthy": state.supervisor.backend_capability().healthy,
            "journalHealthy": journal_healthy,
            "watchdogHealthy": watchdog_healthy,
            "orphanReconciliationHealthy": orphan_reconciliation_healthy,
            "cleanupBacklog": state.supervisor.cleanup_backlog()
        })),
    )
}

async fn readyz(State(state): State<Arc<HealthState>>) -> impl IntoResponse {
    let ready = state.controller_connected.load(Ordering::Acquire)
        && state.supervisor.journal_healthy()
        && state.supervisor.orphan_reconciliation_healthy()
        && state.supervisor.backend_capability().healthy;
    (
        if ready {
            StatusCode::OK
        } else {
            StatusCode::SERVICE_UNAVAILABLE
        },
        Json(json!({"ready": ready})),
    )
}

#[cfg(test)]
mod tests {
    use super::prometheus_label;

    #[test]
    fn prometheus_labels_are_escaped() {
        assert_eq!(prometheus_label("a\\b\n\"c"), "a\\\\b\\n\\\"c");
    }
}
