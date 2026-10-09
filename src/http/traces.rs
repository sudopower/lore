//! `POST /v1/traces`: OTLP/HTTP trace ingest.

use axum::body::Body;
use axum::extract::State;
use axum::http::header::{CONTENT_ENCODING, CONTENT_TYPE};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use opentelemetry_proto::tonic::collector::trace::v1::ExportTraceServiceResponse;
use prost::Message;
use serde::Serialize;

use crate::auth::{Caller, Scope};
use crate::error::AppError;
use crate::http::AppState;
use crate::http::repos::is_length_limit;
use crate::otlp::{self, JSON, PROTOBUF};
use crate::store;

/// The JSON reply to a JSON request: `{}`.
#[derive(Serialize)]
struct Empty {}

fn header_str(headers: &HeaderMap, name: axum::http::HeaderName) -> Option<&str> {
    headers.get(name).and_then(|v| v.to_str().ok())
}

pub async fn post_traces(
    caller: Caller,
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Body,
) -> Result<Response, AppError> {
    caller.require(Scope::Ingest)?;
    let content_type = header_str(&headers, CONTENT_TYPE).unwrap_or("").to_string();
    let content_encoding = header_str(&headers, CONTENT_ENCODING).map(str::to_string);

    // The body limit layer ends the stream with a "length limit exceeded" error.
    let bytes = axum::body::to_bytes(body, usize::MAX).await.map_err(|e| {
        if is_length_limit(&e) {
            AppError::PayloadTooLarge
        } else {
            AppError::BadRequest("could not read the request body".to_string())
        }
    })?;

    // Unzipping and parsing are CPU work, so they stay off the async workers.
    let media = otlp::media_type(&content_type);
    let rows = tokio::task::spawn_blocking(move || {
        let req = otlp::decode(&content_type, content_encoding.as_deref(), &bytes)?;
        Ok::<_, AppError>(otlp::to_rows(&req))
    })
    .await
    .map_err(|e| AppError::Internal(anyhow::Error::from(e).context("decode task failed")))??;

    store::ensure_source(&state.pool, caller.tenant_id, "traces", "otlp", None).await?;
    let mut tx = state.pool.begin().await?;
    store::upsert_spans(&mut tx, caller.tenant_id, &rows).await?;
    tx.commit().await?;

    let (content_type, payload) = if media == PROTOBUF {
        (
            PROTOBUF,
            ExportTraceServiceResponse::default().encode_to_vec(),
        )
    } else {
        (
            JSON,
            serde_json::to_vec(&Empty {}).map_err(|e| AppError::Internal(e.into()))?,
        )
    };
    let mut res = (StatusCode::OK, payload).into_response();
    res.headers_mut()
        .insert(CONTENT_TYPE, HeaderValue::from_static(content_type));
    Ok(res)
}
