use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
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
        .with_state(state);
    let listener = tokio::net::TcpListener::bind(address)
        .await
        .map_err(|error| format!("bind health listener {address}: {error}"))?;
    axum::serve(listener, app)
        .await
        .map_err(|error| format!("serve health listener: {error}"))
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
