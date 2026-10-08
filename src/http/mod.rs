use std::sync::Arc;

use axum::extract::{FromRequest, Request, State};
use axum::http::StatusCode;
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use serde::Serialize;
use serde::de::DeserializeOwned;
use sqlx::PgPool;
use tower_http::trace::TraceLayer;

use crate::config::Config;
use crate::embed::Embedder;
use crate::error::AppError;

pub mod admin;
pub mod me;

#[derive(Clone)]
pub struct AppState {
    pub pool: PgPool,
    pub config: Arc<Config>,
    pub embedder: Arc<dyn Embedder>,
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/healthz", get(healthz))
        .route("/livez", get(livez))
        .route("/v1/me", get(me::me))
        .route(
            "/v1/admin/tenants",
            post(admin::create_tenant).get(admin::list_tenants),
        )
        .route(
            "/v1/admin/tenants/{id}/keys",
            post(admin::create_key).get(admin::list_keys),
        )
        .route("/v1/admin/keys/{id}", delete(admin::revoke_key))
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}

/// `Json` with rejections turned into the standard 400 error body.
pub struct AppJson<T>(pub T);

impl<S, T> FromRequest<S> for AppJson<T>
where
    S: Send + Sync,
    T: DeserializeOwned,
{
    type Rejection = AppError;

    async fn from_request(req: Request, state: &S) -> Result<Self, Self::Rejection> {
        match Json::<T>::from_request(req, state).await {
            Ok(Json(v)) => Ok(AppJson(v)),
            Err(rejection) => Err(AppError::BadRequest(rejection.body_text())),
        }
    }
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
