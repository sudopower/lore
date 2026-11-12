//! Hybrid search: Postgres full-text ranking (good at exact names) fused with pgvector similarity
//! (good at meaning) by Reciprocal Rank Fusion. A third list lifts the first few enumerations among
//! the vector hits (enums, constant lists, seed rows): they answer "which X are there" but are
//! easily outranked by the many chunks that only use X. A fourth list holds the chunks that define
//! a symbol the query names, then their direct callers and callees ([`callgraph::related`]): "how
//! does X reach Y" is answered by code that only mentions X by name. The HTTP route, the CLI, fast
//! answers and the MCP tools all call [`search`].
//!
//! Queries are runtime-checked and bind every value. The one built string is the tsquery text of
//! the lexical branch, which is itself passed as a bind value.

use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;
use std::sync::Arc;

use pgvector::Vector;
use sqlx::{PgConnection, PgPool};
use uuid::Uuid;

use crate::callgraph;
use crate::embed::{Embedder, embed_query_async};
use crate::error::AppError;
use crate::repo::is_full_sha;

/// Hits returned when the caller does not say.
pub const DEFAULT_K: usize = 10;
/// The most hits one search returns.
pub const MAX_K: usize = 50;
/// The longest accepted query, in characters.
pub const MAX_QUERY_CHARS: usize = 2000;
/// Document kinds (`documents.kind`), as the migration lists them.
/// A hit carries at most this many bytes of `metadata.path`. The documents API refuses a longer
/// path, and a document stored before that limit has its path cut here.
pub const MAX_PATH_BYTES: usize = 2048;

pub const DOCUMENT_KINDS: [&str; 4] = ["code", "doc", "session", "note"];

/// A snippet is the first lines of a chunk.
const SNIPPET_LINES: usize = 40;

/// Query words too common to point at the lines of a hit that matter.
pub(crate) const STOP_WORDS: [&str; 40] = [
    "the", "and", "for", "are", "what", "which", "how", "does", "with", "from", "this", "that",
    "can", "there", "their", "when", "where", "who", "why", "into", "its", "our", "use", "used",
    "any", "all", "each", "has", "have", "was", "were", "will", "would", "should", "could",
    "about", "many", "much", "some", "get",
];

/// The lowercased words of `query` that pick the lines of a hit worth showing (the MCP search
/// excerpt, the line in a sources answer): 3 characters or more, not a stop word, each once.
pub fn query_terms(query: &str) -> Vec<String> {
    let mut terms: Vec<String> = Vec::new();
    for word in query.split(|c: char| !(c.is_alphanumeric() || c == '_')) {
        let word = word.to_lowercase();
        if word.chars().count() >= 3
            && !STOP_WORDS.contains(&word.as_str())
            && !terms.contains(&word)
        {
            terms.push(word);
        }
    }
    terms
}

/// Rows taken from each branch before fusion.
const BRANCH_LIMIT: i64 = 50;
/// How many of the vector hits that are enumerations get a place in the enumeration list.
const ENUMERATION_LIMIT: usize = 3;
/// The `k` constant of Reciprocal Rank Fusion.
const RRF_K: f64 = 60.0;

