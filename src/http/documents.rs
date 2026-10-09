//! Notes and documents: `POST /v1/documents`, `GET /v1/documents/{id}` and
//! `DELETE /v1/documents/{source}/{*external_id}`.

use axum::Json;
use axum::body::Body;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::auth::{Caller, Scope};
use crate::chunk::chunk_markdown;
use crate::error::AppError;
use crate::http::AppState;
use crate::http::repos::is_length_limit;
use crate::mcp::MAX_DOCUMENT_CHARS;
use crate::store::{self, NewDocument};

const MAX_DOCUMENTS: usize = 100;
const MAX_TEXT_BYTES: usize = 1024 * 1024;
const MAX_EXTERNAL_ID_BYTES: usize = 512;

/// Room for 100 documents of 1 MiB each plus JSON escaping and the other fields.
pub const BODY_LIMIT: usize = 128 * 1024 * 1024;

#[derive(Deserialize)]
struct PutRequest {
    source: String,
    documents: Vec<DocumentIn>,
}

#[derive(Deserialize)]
struct DocumentIn {
    external_id: String,
    title: String,
    text: String,
    kind: Option<String>,
    uri: Option<String>,
    metadata: Option<Value>,
}

/// A validated document and the hash of everything that is stored for it.
struct Prepared {
    external_id: String,
    kind: String,
    title: String,
    text: String,
    uri: Option<String>,
    metadata: Value,
    hash: String,
}

#[derive(Serialize)]
pub struct PutReport {
    upserted: usize,
    unchanged: usize,
    chunks: usize,
}

#[derive(Serialize)]
pub struct DocumentOut {
    id: Uuid,
    kind: String,
    title: String,
    uri: Option<String>,
    metadata: Value,
    text: String,
}

fn bad(message: impl Into<String>) -> AppError {
    AppError::BadRequest(message.into())
}

/// Postgres rejects a NUL in text and jsonb, which would otherwise surface as a 500.
fn has_nul(v: &Value) -> bool {
    match v {
        Value::String(s) => s.contains('\0'),
        Value::Array(items) => items.iter().any(has_nul),
        Value::Object(map) => map.iter().any(|(k, v)| k.contains('\0') || has_nul(v)),
        _ => false,
    }
}

/// Hash of everything that is stored or indexed, so a changed title, uri, kind or metadata is
/// written even when the text is the same.
fn content_hash(
    kind: &str,
    title: &str,
    text: &str,
    uri: Option<&str>,
    metadata: &Value,
) -> String {
    let mut h = Sha256::new();
    for part in [kind, title, text, uri.unwrap_or(""), &metadata.to_string()] {
        h.update(part.as_bytes());
        h.update([0]);
    }
    // A missing uri and an empty one are different values.
    h.update([u8::from(uri.is_some())]);
    hex::encode(h.finalize())
}

fn prepare(index: usize, doc: DocumentIn) -> Result<Prepared, AppError> {
    let at = |field: &str| format!("documents[{index}].{field}");
    if doc.external_id.is_empty() || doc.external_id.len() > MAX_EXTERNAL_ID_BYTES {
        return Err(bad(format!(
            "{} must be 1 to {MAX_EXTERNAL_ID_BYTES} bytes",
            at("external_id")
        )));
    }
    if doc.title.trim().is_empty() {
        return Err(bad(format!("{} must not be empty", at("title"))));
    }
    if doc.text.len() > MAX_TEXT_BYTES {
        return Err(bad(format!("{} is over 1 MiB", at("text"))));
    }
    let kind = doc.kind.unwrap_or_else(|| "note".to_string());
    if kind != "note" && kind != "doc" {
        return Err(bad(format!("{} must be note or doc", at("kind"))));
    }
    let metadata = doc.metadata.unwrap_or_else(|| json!({}));
    if !metadata.is_object() {
        return Err(bad(format!("{} must be a JSON object", at("metadata"))));
    }
    let strings = [
        &doc.external_id,
        &doc.title,
        &doc.text,
        doc.uri.as_ref().unwrap_or(&doc.title),
    ];
    if strings.iter().any(|s| s.contains('\0')) || has_nul(&metadata) {
        return Err(bad(format!(
            "documents[{index}] cannot contain a NUL character"
        )));
    }
    let hash = content_hash(&kind, &doc.title, &doc.text, doc.uri.as_deref(), &metadata);
    Ok(Prepared {
        external_id: doc.external_id,
        kind,
        title: doc.title,
        text: doc.text,
        uri: doc.uri,
        metadata,
        hash,
    })
}

/// Parses and checks the whole body before anything is written, so a bad document leaves the
/// others of the request unwritten too.
fn parse(bytes: &[u8]) -> Result<(String, Vec<Prepared>), AppError> {
    let req: PutRequest = serde_json::from_slice(bytes).map_err(|e| bad(e.to_string()))?;
    if !crate::http::repos::valid_repo_name(&req.source) {
        return Err(bad(
            "source must be 1 to 100 characters of A-Z, a-z, 0-9, . _ and -",
        ));
    }
    if !(1..=MAX_DOCUMENTS).contains(&req.documents.len()) {
        return Err(bad(format!("send 1 to {MAX_DOCUMENTS} documents")));
    }
    let docs = req
        .documents
        .into_iter()
        .enumerate()
        .map(|(i, d)| prepare(i, d))
        .collect::<Result<Vec<_>, _>>()?;
    Ok((req.source, docs))
}

