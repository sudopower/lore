//! SQL for sources, documents and chunks: the storage every source kind (repos, traces, documents)
//! shares. Queries are runtime-checked. Every function that reaches a tenant-owned row takes the
//! tenant id, so another tenant's rows are never visible.
//!
//! Vocabulary: a "source kind" is `sources.kind` (`repo`, `traces`, `documents`); a "document kind"
//! is `documents.kind` (`code`, `doc`, `session`, `note`).

use anyhow::{Result, bail};
use pgvector::Vector;
use serde::Serialize;
use sqlx::{PgConnection, PgPool, Postgres, Transaction};
use uuid::Uuid;

use crate::chunk::{Chunk, embed_text, lexical_text};
use crate::embed::embed_documents_async;
use crate::error::AppError;
use crate::http::AppState;

const SOURCE_KINDS: [&str; 3] = ["repo", "traces", "documents"];

#[derive(Debug, Clone, PartialEq, Eq, Serialize, sqlx::FromRow)]
pub struct Source {
    pub id: Uuid,
    pub tenant_id: Uuid,
    pub kind: String,
    pub name: String,
    pub web_url_template: Option<String>,
    pub indexed_commit: Option<String>,
}

#[derive(Debug, Clone)]
pub struct NewDocument {
    pub tenant_id: Uuid,
    pub source_id: Uuid,
    pub external_id: String,
    /// Document kind: `code`, `doc`, `session` or `note`.
    pub kind: String,
    pub title: String,
    pub uri: Option<String>,
    pub metadata: serde_json::Value,
    pub content_hash: String,
}

/// A chunk with the texts that were derived from it: `lexical` feeds the full-text index and
/// `embedding` is the vector of `embed_text(title, chunk)`.
#[derive(Debug, Clone)]
pub struct IndexedChunk {
    pub chunk: Chunk,
    pub lexical: String,
    pub embedding: Vec<f32>,
}

const SOURCE_COLUMNS: &str = "id, tenant_id, kind, name, web_url_template, indexed_commit";

/// Returns the source `(tenant, kind, name)`, creating it when missing. A given template replaces
/// the stored one; `None` keeps it. The template is rendered as a link by the web page, so it must
/// be an `http://` or `https://` URL.
pub async fn ensure_source(
    pool: &PgPool,
    tenant_id: Uuid,
    kind: &str,
    name: &str,
    web_url_template: Option<&str>,
) -> Result<Source, AppError> {
    if !SOURCE_KINDS.contains(&kind) {
        return Err(AppError::BadRequest(
            "source kind must be repo, traces or documents".to_string(),
        ));
    }
    if let Some(t) = web_url_template
        && !(t.starts_with("http://") || t.starts_with("https://"))
    {
        return Err(AppError::BadRequest(
            "web_url_template must start with http:// or https://".to_string(),
        ));
    }
    let source = sqlx::query_as(&format!(
        "INSERT INTO sources (id, tenant_id, kind, name, web_url_template)
         VALUES ($1, $2, $3, $4, $5)
         ON CONFLICT (tenant_id, kind, name) DO UPDATE SET
           web_url_template = COALESCE(EXCLUDED.web_url_template, sources.web_url_template),
           updated_at = CASE WHEN EXCLUDED.web_url_template IS NULL
                             THEN sources.updated_at ELSE now() END
         RETURNING {SOURCE_COLUMNS}"
    ))
    .bind(Uuid::new_v4())
    .bind(tenant_id)
    .bind(kind)
    .bind(name)
    .bind(web_url_template)
    .fetch_one(pool)
    .await?;
    Ok(source)
}

pub async fn get_source(pool: &PgPool, tenant_id: Uuid, id: Uuid) -> sqlx::Result<Option<Source>> {
    sqlx::query_as(&format!(
        "SELECT {SOURCE_COLUMNS} FROM sources WHERE id = $1 AND tenant_id = $2"
    ))
    .bind(id)
    .bind(tenant_id)
    .fetch_optional(pool)
    .await
}