#[derive(Debug, Clone)]
pub struct SearchQuery {
    pub tenant_id: Uuid,
    pub q: String,
    /// Clamped to 1..=50.
    pub k: usize,
    /// Document kinds to keep (`code`, `doc`, `session`, `note`); `None` keeps all.
    pub kinds: Option<Vec<String>>,
    /// A source name; `None` searches every source of the tenant.
    pub source: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Hit {
    pub chunk_id: i64,
    pub document_id: Uuid,
    /// The source name.
    pub source: String,
    /// Other eligible sources with exactly the same chunk text.
    pub also_in: Vec<String>,
    /// The document kind.
    pub kind: String,
    pub title: String,
    /// A rendered web link for repo files whose source has a template, else `documents.uri`.
    pub uri: Option<String>,
    /// `documents.metadata->>'path'` cut to `MAX_PATH_BYTES`: repo files and documents that set it.
    pub path: Option<String>,
    /// The session id for `session` hits, `None` for every other kind.
    pub external_id: Option<String>,
    pub start_line: Option<i32>,
    pub end_line: Option<i32>,
    pub symbols: Vec<String>,
    pub text: String,
    /// The Reciprocal Rank Fusion score: higher is better.
    pub score: f64,
    /// A later SQL migration names the same object. Internal answer annotation, not a wire field.
    pub later_changes: Vec<String>,
}

impl Hit {
    pub fn sql_change_note(&self) -> Option<String> {
        if self.later_changes.is_empty() {
            return None;
        }
        let paths = self
            .later_changes
            .iter()
            .map(|path| format!("`{}`", path.replace(['`', '\n', '\r'], "'")))
            .collect::<Vec<_>>()
            .join(", ");
        Some(format!("later SQL changes in {paths}"))
    }
}

/// The columns of a hit, plus what `uri` is rendered from.
#[derive(sqlx::FromRow)]
struct HitRow {
    id: i64,
    document_id: Uuid,
    source: String,
    kind: String,
    title: String,
    doc_uri: Option<String>,
    path: Option<String>,
    external_id: Option<String>,
    commit: Option<String>,
    template: Option<String>,
    start_line: Option<i32>,
    end_line: Option<i32>,
    symbols: Vec<String>,
    text: String,
    is_test: bool,
    is_generated: bool,
    /// Full, uncut path only for chunks with stored SQL subjects. Never a wire field.
    sql_path: Option<String>,
}

impl HitRow {
    fn into_hit(self, score: f64) -> Hit {
        let path = self.path.map(|p| cut_bytes(p, MAX_PATH_BYTES));
        let rendered = match (
            self.template.as_deref(),
            path.as_deref(),
            self.commit.as_deref(),
            self.start_line,
            self.end_line,
        ) {
            (Some(template), Some(path), Some(commit), Some(start), Some(end))
                if is_full_sha(commit) =>
            {
                Some(render_web_url(template, path, commit, start, end))
            }
            _ => None,
        };
        Hit {
            chunk_id: self.id,
            document_id: self.document_id,
            source: self.source,
            also_in: Vec::new(),
            kind: self.kind,
            title: self.title,
            uri: rendered.or(self.doc_uri),
            path,
            external_id: self.external_id,
            start_line: self.start_line,
            end_line: self.end_line,
            symbols: self.symbols,
            text: self.text,
            score,
            later_changes: Vec::new(),
        }
    }
}

/// `s` cut to at most `max` bytes, on a char boundary.
fn cut_bytes(mut s: String, max: usize) -> String {
    if s.len() > max {
        let mut end = max;
        while !s.is_char_boundary(end) {
            end -= 1;
        }
        s.truncate(end);
    }
    s
}

/// The first 40 lines of `text`, joined by `\n`, for the HTTP route. The MCP `search` tool shows
/// an excerpt instead (`mcp::excerpt`).
pub fn snippet(text: &str) -> String {
    text.lines()
        .take(SNIPPET_LINES)
        .collect::<Vec<_>>()
        .join("\n")
}

/// Hybrid search over the tenant's chunks. Returns at most `k` hits, best first. Another
/// tenant's chunks are never visible. A blank or oversized `q` and an unknown kind are a
/// `BadRequest`.
pub async fn search(
    pool: &PgPool,
    embedder: Arc<dyn Embedder>,
    query: SearchQuery,
) -> Result<Vec<Hit>, AppError> {
    let q = query.q.trim();
    if q.is_empty() {
        return Err(AppError::BadRequest("q must not be empty".to_string()));
    }
    if q.chars().count() > MAX_QUERY_CHARS {
        return Err(AppError::BadRequest(format!(
            "q must be at most {MAX_QUERY_CHARS} characters"
        )));
    }
    // Postgres text cannot hold a NUL.
    if q.contains('\0') || query.source.as_deref().is_some_and(|s| s.contains('\0')) {
        return Err(AppError::BadRequest(
            "q and source must not contain NUL".to_string(),
        ));
    }
    let k = clamp_k(query.k);
    let kinds = validate_kinds(query.kinds)?;
    let filter = Filter {
        tenant_id: query.tenant_id,
        kinds: kinds.as_deref(),
        source: query.source.as_deref(),
    };

    let scope = callgraph::Scope {
        tenant_id: query.tenant_id,
        kinds: kinds.as_deref(),
        source: query.source.as_deref(),
    };
    // Embed before acquiring the one connection shared by every ranking branch.
    let embedding = embed_query_async(embedder, q.to_string()).await?;
    // A push can commit between any two queries, including ranking and hydration. One MVCC
    // snapshot keeps every branch on the same version, even when its old chunks are deleted.
    let mut tx = pool.begin().await?;
    sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ, READ ONLY")
        .execute(&mut *tx)
        .await?;
    let lexical = lexical_branch(&mut tx, q, &filter).await?;
    let vector_hits = vector_branch(&mut tx, embedding, &filter).await?;
    let related = callgraph::related_on(&mut tx, &scope, q).await?;
    let vector: Vec<i64> = vector_hits.iter().map(|(id, _)| *id).collect();
    // The enumerations the vector branch found, in its order: an enumeration far from the query
    // in meaning is not lifted, however few enumerations there are.
    let enumerations: Vec<i64> = vector_hits
        .iter()
        .filter(|(_, enumerates)| *enumerates)
        .map(|(id, _)| *id)
        .take(ENUMERATION_LIMIT)
        .collect();

    let fused = fuse(&[&lexical, &vector, &enumerations, &related]);
    if fused.is_empty() {
        tx.commit().await?;
        return Ok(Vec::new());
    }

    let ids: Vec<i64> = fused.iter().map(|(id, _)| *id).collect();
    let mut by_id =
        hit_rows_by_id(&mut *tx, query.tenant_id, &ids, filter.kinds, filter.source).await?;
    let mut sql_positions: HashMap<i64, String> = by_id
        .iter()
        .filter_map(|(&id, row)| row.sql_path.clone().map(|path| (id, path)))
        .collect();
    let intent = RankingIntent::new(q);
    let mut hits: Vec<Hit> = fused
        .into_iter()
        .filter_map(|(id, score)| {
            by_id.remove(&id).map(|row| {
                let weight = intent.weight(&row);
                row.into_hit(score * weight)
            })
        })
        .collect();
    hits.sort_by(|a, b| b.score.total_cmp(&a.score));
    let (hits, origins) = collapse_duplicates(hits, k, &sql_positions);
    if !hits
        .iter()
        .any(|hit| sql_positions.contains_key(&hit.chunk_id))
    {
        tx.commit().await?;
        return Ok(hits);
    }
    let fused = hits.iter().map(|h| (h.chunk_id, h.score)).collect();
    // Expand every authorized original behind a representative, not another source merely
    // sharing an object name. Keep lookup and companion hydration on the same MVCC snapshot.
    let (expanded, later_changes) = sql_changes(&mut tx, fused, &origins, &filter).await?;
    let mut by_id: HashMap<i64, Hit> = hits.into_iter().map(|h| (h.chunk_id, h)).collect();
    let missing: Vec<i64> = expanded
        .iter()
        .filter_map(|(id, _)| (!by_id.contains_key(id)).then_some(*id))
        .collect();
    if !missing.is_empty() {
        let rows = hit_rows_by_id(
            &mut *tx,
            query.tenant_id,
            &missing,
            filter.kinds,
            filter.source,
        )
        .await?;
        for (id, row) in rows {
            if let Some(path) = &row.sql_path {
                sql_positions.insert(id, path.clone());
            }
            by_id.insert(id, row.into_hit(0.0));
        }
    }
    tx.commit().await?;
    let hits = expanded
        .into_iter()
        .filter_map(|(id, score)| {
            by_id.remove(&id).map(|hit| Hit {
                score,
                later_changes: later_changes.get(&id).cloned().unwrap_or_default(),
                ..hit
            })
        })
        .collect();
    // Apply k after companion collapse too, so duplicate companions cannot consume its slots.
    Ok(collapse_duplicates(hits, k, &sql_positions).0)
}

/// The tenant's chunks with these ids as hits with score 0, by id. One query for all rows; ids of
/// other tenants and of deleted chunks are missing from the map.
pub(crate) async fn hits_by_id<'e, E>(
    executor: E,
    tenant_id: Uuid,
    ids: &[i64],
) -> Result<HashMap<i64, Hit>, AppError>
where
    E: sqlx::Executor<'e, Database = sqlx::Postgres>,
{
    Ok(hit_rows_by_id(executor, tenant_id, ids, None, None)
        .await?
        .into_iter()
        .map(|(id, row)| (id, row.into_hit(0.0)))
        .collect())
}