/// Documents are handled in request order, so when one external id appears twice the later copy
/// wins. A failure partway leaves the earlier documents written; sending the request again skips
/// them as unchanged.
pub async fn post_documents(
    caller: Caller,
    State(state): State<AppState>,
    body: Body,
) -> Result<Json<PutReport>, AppError> {
    caller.require(Scope::Ingest)?;
    // The body limit layer ends the stream with a "length limit exceeded" error.
    let bytes = axum::body::to_bytes(body, usize::MAX).await.map_err(|e| {
        if is_length_limit(&e) {
            AppError::PayloadTooLarge
        } else {
            bad("could not read the request body")
        }
    })?;
    // Parsing and hashing up to 100 MiB is CPU work, so it stays off the async workers.
    let (source_name, docs) = tokio::task::spawn_blocking(move || parse(&bytes))
        .await
        .map_err(|e| AppError::Internal(anyhow::Error::from(e).context("parse task failed")))??;

    let tenant_id = caller.tenant_id;
    let source =
        store::ensure_source(&state.pool, tenant_id, "documents", &source_name, None).await?;
    let mut report = PutReport {
        upserted: 0,
        unchanged: 0,
        chunks: 0,
    };
    for doc in docs {
        let known = store::document_hash(&state.pool, source.id, &doc.external_id).await?;
        if known.as_deref() == Some(doc.hash.as_str()) {
            report.unchanged += 1;
            continue;
        }
        let chunks = chunk_markdown(&doc.text, &doc.title);
        let new_doc = NewDocument {
            tenant_id,
            source_id: source.id,
            external_id: doc.external_id,
            kind: doc.kind,
            title: doc.title.clone(),
            uri: doc.uri,
            metadata: doc.metadata,
            content_hash: doc.hash,
        };
        report.chunks +=
            store::index_text(&state, tenant_id, &source, new_doc, chunks, &doc.title).await?;
        report.upserted += 1;
    }
    Ok(Json(report))
}

pub async fn get_document(
    caller: Caller,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<Json<DocumentOut>, AppError> {
    caller.require(Scope::Read)?;
    let (row, mut text) = store::document_text(&state.pool, caller.tenant_id, id)
        .await
        .ok_or(AppError::NotFound)?;
    if let Some((cut, _)) = text.char_indices().nth(MAX_DOCUMENT_CHARS) {
        text.truncate(cut);
    }
    Ok(Json(DocumentOut {
        id: row.id,
        kind: row.kind,
        title: row.title,
        uri: row.uri,
        metadata: row.metadata,
        text,
    }))
}

pub async fn delete_document(
    caller: Caller,
    State(state): State<AppState>,
    Path((source, external_id)): Path<(String, String)>,
) -> Result<StatusCode, AppError> {
    caller.require(Scope::Ingest)?;
    // A source of another tenant, or one that is not a documents source, looks like a missing one.
    let source = store::find_source(&state.pool, caller.tenant_id, "documents", &source)
        .await?
        .ok_or(AppError::NotFound)?;
    let mut tx = state.pool.begin().await?;
    let deleted = store::delete_document(&mut tx, source.id, &external_id).await?;
    tx.commit().await?;
    if deleted {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(AppError::NotFound)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn doc(external_id: &str) -> DocumentIn {
        DocumentIn {
            external_id: external_id.to_string(),
            title: "T".to_string(),
            text: "x".to_string(),
            kind: None,
            uri: None,
            metadata: None,
        }
    }

    #[test]
    fn hash_covers_every_stored_field() {
        let base = content_hash("note", "t", "x", None, &json!({}));
        assert_eq!(base, content_hash("note", "t", "x", None, &json!({})));
        assert_ne!(base, content_hash("doc", "t", "x", None, &json!({})));
        assert_ne!(base, content_hash("note", "u", "x", None, &json!({})));
        assert_ne!(base, content_hash("note", "t", "y", None, &json!({})));
        assert_ne!(base, content_hash("note", "t", "x", Some(""), &json!({})));
        assert_ne!(base, content_hash("note", "t", "x", None, &json!({"a": 1})));
    }

    #[test]
    fn prepare_applies_defaults_and_checks() {
        let ok = prepare(0, doc("a")).unwrap();
        assert_eq!(ok.kind, "note");
        assert_eq!(ok.metadata, json!({}));

        assert!(prepare(0, doc("")).is_err());
        assert!(prepare(0, doc(&"a".repeat(MAX_EXTERNAL_ID_BYTES + 1))).is_err());
        let mut d = doc("a");
        d.title = "  ".to_string();
        assert!(prepare(0, d).is_err());
        let mut d = doc("a");
        d.metadata = Some(json!([1]));
        assert!(prepare(0, d).is_err());
        let mut d = doc("a");
        d.metadata = Some(json!({"k": ["a\0b"]}));
        assert!(prepare(0, d).is_err());
        let mut d = doc("a");
        d.text = "a\0b".to_string();
        assert!(prepare(0, d).is_err());
    }
}
