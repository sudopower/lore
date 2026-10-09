//! Answers with citations. Fast mode (one search, one model call) lives in [`fast`]; the prompt
//! it sends is built in [`prompt`]. The request and response types here are shared by the HTTP
//! route, the CLI, the web page and MCP.

use std::time::Instant;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::error::AppError;
use crate::http::AppState;

pub mod fast;
pub mod prompt;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    #[default]
    Fast,
    Deep,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AskRequest {
    pub question: String,
    #[serde(default)]
    pub mode: Mode,
    /// Sources to retrieve in fast mode (the search clamps it to 1..=50). Default 12.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub k: Option<usize>,
    /// Only these document kinds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<Vec<String>>,
    /// Only the source with this name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Citation {
    /// `"1"` in fast mode; `"path:10-20"` or `"session:<id>"` in deep mode.
    pub label: String,
    pub document_id: Option<Uuid>,
    pub kind: Option<String>,
    pub title: Option<String>,
    pub path: Option<String>,
    pub start_line: Option<i32>,
    pub end_line: Option<i32>,
    pub url: Option<String>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsageOut {
    pub input_tokens: u64,
    pub output_tokens: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AskResponse {
    pub answer: String,
    pub mode: Mode,
    pub model: String,
    pub citations: Vec<Citation>,
    pub sources_considered: usize,
    pub tool_calls: u32,
    pub usage: UsageOut,
    pub latency_ms: u64,
}

/// Answers `req.question` from the tenant's indexed material. A blank question is a
/// `BadRequest`, and so is deep mode until it exists.
pub async fn ask(
    state: &AppState,
    tenant_id: Uuid,
    req: AskRequest,
) -> Result<AskResponse, AppError> {
    let started = Instant::now();
    if req.question.trim().is_empty() {
        return Err(AppError::BadRequest("question is required".to_string()));
    }
    let mut res = match req.mode {
        Mode::Fast => fast::answer(state, tenant_id, &req).await?,
        Mode::Deep => {
            return Err(AppError::BadRequest(
                "deep mode is not available yet".to_string(),
            ));
        }
    };
    res.latency_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    Ok(res)
}