async fn hit_rows_by_id<'e, E>(
    executor: E,
    tenant_id: Uuid,
    ids: &[i64],
    kinds: Option<&[String]>,
    source: Option<&str>,
) -> Result<HashMap<i64, HitRow>, AppError>
where
    E: sqlx::Executor<'e, Database = sqlx::Postgres>,
{
    let rows: Vec<HitRow> = sqlx::query_as(
        "SELECT c.id, c.document_id, s.name AS source, d.kind, d.title, d.uri AS doc_uri,
                left(d.metadata->>'path', 2048) AS path,
                CASE WHEN d.kind = 'session' THEN d.external_id END AS external_id,
                d.metadata->>'commit' AS commit,
                s.web_url_template AS template, c.start_line, c.end_line, c.symbols, c.text,
                c.is_test, c.is_generated,
                CASE WHEN cardinality(c.sql_subjects) > 0
                     THEN d.metadata->>'path' END AS sql_path
         FROM chunks c
         JOIN documents d ON d.id = c.document_id
         JOIN sources s ON s.id = d.source_id
         WHERE c.id = ANY($1) AND c.tenant_id = $2
           AND ($3::text[] IS NULL OR d.kind = ANY($3))
           AND ($4::text IS NULL OR s.name = $4)",
    )
    .bind(ids)
    .bind(tenant_id)
    .bind(kinds)
    .bind(source)
    .fetch_all(executor)
    .await?;
    Ok(rows.into_iter().map(|r| (r.id, r)).collect())
}

/// Small priors leave all chunks retrievable. Explicit requests for tests/generated output
/// remove the corresponding penalty rather than hiding those files behind production code.
struct RankingIntent {
    tests: bool,
    generated: bool,
    behavioral: bool,
}

impl RankingIntent {
    fn new(q: &str) -> Self {
        let terms: Vec<String> = q
            .split(|c: char| !c.is_alphanumeric())
            .map(str::to_lowercase)
            .collect();
        Self {
            tests: terms.iter().any(|s| {
                matches!(
                    s.as_str(),
                    "test"
                        | "tests"
                        | "testing"
                        | "fixture"
                        | "fixtures"
                        | "mock"
                        | "mocks"
                        | "snapshot"
                        | "snapshots"
                        | "e2e"
                )
            }),
            generated: terms
                .iter()
                .any(|s| matches!(s.as_str(), "generated" | "protobuf" | "codegen")),
            behavioral: !terms
                .iter()
                .any(|s| matches!(s.as_str(), "readme" | "documentation" | "docs"))
                && terms.iter().any(|s| {
                    matches!(
                        s.as_str(),
                        "limits"
                            | "limit"
                            | "enforces"
                            | "enforce"
                            | "rejects"
                            | "reject"
                            | "happens"
                            | "does"
                    )
                }),
        }
    }

