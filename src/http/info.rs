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
    // Deep mode drives tools, so it is on only when the provider can use them.
    let modes = if state.llm.supports_tools() {
        vec!["fast", "deep"]
    } else {
        vec!["fast"]
    };
    Json(Info {
        version: state.config.version.clone(),
        llm: state.llm.name().to_string(),
        embedder: state.embedder.name().to_string(),
        modes,
    })
}
