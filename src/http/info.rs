//! `GET /v1/info`: what this server is running. No key needed.

use axum::Json;
use axum::extract::State;
use serde::Serialize;

use crate::http::AppState;

#[derive(Serialize)]
pub struct Info {
    version: String,
    llm: String,
    embedder: String,
    modes: Vec<&'static str>,
}

pub async fn info(State(state): State<AppState>) -> Json<Info> {
    Json(Info {
        version: state.config.version.clone(),
        llm: state.llm.name().to_string(),
        embedder: state.embedder.name().to_string(),
        modes: vec!["fast"],
    })
}