    fn weight(&self, row: &HitRow) -> f64 {
        let mut weight = 1.0;
        if row.is_test && !self.tests {
            weight *= 0.7;
        }
        if row.is_generated && !self.generated {
            weight *= 0.7;
        }
        let readme = row.path.as_deref().is_some_and(|path| {
            path.rsplit('/').next().is_some_and(|name| {
                name.eq_ignore_ascii_case("readme.md") || name.eq_ignore_ascii_case("readme.mdx")
            })
        });
        if self.behavioral && readme {
            weight *= 0.9;
        }
        weight
    }
}

/// Collapse identical text shared across sources. Repeats confined to one source retain
/// separate locations. SQL also requires the same full migration path: identical text at
/// different chronological positions is different evidence. Origins stay private and bounded
/// by the retrieved candidate pool. Compare full text, never snippets or a hash alone, before k.
fn collapse_duplicates(
    hits: Vec<Hit>,
    k: usize,
    sql_positions: &HashMap<i64, String>,
) -> (Vec<Hit>, HashMap<i64, Vec<i64>>) {
    let mut first_source: HashMap<(&str, Option<&str>), &str> = HashMap::new();
    let mut shared: HashSet<(String, Option<String>)> = HashSet::new();
    for hit in &hits {
        let path = sql_positions.get(&hit.chunk_id);
        if let Some(source) =
            first_source.insert((&hit.text, path.map(String::as_str)), &hit.source)
            && source != hit.source
        {
            shared.insert((hit.text.clone(), path.cloned()));
        }
    }
    let mut groups: HashMap<(String, Option<String>), usize> = HashMap::new();
    let mut origins: HashMap<i64, Vec<i64>> = HashMap::new();
    let mut out: Vec<Hit> = Vec::new();
    for hit in hits {
        let key = (hit.text.clone(), sql_positions.get(&hit.chunk_id).cloned());
        if shared.contains(&key) {
            if let Some(&i) = groups.get(&key) {
                origins
                    .get_mut(&out[i].chunk_id)
                    .unwrap()
                    .push(hit.chunk_id);
                for source in std::iter::once(hit.source).chain(hit.also_in) {
                    if out[i].source != source && !out[i].also_in.contains(&source) {
                        out[i].also_in.push(source);
                    }
                }
                for path in hit.later_changes {
                    if !out[i].later_changes.contains(&path) {
                        out[i].later_changes.push(path);
                    }
                }
                continue;
            }
            groups.insert(key, out.len());
        }
        origins.insert(hit.chunk_id, vec![hit.chunk_id]);
        out.push(hit);
    }
    for hit in &mut out {
        hit.also_in.sort();
    }
    out.truncate(k);
    let retained: HashSet<i64> = out.iter().map(|h| h.chunk_id).collect();
    origins.retain(|id, _| retained.contains(id));
    (out, origins)
}

#[derive(Clone, sqlx::FromRow)]
struct SqlChange {
    origin: i64,
    id: i64,
    path: String,
}

/// Add the newest migration for each retrieved SQL subject, before the older evidence. The
/// source id, tenant and kind remain fixed. Paths use bytewise ordering, not locale, timestamps,
/// natural-number ordering or ingestion order. Each subject contributes at most one companion.
async fn sql_changes(
    conn: &mut PgConnection,
    fused: Vec<(i64, f64)>,
    origins: &HashMap<i64, Vec<i64>>,
    f: &Filter<'_>,
) -> Result<(Vec<(i64, f64)>, HashMap<i64, Vec<String>>), AppError> {
    let ids: Vec<i64> = origins.values().flatten().copied().collect();
    let mut changes: Vec<SqlChange> = sqlx::query_as(
        "SELECT base.id AS origin, latest.id, s.name || ':' || latest.path AS path
         FROM chunks base
         JOIN documents original ON original.id = base.document_id
         JOIN sources s ON s.id = original.source_id
         JOIN sql_subject_lookup original_subject ON original_subject.chunk_id = base.id
         CROSS JOIN LATERAL (
             SELECT candidate.id, candidate.path FROM (
                 (SELECT entry.chunk_id AS id, entry.path, entry.ord
                  FROM sql_subject_lookup entry
                  WHERE entry.tenant_id = $2
                    AND entry.source_id = original.source_id AND entry.kind = original.kind
                    AND entry.identity_hash = original_subject.identity_hash
                    AND entry.identity = original_subject.identity
                    AND octet_length(entry.path) <= 2048
                    AND entry.path > (original.metadata->>'path') COLLATE \"C\"
                  ORDER BY entry.path DESC, entry.ord DESC, entry.chunk_id DESC
                  LIMIT 1)
                 UNION ALL
                 (SELECT entry.chunk_id AS id, entry.path, entry.ord
                  FROM sql_subject_lookup entry
                  WHERE entry.tenant_id = $2
                    AND entry.source_id = original.source_id AND entry.kind = original.kind
                    AND entry.identity_hash = original_subject.identity_hash
                    AND entry.identity = original_subject.identity
                    AND octet_length(entry.path) > 2048
                    AND entry.path > (original.metadata->>'path') COLLATE \"C\"
                  ORDER BY entry.path DESC, entry.ord DESC, entry.chunk_id DESC
                  LIMIT 1)
             ) candidate
             ORDER BY candidate.path DESC, candidate.ord DESC, candidate.id DESC
             LIMIT 1
         ) latest
         WHERE base.id = ANY($1) AND base.tenant_id = $2
         ORDER BY base.id, latest.path COLLATE \"C\" DESC, latest.id",
    )
    .bind(&ids)
    .bind(f.tenant_id)
    .fetch_all(conn)
    .await?;
    let representatives: HashMap<i64, i64> = origins
        .iter()
        .flat_map(|(&id, aliases)| aliases.iter().map(move |&alias| (alias, id)))
        .collect();
    let alias_changes: Vec<SqlChange> = changes
        .iter()
        .filter_map(|change| {
            let &origin = representatives.get(&change.origin)?;
            (origin != change.origin).then(|| SqlChange {
                origin,
                ..change.clone()
            })
        })
        .collect();
    // Keep actual origins too: an alias can itself become a companion of another subject.
    changes.extend(alias_changes);
    Ok(promote_sql(fused, &changes, usize::MAX))
}

