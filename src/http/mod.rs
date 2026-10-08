use std::sync::Arc;

use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::get;
use axum::{Json, Router};
use serde::Serialize;
use sqlx::PgPool;
use tower_http::trace::TraceLayer;

use crate::config::Config;

#[derive(Clone)]
pub struct AppState {
    pub pool: PgPool,
    pub config: Arc<Config>,
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/healthz", get(healthz))
        .route("/livez", get(livez))
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}

#[derive(Serialize)]
struct Health {
    status: &'static str,
    db: &'static str,
}

#[derive(Serialize)]
struct Live {
    status: &'static str,
}

async fn healthz(State(state): State<AppState>) -> (StatusCode, Json<Health>) {
    match sqlx::query("SELECT 1").execute(&state.pool).await {
        Ok(_) => (
            StatusCode::OK,
            Json(Health {
                status: "ok",
                db: "ok",
            }),
        ),
        Err(e) => {
            tracing::error!(error = %e, "health check: database error");
            (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(Health {
                    status: "degraded",
                    db: "error",
                }),
            )
        }
    }
}

async fn livez() -> Json<Live> {
    Json(Live { status: "ok" })
}
