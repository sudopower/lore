//! `POST /v1/ask`: an answer with citations.

use axum::Json;
use axum::extract::State;

use crate::answer::{AskRequest, AskResponse, ask};
use crate::auth::{Caller, Scope};
use crate::error::AppError;
use crate::http::{AppJson, AppState};

pub async fn post_ask(
    caller: Caller,
    State(state): State<AppState>,
    AppJson(req): AppJson<AskRequest>,
) -> Result<Json<AskResponse>, AppError> {
    caller.require(Scope::Read)?;
    Ok(Json(ask(&state, caller.tenant_id, req).await?))
}