fn promote_sql(
    fused: Vec<(i64, f64)>,
    changes: &[SqlChange],
    k: usize,
) -> (Vec<(i64, f64)>, HashMap<i64, Vec<String>>) {
    let mut by_origin: HashMap<i64, Vec<&SqlChange>> = HashMap::new();
    let mut annotations: HashMap<i64, Vec<String>> = HashMap::new();
    for change in changes {
        by_origin.entry(change.origin).or_default().push(change);
        let paths = annotations.entry(change.origin).or_default();
        if !paths.contains(&change.path) {
            paths.push(change.path.clone());
        }
    }
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    for (id, score) in fused {
        // A companion can itself be an older retrieved hit (a grouped chunk may name several
        // subjects). Expand its known changes first too, so promoting it cannot reverse time.
        let mut stack = vec![(id, false)];
        while let Some((id, expanded)) = stack.pop() {
            if seen.contains(&id) {
                continue;
            }
            if expanded {
                seen.insert(id);
                out.push((id, score));
                if out.len() == k {
                    return (out, annotations);
                }
            } else {
                stack.push((id, true));
                if let Some(changes) = by_origin.get(&id) {
                    stack.extend(changes.iter().rev().map(|change| (change.id, false)));
                }
            }
        }
    }
    (out, annotations)
}

/// The filters both branches share.
struct Filter<'a> {
    tenant_id: Uuid,
    kinds: Option<&'a [String]>,
    source: Option<&'a str>,
}

async fn lexical_branch(
    connection: &mut PgConnection,
    q: &str,
    f: &Filter<'_>,
) -> Result<Vec<i64>, AppError> {
    let lexemes: Vec<String> =
        sqlx::query_scalar("SELECT tsvector_to_array(to_tsvector('english', $1))")
            .bind(q)
            .fetch_one(&mut *connection)
            .await?;
    let Some(tsquery) = or_query(&lexemes) else {
        return Ok(Vec::new());
    };
    let ids = sqlx::query_scalar(
        "SELECT c.id
         FROM chunks c
         JOIN documents d ON d.id = c.document_id
         JOIN sources s ON s.id = d.source_id
         WHERE c.tenant_id = $1 AND c.tsv @@ to_tsquery('simple', $2)
           AND ($3::text[] IS NULL OR d.kind = ANY($3)) AND ($4::text IS NULL OR s.name = $4)
         ORDER BY ts_rank_cd(c.tsv, to_tsquery('simple', $2), 32) DESC, c.id
         LIMIT $5",
    )
    .bind(f.tenant_id)
    .bind(&tsquery)
    .bind(f.kinds)
    .bind(f.source)
    .bind(BRANCH_LIMIT)
    .fetch_all(connection)
    .await?;
    Ok(ids)
}

/// The nearest chunks, best first, each with whether it is an enumeration.
async fn vector_branch(
    connection: &mut PgConnection,
    embedding: Vec<f32>,
    f: &Filter<'_>,
) -> Result<Vec<(i64, bool)>, AppError> {
    // The filters are applied after the index scan, so let it keep scanning until it has enough
    // rows that pass them.
    sqlx::query("SET LOCAL hnsw.iterative_scan = 'relaxed_order'")
        .execute(&mut *connection)
        .await?;
    sqlx::query("SET LOCAL hnsw.ef_search = 100")
        .execute(&mut *connection)
        .await?;
    // Exact source-scoped ranking avoids repeated document joins. Adding zero keeps the
    // distance sort off HNSW, including its approximate candidate set and tie ordering.
    // Source-null searches retain the existing approximate index path below.
    let query = if f.source.is_some() {
        "SELECT c.id, c.enumerates
         FROM chunks c
         WHERE c.tenant_id = $1
           AND c.document_id = ANY(ARRAY(
               SELECT d.id FROM documents d JOIN sources s ON s.id = d.source_id
               WHERE ($2::text[] IS NULL OR d.kind = ANY($2)) AND s.name = $3))
         ORDER BY (c.embedding <=> $4) + 0, c.id
         LIMIT $5"
    } else {
        "SELECT c.id, c.enumerates
         FROM chunks c
         JOIN documents d ON d.id = c.document_id
         JOIN sources s ON s.id = d.source_id
         WHERE c.tenant_id = $1
           AND ($2::text[] IS NULL OR d.kind = ANY($2)) AND ($3::text IS NULL OR s.name = $3)
         ORDER BY c.embedding <=> $4, c.id
         LIMIT $5"
    };
    let hits = sqlx::query_as(query)
        .bind(f.tenant_id)
        .bind(f.kinds)
        .bind(f.source)
        .bind(Vector::from(embedding))
        .bind(BRANCH_LIMIT)
        .fetch_all(connection)
        .await?;
    Ok(hits)
}

