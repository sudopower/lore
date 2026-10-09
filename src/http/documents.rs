//! Notes and documents: `POST /v1/documents`, `GET /v1/documents/{id}` and
//! `DELETE /v1/documents/{source}/{*external_id}`.
//!
//! Everything a request can get wrong is checked in `parse` before the first write, including the
//! chunk count, so a bad document leaves the whole request unwritten. The text is stored as sent
//! (`documents.body`) next to its chunks, because the chunks are cut and overlap.

use axum::Json;
use axum::body::Body;
use axum::extract::{Path, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::auth::{Caller, Scope};
use crate::chunk::{Chunk, chunk_markdown_split};
use crate::error::AppError;
use crate::http::AppState;
use crate::http::repos::is_length_limit;
use crate::mcp::MAX_DOCUMENT_CHARS;
use crate::store::{self, NewDocument};

const MAX_DOCUMENTS: usize = 100;
const MAX_TEXT_BYTES: usize = 1024 * 1024;
const MAX_EXTERNAL_ID_BYTES: usize = 512;
const MAX_TITLE_BYTES: usize = 1024;
/// A search hit repeats the uri, so it is capped like the title.
const MAX_URI_BYTES: usize = 2048;
/// Length of the serialized metadata object.
const MAX_METADATA_BYTES: usize = 64 * 1024;
/// Chunks per document. Each one is embedded, so this bounds the work a 1 MiB text can cause.
const MAX_CHUNKS: usize = 1000;

/// Room for 100 documents of 1 MiB each plus JSON escaping and the other fields.
pub const BODY_LIMIT: usize = 128 * 1024 * 1024;

/// The body limit layer answers a too large Content-Length with a plain text 413 of its own; this
/// gives it the same JSON error as a body that is too large while streaming.
pub async fn json_payload_too_large(res: Response) -> Response {
    let is_json = res
        .headers()
        .get(header::CONTENT_TYPE)
        .is_some_and(|v| v.as_bytes().starts_with(b"application/json"));
    if res.status() == StatusCode::PAYLOAD_TOO_LARGE && !is_json {
        return AppError::PayloadTooLarge.into_response();
    }
    res
}

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

/// A validated document, its chunks and the hash of everything that is stored for it.
struct Prepared {
    external_id: String,
    kind: String,
    title: String,
    text: String,
    uri: Option<String>,
    metadata: Value,
    hash: String,
    chunks: Vec<Chunk>,
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

/// A `.` or `..` segment would climb out of the source when the id is used as a path in a URL.
fn has_dot_segment(external_id: &str) -> bool {
    external_id.split('/').any(|s| s == "." || s == "..")
}

fn prepare(index: usize, doc: DocumentIn) -> Result<Prepared, AppError> {
    let at = |field: &str| format!("documents[{index}].{field}");
    if doc.external_id.is_empty() || doc.external_id.len() > MAX_EXTERNAL_ID_BYTES {
        return Err(bad(format!(
            "{} must be 1 to {MAX_EXTERNAL_ID_BYTES} bytes",
            at("external_id")
        )));
    }
    if has_dot_segment(&doc.external_id) {
        return Err(bad(format!(
            "{} cannot have a . or .. segment",
            at("external_id")
        )));
    }
    if doc.title.trim().is_empty() {
        return Err(bad(format!("{} must not be empty", at("title"))));
    }
    if doc.title.len() > MAX_TITLE_BYTES {
        return Err(bad(format!(
            "{} is over {MAX_TITLE_BYTES} bytes",
            at("title")
        )));
    }
    if doc.uri.as_deref().is_some_and(|u| u.len() > MAX_URI_BYTES) {
        return Err(bad(format!("{} is over {MAX_URI_BYTES} bytes", at("uri"))));
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
    if metadata.to_string().len() > MAX_METADATA_BYTES {
        return Err(bad(format!(
            "{} is over {} KiB",
            at("metadata"),
            MAX_METADATA_BYTES / 1024
        )));
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
    let chunks = chunk_markdown_split(&doc.text, &doc.title, MAX_CHUNKS).ok_or_else(|| {
        bad(format!(
            "{} makes more than {MAX_CHUNKS} chunks",
            at("text")
        ))
    })?;
    let hash = content_hash(&kind, &doc.title, &doc.text, doc.uri.as_deref(), &metadata);
    Ok(Prepared {
        external_id: doc.external_id,
        kind,
        title: doc.title,
        text: doc.text,
        uri: doc.uri,
        metadata,
        hash,
        chunks,
    })
}

/// Parses, checks and chunks the whole body before anything is written, so a bad document leaves
/// the others of the request unwritten too.
fn parse(bytes: &[u8]) -> Result<(String, Vec<Prepared>), AppError> {
    let req: PutRequest = serde_json::from_slice(bytes).map_err(|e| bad(e.to_string()))?;
    if !crate::http::repos::valid_repo_name(&req.source) || req.source == "." || req.source == ".."
    {
        return Err(bad(
            "source must be 1 to 100 characters of A-Z, a-z, 0-9, . _ and -, and not . or ..",
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
    // Parsing, hashing and chunking up to 100 MiB is CPU work, so it stays off the async workers.
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
        let new_doc = NewDocument {
            tenant_id,
            source_id: source.id,
            external_id: doc.external_id,
            kind: doc.kind,
            title: doc.title.clone(),
            uri: doc.uri,
            metadata: doc.metadata,
            content_hash: doc.hash,
            body: Some(doc.text),
        };
        report.chunks +=
            store::index_text(&state, tenant_id, &source, new_doc, doc.chunks, &doc.title).await?;
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

    fn message(r: Result<Prepared, AppError>) -> String {
        match r {
            Err(AppError::BadRequest(m)) => m,
            Err(e) => panic!("not a 400: {e:?}"),
            Ok(_) => panic!("accepted"),
        }
    }

    #[test]
    fn title_and_metadata_have_size_limits() {
        let mut d = doc("a");
        d.title = "t".repeat(MAX_TITLE_BYTES);
        assert!(prepare(0, d).is_ok());
        let mut d = doc("a");
        d.title = "t".repeat(MAX_TITLE_BYTES + 1);
        assert!(message(prepare(0, d)).contains("title"));
        // Bytes, not characters.
        let mut d = doc("a");
        d.title = "\u{e9}".repeat(MAX_TITLE_BYTES / 2 + 1);
        assert!(prepare(0, d).is_err());

        // `{"k":"..."}` is 8 bytes of framing around the value.
        let mut d = doc("a");
        d.metadata = Some(json!({"k": "m".repeat(MAX_METADATA_BYTES - 8)}));
        assert!(prepare(0, d).is_ok());
        let mut d = doc("a");
        d.metadata = Some(json!({"k": "m".repeat(MAX_METADATA_BYTES - 7)}));
        assert!(message(prepare(0, d)).contains("metadata"));
    }

    #[test]
    fn uri_has_a_size_limit_in_bytes() {
        let mut d = doc("a");
        d.uri = Some("u".repeat(MAX_URI_BYTES));
        assert!(prepare(0, d).is_ok());
        let mut d = doc("a");
        d.uri = Some("u".repeat(MAX_URI_BYTES + 1));
        let m = message(prepare(0, d));
        assert!(m.contains("documents[0].uri") && m.contains("2048"), "{m}");
        // Bytes, not characters.
        let mut d = doc("a");
        d.uri = Some("\u{e9}".repeat(MAX_URI_BYTES / 2 + 1));
        assert!(message(prepare(0, d)).contains("uri"));
        // No uri, and an empty one, are fine.
        let mut d = doc("a");
        d.uri = Some(String::new());
        assert!(prepare(0, d).is_ok());
    }

    #[test]
    fn dot_segments_are_rejected_in_ids() {
        for id in [".", "..", "a/../b", "a/./b", "../a", "a/..", "./a", "a/."] {
            assert!(message(prepare(0, doc(id))).contains("segment"), "{id}");
        }
        for id in [
            "a.b", "..a", ".hidden", "a/.b", "a..b", "a/b.", "...", "a//b",
        ] {
            assert!(prepare(0, doc(id)).is_ok(), "{id}");
        }
    }

    #[test]
    fn a_document_over_the_chunk_limit_is_refused_with_the_limit_named() {
        let mut d = doc("a");
        d.text = "#\n".repeat(MAX_CHUNKS + 1);
        let m = message(prepare(0, d));
        assert!(m.contains("1000") && m.contains("chunks"), "{m}");
        let mut d = doc("a");
        d.text = "#\n".repeat(MAX_CHUNKS);
        let ok = prepare(0, d).unwrap();
        assert_eq!(ok.chunks.len(), MAX_CHUNKS);
        // The chunks are the ones that get written, so the text is chunked once.
        let mut d = doc("a");
        d.title = "Title".to_string();
        d.text = "# H\nbody".to_string();
        let ok = prepare(0, d).unwrap();
        assert_eq!(ok.chunks.len(), 1);
        assert_eq!(ok.chunks[0].symbols, vec!["H".to_string()]);
    }

    #[test]
    fn parse_refuses_dot_sources() {
        let body = |source: &str| {
            json!({"source": source, "documents": [{"external_id": "a", "title": "t", "text": "x"}]})
                .to_string()
        };
        for source in [".", ".."] {
            assert!(parse(body(source).as_bytes()).is_err(), "{source}");
        }
        for source in ["a.b", "..a", ".hidden", "a..", "notes"] {
            assert!(parse(body(source).as_bytes()).is_ok(), "{source}");
        }
    }
}
