use axum::Json;
use axum::extract::State;
use serde::Serialize;
use uuid::Uuid;

use crate::auth::{Caller, Scope};
use crate::error::AppError;
use crate::http::AppState;

#[derive(Serialize)]
pub struct Me {
    tenant_id: Uuid,
    tenant_name: String,
    scopes: Vec<Scope>,
}

/// Who the presented key belongs to. Any valid key works, whatever its scopes.
pub async fn me(caller: Caller, State(state): State<AppState>) -> Result<Json<Me>, AppError> {
    let tenant_name: Option<String> = sqlx::query_scalar("SELECT name FROM tenants WHERE id = $1")
        .bind(caller.tenant_id)
        .fetch_optional(&state.pool)
        .await?;
    // The key row cascades with its tenant, so a missing tenant means the key is gone too.
    let tenant_name = tenant_name.ok_or(AppError::Unauthorized)?;
    Ok(Json(Me {
        tenant_id: caller.tenant_id,
        tenant_name,
        scopes: caller.scopes,
    }))
}