/// `k` limited to 1..=50.
pub fn clamp_k(k: usize) -> usize {
    k.clamp(1, MAX_K)
}

/// Checks the kinds against the document kinds. `None`, or an empty list, means all kinds.
fn validate_kinds(kinds: Option<Vec<String>>) -> Result<Option<Vec<String>>, AppError> {
    let Some(kinds) = kinds.filter(|k| !k.is_empty()) else {
        return Ok(None);
    };
    if let Some(bad) = kinds.iter().find(|k| !DOCUMENT_KINDS.contains(&k.as_str())) {
        return Err(AppError::BadRequest(format!(
            "unknown kind {bad:?}; the kinds are {}",
            DOCUMENT_KINDS.join(", ")
        )));
    }
    Ok(Some(kinds))
}

/// The tsquery text that matches any of the lexemes: each one single-quoted, joined by ` | `.
/// Inside the quotes a `'` is doubled and a backslash is escaped, so a lexeme can never end the
/// quote or turn into query syntax. `None` when there are no lexemes.
fn or_query(lexemes: &[String]) -> Option<String> {
    if lexemes.is_empty() {
        return None;
    }
    let quoted: Vec<String> = lexemes
        .iter()
        .map(|l| format!("'{}'", l.replace('\\', "\\\\").replace('\'', "''")))
        .collect();
    Some(quoted.join(" | "))
}

/// Reciprocal Rank Fusion of ranked id lists (best first), the lexical list first: an id scores
/// `1 / (60 + rank)` for each list it is in, with 1-based ranks. Returns `(id, score)` best
/// first. Ties go to the id with the better lexical rank (an id missing from the lexical list
/// comes after those in it), then to the lower id. Ranks inside one list are distinct, so the id
/// only keeps the order total; equal scores of different ids with equal lexical ranks cannot
/// happen. Ties inside a branch (equal `ts_rank_cd` or equal distance) are broken by id in SQL.
fn fuse(lists: &[&[i64]]) -> Vec<(i64, f64)> {
    struct Entry {
        score: f64,
        lexical_rank: usize,
    }
    let mut entries: HashMap<i64, Entry> = HashMap::new();
    for (n, list) in lists.iter().enumerate() {
        for (i, id) in list.iter().enumerate() {
            let e = entries.entry(*id).or_insert(Entry {
                score: 0.0,
                lexical_rank: usize::MAX,
            });
            if n == 0 {
                e.lexical_rank = i + 1;
            }
            e.score += 1.0 / (RRF_K + (i + 1) as f64);
        }
    }
    let mut out: Vec<(i64, Entry)> = entries.into_iter().collect();
    out.sort_by(|(a_id, a), (b_id, b)| {
        b.score
            .total_cmp(&a.score)
            .then(a.lexical_rank.cmp(&b.lexical_rank))
            .then(a_id.cmp(b_id))
    });
    out.into_iter().map(|(id, e)| (id, e.score)).collect()
}

/// Fills a source's web URL template for a chunk. Only `{path}`, `{commit}`, `{start}` and `{end}`
/// are substituted, in one pass over the template, so nothing that is substituted is scanned
/// again; any other `{...}` stays literal. The path is percent-encoded per segment (the `/`
/// between segments stays), so a path cannot add a query, a fragment or a new segment. The
/// commit must be a 40 character hex sha: the caller checks that.
pub fn render_web_url(template: &str, path: &str, commit: &str, start: i32, end: i32) -> String {
    let mut out = String::with_capacity(template.len() + path.len() + 64);
    let mut rest = template;
    while let Some(at) = rest.find('{') {
        let (before, tail) = rest.split_at(at);
        out.push_str(before);
        if let Some(r) = tail.strip_prefix("{path}") {
            push_encoded_path(&mut out, path);
            rest = r;
        } else if let Some(r) = tail.strip_prefix("{commit}") {
            out.push_str(commit);
            rest = r;
        } else if let Some(r) = tail.strip_prefix("{start}") {
            let _ = write!(out, "{start}");
            rest = r;
        } else if let Some(r) = tail.strip_prefix("{end}") {
            let _ = write!(out, "{end}");
            rest = r;
        } else {
            out.push('{');
            rest = tail.strip_prefix('{').unwrap_or(tail);
        }
    }
    out.push_str(rest);
    out
}

/// Percent-encodes every byte but `A-Z a-z 0-9 - . _ ~` in each `/` separated segment.
fn push_encoded_path(out: &mut String, path: &str) {
    for (i, segment) in path.split('/').enumerate() {
        if i > 0 {
            out.push('/');
        }
        for b in segment.bytes() {
            if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
                out.push(char::from(b));
            } else {
                let _ = write!(out, "%{b:02X}");
            }
        }
    }
}