/// The tenant's sources, optionally only those of one source kind.
pub async fn list_sources(
    pool: &PgPool,
    tenant_id: Uuid,
    kind: Option<&str>,
) -> sqlx::Result<Vec<Source>> {
    sqlx::query_as(&format!(
        "SELECT {SOURCE_COLUMNS} FROM sources
         WHERE tenant_id = $1 AND ($2::text IS NULL OR kind = $2)
         ORDER BY kind, name"
    ))
    .bind(tenant_id)
    .bind(kind)
    .fetch_all(pool)
    .await
}

/// Records the commit a repo source has been indexed up to. `source_id` comes from a `Source` the
/// caller already fetched for its tenant.
pub async fn set_indexed_commit(pool: &PgPool, source_id: Uuid, sha: &str) -> sqlx::Result<()> {
    sqlx::query("UPDATE sources SET indexed_commit = $2, updated_at = now() WHERE id = $1")
        .bind(source_id)
        .bind(sha)
        .execute(pool)
        .await?;
    Ok(())
}

/// Inserts the document, or updates the existing `(source, external id)` row, and returns its id.
/// The source must belong to `doc.tenant_id`, otherwise `NotFound`.
pub async fn upsert_document(
    tx: &mut Transaction<'_, Postgres>,
    doc: &NewDocument,
) -> Result<Uuid, AppError> {
    let id: Option<Uuid> = sqlx::query_scalar(
        "INSERT INTO documents
           (id, tenant_id, source_id, external_id, kind, title, uri, metadata, content_hash)
         SELECT $1, s.tenant_id, s.id, $4, $5, $6, $7, $8, $9
         FROM sources s WHERE s.id = $3 AND s.tenant_id = $2
         ON CONFLICT (source_id, external_id) DO UPDATE SET
           kind = EXCLUDED.kind,
           title = EXCLUDED.title,
           uri = EXCLUDED.uri,
           metadata = EXCLUDED.metadata,
           content_hash = EXCLUDED.content_hash,
           updated_at = now()
         RETURNING id",
    )
    .bind(Uuid::new_v4())
    .bind(doc.tenant_id)
    .bind(doc.source_id)
    .bind(&doc.external_id)
    .bind(&doc.kind)
    .bind(&doc.title)
    .bind(&doc.uri)
    .bind(&doc.metadata)
    .bind(&doc.content_hash)
    .fetch_optional(&mut **tx)
    .await?;
    id.ok_or(AppError::NotFound)
}

/// The stored content hash of a document, for skipping files that did not change.
pub async fn document_hash(
    pool: &PgPool,
    source_id: Uuid,
    external_id: &str,
) -> sqlx::Result<Option<String>> {
    sqlx::query_scalar(
        "SELECT content_hash FROM documents WHERE source_id = $1 AND external_id = $2",
    )
    .bind(source_id)
    .bind(external_id)
    .fetch_optional(pool)
    .await
}

/// The external ids of every document of a source.
pub async fn list_external_ids(pool: &PgPool, source_id: Uuid) -> sqlx::Result<Vec<String>> {
    sqlx::query_scalar("SELECT external_id FROM documents WHERE source_id = $1")
        .bind(source_id)
        .fetch_all(pool)
        .await
}

/// Replaces all chunks of a document: deletes the old ones, then inserts `chunks` with `ord` set
/// to the index. Symbols weigh more (A) than the body (B) in the full-text vector. The document
/// must belong to `tenant_id`, otherwise `NotFound`.
pub async fn replace_chunks(
    tx: &mut Transaction<'_, Postgres>,
    tenant_id: Uuid,
    document_id: Uuid,
    chunks: &[IndexedChunk],
) -> Result<(), AppError> {
    let owned: Option<Uuid> =
        sqlx::query_scalar("SELECT id FROM documents WHERE id = $1 AND tenant_id = $2")
            .bind(document_id)
            .bind(tenant_id)
            .fetch_optional(&mut **tx)
            .await?;
    if owned.is_none() {
        return Err(AppError::NotFound);
    }
    sqlx::query("DELETE FROM chunks WHERE document_id = $1 AND tenant_id = $2")
        .bind(document_id)
        .bind(tenant_id)
        .execute(&mut **tx)
        .await?;
    for (ord, c) in chunks.iter().enumerate() {
        insert_chunk(tx, tenant_id, document_id, ord, c).await?;
    }
    Ok(())
}

