//! `GET /v1/search`: hybrid search over the caller's tenant.

use axum::Json;
use axum::extract::State;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::auth::{Caller, Scope};
use crate::error::AppError;
use crate::http::{AppQuery, AppState};
use crate::search::{DEFAULT_K, Hit, SearchQuery, clamp_k, search};

/// A snippet is the first lines of a chunk.
const SNIPPET_LINES: usize = 40;

#[derive(Deserialize)]
pub struct SearchParams {
    q: Option<String>,
    /// Any integer; the search clamps it to 1..=50.
    k: Option<i64>,
    /// Comma-separated document kinds.
    kind: Option<String>,
    /// A source name.
    source: Option<String>,
}

#[derive(Serialize)]
pub struct HitItem {
    chunk_id: i64,
    document_id: Uuid,
    source: String,
    kind: String,
    title: String,
    uri: Option<String>,
    path: Option<String>,
    start_line: Option<i32>,
    end_line: Option<i32>,
    symbols: Vec<String>,
    snippet: String,
    score: f64,
}

#[derive(Serialize)]
pub struct SearchResponse {
    hits: Vec<HitItem>,
}

impl From<Hit> for HitItem {
    fn from(h: Hit) -> Self {
        Self {
            chunk_id: h.chunk_id,
            document_id: h.document_id,
            source: h.source,
            kind: h.kind,
            title: h.title,
            uri: h.uri,
            path: h.path,
            start_line: h.start_line,
            end_line: h.end_line,
            symbols: h.symbols,
            snippet: snippet(&h.text),
            score: h.score,
        }
    }
}

/// The first 40 lines of `text`, joined by `\n`.
fn snippet(text: &str) -> String {
    text.lines()
        .take(SNIPPET_LINES)
        .collect::<Vec<_>>()
        .join("\n")
}

/// `code,doc` to `["code", "doc"]`. Blank entries are dropped, and no entries at all means no
/// filter. Whether each kind exists is checked by the search.
fn parse_kinds(kind: Option<&str>) -> Option<Vec<String>> {
    let kinds: Vec<String> = kind?
        .split(',')
        .map(str::trim)
        .filter(|k| !k.is_empty())
        .map(str::to_string)
        .collect();
    (!kinds.is_empty()).then_some(kinds)
}

pub async fn get_search(
    caller: Caller,
    State(state): State<AppState>,
    AppQuery(params): AppQuery<SearchParams>,
) -> Result<Json<SearchResponse>, AppError> {
    caller.require(Scope::Read)?;
    let q = params
        .q
        .filter(|q| !q.trim().is_empty())
        .ok_or_else(|| AppError::BadRequest("q is required".to_string()))?;
    let k = params
        .k
        .map_or(DEFAULT_K, |k| clamp_k(usize::try_from(k).unwrap_or(0)));
    let query = SearchQuery {
        tenant_id: caller.tenant_id,
        q,
        k,
        kinds: parse_kinds(params.kind.as_deref()),
        source: params.source.filter(|s| !s.trim().is_empty()),
    };
    let hits = search(&state.pool, state.embedder.clone(), query).await?;
    Ok(Json(SearchResponse {
        hits: hits.into_iter().map(HitItem::from).collect(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn kinds_are_split_on_commas() {
        assert_eq!(parse_kinds(None), None);
        assert_eq!(parse_kinds(Some("")), None);
        assert_eq!(parse_kinds(Some(" , ")), None);
        assert_eq!(
            parse_kinds(Some("code,doc")),
            Some(vec!["code".to_string(), "doc".to_string()])
        );
        assert_eq!(
            parse_kinds(Some(" code ,,note,")),
            Some(vec!["code".to_string(), "note".to_string()])
        );
    }
}