#[cfg(test)]
mod exact_vector_tests;

#[cfg(test)]
mod tests {
    use super::*;

    const SHA: &str = "0123456789abcdef0123456789abcdef01234567";

    #[test]
    fn duplicate_collapse_uses_full_text_and_keeps_the_best_location() {
        let make = |id: i64, source: &str, text: &str| Hit {
            chunk_id: id,
            document_id: Uuid::nil(),
            source: source.to_string(),
            also_in: Vec::new(),
            kind: "code".to_string(),
            title: "agent".to_string(),
            uri: None,
            path: None,
            external_id: None,
            start_line: None,
            end_line: None,
            symbols: Vec::new(),
            text: text.to_string(),
            score: 1.0 / id as f64,
            later_changes: Vec::new(),
        };
        let found = collapse_duplicates(
            vec![
                make(1, "best", "same"),
                make(2, "z", "same"),
                make(3, "a", "same"),
                make(4, "a", "same"),
                make(5, "best", "same\nmore"),
                make(6, "best", "same"),
            ],
            3,
            &HashMap::new(),
        )
        .0;
        assert_eq!(found.iter().map(|h| h.chunk_id).collect::<Vec<_>>(), [1, 5]);
        assert_eq!(found[0].also_in, ["a", "z"]);
        assert!(found[1].also_in.is_empty());
    }

    #[test]
    fn snippet_is_the_first_40_lines() {
        let text: String = (1..=100).map(|i| format!("line {i}\n")).collect();
        let s = snippet(&text);
        assert_eq!(s.lines().count(), 40);
        assert!(s.starts_with("line 1\n"));
        assert!(s.ends_with("line 40"));
        assert_eq!(snippet("a\nb"), "a\nb");
        assert_eq!(snippet(""), "");
    }

    #[test]
    fn k_is_clamped_to_1_through_50() {
        assert_eq!(clamp_k(0), 1);
        assert_eq!(clamp_k(1), 1);
        assert_eq!(clamp_k(10), 10);
        assert_eq!(clamp_k(50), 50);
        assert_eq!(clamp_k(51), 50);
        assert_eq!(clamp_k(usize::MAX), 50);
    }

    #[test]
    fn sql_companions_are_newest_first_unique_and_bounded() {
        let changes = [
            SqlChange {
                origin: 1,
                id: 2,
                path: "002.sql".into(),
            },
            SqlChange {
                origin: 2,
                id: 3,
                path: "003.sql".into(),
            },
        ];
        let fused = vec![(1, 0.03), (2, 0.02), (3, 0.01), (4, 0.005)];
        let (hits, notes) = promote_sql(fused.clone(), &changes, 5);
        assert_eq!(
            hits.iter().map(|(id, _)| *id).collect::<Vec<_>>(),
            [3, 2, 1, 4]
        );
        assert_eq!(notes[&1], ["002.sql"]);
        assert_eq!(notes[&2], ["003.sql"]);
        assert_eq!(promote_sql(fused, &changes, 1).0, [(3, 0.03)]);
    }

    #[test]
    fn kinds_are_checked_against_the_vocabulary() {
        assert_eq!(validate_kinds(None).unwrap(), None);
        assert_eq!(validate_kinds(Some(vec![])).unwrap(), None);
        let ok = vec!["code".to_string(), "note".to_string()];
        assert_eq!(validate_kinds(Some(ok.clone())).unwrap(), Some(ok));
        let err = validate_kinds(Some(vec!["code".to_string(), "docs".to_string()])).unwrap_err();
        let AppError::BadRequest(msg) = err else {
            panic!("want BadRequest");
        };
        assert!(msg.contains("\"docs\""), "{msg}");
        assert!(msg.contains("code, doc, session, note"), "{msg}");
    }