async fn insert_chunk(
    conn: &mut PgConnection,
    tenant_id: Uuid,
    document_id: Uuid,
    ord: usize,
    c: &IndexedChunk,
) -> sqlx::Result<()> {
    sqlx::query(
        "INSERT INTO chunks
           (tenant_id, document_id, ord, text, start_line, end_line, symbols, tsv, embedding)
         VALUES ($1, $2, $3, $4, $5, $6, $7,
                 setweight(to_tsvector('english', $8::text), 'A')
                   || setweight(to_tsvector('english', $9::text), 'B'),
                 $10)",
    )
    .bind(tenant_id)
    .bind(document_id)
    .bind(i32::try_from(ord).unwrap_or(i32::MAX))
    .bind(&c.chunk.text)
    .bind(i32::try_from(c.chunk.start_line).unwrap_or(i32::MAX))
    .bind(i32::try_from(c.chunk.end_line).unwrap_or(i32::MAX))
    .bind(&c.chunk.symbols)
    .bind(c.chunk.symbols.join(" "))
    .bind(&c.lexical)
    .bind(Vector::from(c.embedding.clone()))
    .execute(conn)
    .await?;
    Ok(())
}

/// Deletes a document; its chunks go with it (cascade). Returns whether a document was deleted.
pub async fn delete_document(
    tx: &mut Transaction<'_, Postgres>,
    source_id: Uuid,
    external_id: &str,
) -> sqlx::Result<bool> {
    let res = sqlx::query("DELETE FROM documents WHERE source_id = $1 AND external_id = $2")
        .bind(source_id)
        .bind(external_id)
        .execute(&mut **tx)
        .await?;
    Ok(res.rows_affected() > 0)
}

/// The shared indexing path (repos, sessions, documents): embeds the chunks, then stores the
/// document and its chunks in one short transaction. Returns the chunk count.
///
/// The embedding runs before the transaction opens, so no connection is held while the model
/// works. `title_for_embed` is the path or title passed to `embed_text`.
pub async fn index_text(
    state: &AppState,
    tenant_id: Uuid,
    source: &Source,
    doc: NewDocument,
    chunks: Vec<Chunk>,
    title_for_embed: &str,
) -> Result<usize> {
    if source.tenant_id != tenant_id || doc.tenant_id != tenant_id || doc.source_id != source.id {
        bail!("document does not belong to the source and tenant");
    }
    let lexicals: Vec<String> = chunks.iter().map(lexical_text).collect();
    let texts: Vec<String> = chunks
        .iter()
        .map(|c| embed_text(title_for_embed, c))
        .collect();
    let embeddings = if texts.is_empty() {
        Vec::new()
    } else {
        embed_documents_async(state.embedder.clone(), texts).await?
    };
    if embeddings.len() != chunks.len() {
        bail!(
            "embedder returned {} vectors for {} chunks",
            embeddings.len(),
            chunks.len()
        );
    }
    let indexed: Vec<IndexedChunk> = chunks
        .into_iter()
        .zip(lexicals)
        .zip(embeddings)
        .map(|((chunk, lexical), embedding)| IndexedChunk {
            chunk,
            lexical,
            embedding,
        })
        .collect();

    let mut tx = state.pool.begin().await?;
    let document_id = upsert_document(&mut tx, &doc).await?;
    replace_chunks(&mut tx, tenant_id, document_id, &indexed).await?;
    tx.commit().await?;
    Ok(indexed.len())
}