    #[test]
    fn or_query_quotes_and_escapes_lexemes() {
        assert_eq!(or_query(&[]), None);
        let one = vec!["plan".to_string()];
        assert_eq!(or_query(&one).unwrap(), "'plan'");
        let many = vec!["plan".to_string(), "limit".to_string()];
        assert_eq!(or_query(&many).unwrap(), "'plan' | 'limit'");
        let hostile = vec!["it's".to_string(), r"a\b".to_string(), r"end\".to_string()];
        assert_eq!(or_query(&hostile).unwrap(), r"'it''s' | 'a\\b' | 'end\\'");
        // Syntax characters stay inside the quotes.
        let syntax = vec!["(x) & !y | z:*".to_string()];
        assert_eq!(or_query(&syntax).unwrap(), "'(x) & !y | z:*'");
    }

    #[test]
    fn fusion_adds_the_reciprocal_ranks() {
        let fused = fuse(&[&[1, 2], &[2, 3]]);
        let ids: Vec<i64> = fused.iter().map(|f| f.0).collect();
        assert_eq!(ids, vec![2, 1, 3]);
        let score = |id: i64| fused.iter().find(|f| f.0 == id).unwrap().1;
        assert!((score(2) - (1.0 / 62.0 + 1.0 / 61.0)).abs() < 1e-12);
        assert!((score(1) - 1.0 / 61.0).abs() < 1e-12);
        assert!((score(3) - 1.0 / 62.0).abs() < 1e-12);
    }

    #[test]
    fn a_third_list_adds_its_reciprocal_ranks() {
        // First place in the third list scores like first place in any list: it ties with the
        // vector-only 9 and wins on the lower id, and it beats the vector's second place.
        let fused = fuse(&[&[1, 2], &[9, 8, 2], &[5]]);
        let ids: Vec<i64> = fused.iter().map(|f| f.0).collect();
        assert_eq!(ids, vec![2, 1, 5, 9, 8]);
        let score = |id: i64| fused.iter().find(|f| f.0 == id).unwrap().1;
        assert!((score(5) - 1.0 / 61.0).abs() < 1e-12);
        assert!((score(2) - (1.0 / 62.0 + 1.0 / 63.0)).abs() < 1e-12);
    }

    #[test]
    fn one_branch_alone_keeps_its_order() {
        let ids = |v: Vec<(i64, f64)>| v.into_iter().map(|f| f.0).collect::<Vec<_>>();
        assert_eq!(ids(fuse(&[&[], &[7, 5, 9]])), vec![7, 5, 9]);
        assert_eq!(ids(fuse(&[&[7, 5, 9], &[]])), vec![7, 5, 9]);
        assert!(fuse(&[&[], &[]]).is_empty());
    }

    #[test]
    fn equal_scores_go_to_the_better_lexical_rank_then_the_lower_id() {
        // Lexical-only rank 1 and vector-only rank 1 score the same. The lexical one wins even
        // with the higher id.
        let ids: Vec<i64> = fuse(&[&[9], &[3]]).into_iter().map(|f| f.0).collect();
        assert_eq!(ids, vec![9, 3]);
        // Lexical 1 + vector 2 against lexical 2 + vector 1: equal sums, lexical rank decides.
        let ids: Vec<i64> = fuse(&[&[8, 4], &[4, 8]]).into_iter().map(|f| f.0).collect();
        assert_eq!(ids, vec![8, 4]);
        // The result is the same whatever the order of the ids in the lists.
        let ids: Vec<i64> = fuse(&[&[4, 8], &[8, 4]]).into_iter().map(|f| f.0).collect();
        assert_eq!(ids, vec![4, 8]);
    }

    #[test]
    fn web_url_fills_the_four_placeholders() {
        let url = render_web_url(
            "https://github.com/acme/app/blob/{commit}/{path}#L{start}-L{end}",
            "src/billing/plans.rs",
            SHA,
            7,
            12,
        );
        assert_eq!(
            url,
            format!("https://github.com/acme/app/blob/{SHA}/src/billing/plans.rs#L7-L12")
        );
        // Placeholders may repeat, and a template may omit some.
        assert_eq!(
            render_web_url("https://x/{start}/{start}/{end}", "p", SHA, 1, 2),
            "https://x/1/1/2"
        );
        assert_eq!(
            render_web_url("https://x/static", "p", SHA, 1, 2),
            "https://x/static"
        );
    }

    #[test]
    fn web_url_encodes_each_path_segment() {
        let url = render_web_url("https://x/{path}", "a b/c#d/e?f/g%h/h\u{e9}.rs", SHA, 1, 1);
        assert_eq!(url, "https://x/a%20b/c%23d/e%3Ff/g%25h/h%C3%A9.rs");
        // Unreserved characters stay, and so do the slashes between segments.
        assert_eq!(
            render_web_url("https://x/{path}", "src/a-b_c.d~e/f.rs", SHA, 1, 1),
            "https://x/src/a-b_c.d~e/f.rs"
        );
        // Characters that could change the meaning of a URL are all encoded.
        let url = render_web_url(
            "https://x/{path}",
            "a:b@c&d=e+f;g,h\"i<j>k{l}m\\n|o",
            SHA,
            1,
            1,
        );
        assert_eq!(
            url,
            "https://x/a%3Ab%40c%26d%3De%2Bf%3Bg%2Ch%22i%3Cj%3Ek%7Bl%7Dm%5Cn%7Co"
        );
    }

    #[test]
    fn web_url_substitutes_in_one_pass() {
        // A path that spells a placeholder is encoded, and is not substituted a second time.
        let url = render_web_url("https://x/{path}?c={commit}", "{commit}/{start}", SHA, 1, 2);
        assert_eq!(url, format!("https://x/%7Bcommit%7D/%7Bstart%7D?c={SHA}"));
    }

    #[test]
    fn web_url_keeps_unknown_placeholders_literal() {
        assert_eq!(
            render_web_url("https://x/{branch}/{path}/{PATH}/{", "f.rs", SHA, 1, 2),
            "https://x/{branch}/f.rs/{PATH}/{"
        );
        assert_eq!(
            render_web_url("https://x/{{path}}", "f.rs", SHA, 1, 2),
            "https://x/{f.rs}"
        );
        assert_eq!(
            render_web_url("https://x/}{", "f.rs", SHA, 1, 2),
            "https://x/}{"
        );
        // Non-ASCII text in the template is kept as is.
        assert_eq!(
            render_web_url("https://x/\u{e9}/{path}", "f", SHA, 1, 2),
            "https://x/\u{e9}/f"
        );
    }
}
