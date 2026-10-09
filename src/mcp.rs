//! The MCP server: lore's search, ask and document tools for agents, over streamable HTTP at
//! `/mcp`.
//!
//! The endpoint is stateless (no sessions) and answers with plain JSON. An axum middleware
//! authenticates every request with the same tenant keys as the REST API and puts the [`Caller`]
//! in the request extensions; rmcp copies the request `Parts` into the tool context, so each tool
//! reads the caller from there. Every tool is scoped to the caller's tenant.

use std::sync::Arc;

use axum::extract::{Request, State};
use axum::http::request::Parts;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::tool::Extension;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{
    CallToolResult, ContentBlock, ErrorData, Implementation, ServerCapabilities, ServerConfig,
};
use rmcp::transport::streamable_http_server::session::never::NeverSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};
use rmcp::{ServerHandler, schemars, tool, tool_handler, tool_router};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::answer::{self, AskRequest, Citation, Mode};
use crate::auth::{Caller, Scope, caller_from_headers};
use crate::error::AppError;
use crate::http::AppState;
use crate::search::{SearchQuery, search, snippet};
use crate::store::{DocumentRow, document_text, list_source_summaries};

/// What the server tells a client about itself.
const INSTRUCTIONS: &str = "Search and ask questions about indexed code, documents and agent sessions. Use `ask` for a short answer with citations, `search` to find raw passages, `get_document` to read one in full.";

/// Hits returned by `search` when `k` is not given.
const DEFAULT_SEARCH_K: usize = 8;

/// The longest `get_document` text, in characters, header included.
pub const MAX_DOCUMENT_CHARS: usize = 100_000;

/// Builds the `/mcp` service. It is stateless, answers with plain JSON, and checks the `Host`
/// header only when `LORE_MCP_ALLOWED_HOSTS` is set (the bearer key is the real protection).
pub fn service(state: AppState) -> StreamableHttpService<LoreMcp, NeverSessionManager> {
    let config = StreamableHttpServerConfig::default()
        .with_legacy_session_mode(false)
        .with_json_response(true);
    let config = match state.config.mcp_allowed_hosts.clone() {
        Some(hosts) => config.with_allowed_hosts(hosts),
        None => config.disable_allowed_hosts(),
    };
    StreamableHttpService::new(
        move || Ok(LoreMcp::new(state.clone())),
        Arc::new(NeverSessionManager::default()),
        config,
    )
}

/// Authenticates a request to `/mcp`: a missing or bad key is a 401 and a key without the read
/// scope is a 403, both with the standard error body, before rmcp sees the request.
pub async fn require_caller(
    State(state): State<AppState>,
    mut req: Request,
    next: Next,
) -> Response {
    let caller = match caller_from_headers(&state, req.headers()).await {
        Ok(caller) => caller,
        Err(e) => return e.into_response(),
    };
    if let Err(e) = caller.require(Scope::Read) {
        return e.into_response();
    }
    req.extensions_mut().insert(caller);
    next.run(req).await
}

#[derive(Clone)]
pub struct LoreMcp {
    state: AppState,
    tool_router: ToolRouter<Self>,
}

impl LoreMcp {
    pub fn new(state: AppState) -> Self {
        Self {
            state,
            tool_router: Self::tool_router(),
        }
    }
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct SearchArgs {
    /// What to look for: a name, a phrase or a question.
    query: String,
    /// How many passages to return, 1 to 50 (default 8). Other values are clamped.
    k: Option<i64>,
    /// Only these document kinds: `code`, `doc`, `session` or `note`.
    kind: Option<Vec<String>>,
}

#[derive(Debug, Clone, Copy, Default, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum AskMode {
    #[default]
    Fast,
    Deep,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct AskArgs {
    /// The question, in plain language.
    question: String,
    /// `fast` (default) answers from the best passages. `deep` is not available yet.
    mode: Option<AskMode>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct GetDocumentArgs {
    /// The `document_id` of a search hit or an ask citation (a UUID).
    document_id: String,
}

/// One `search` hit, as the tool returns it.
#[derive(Debug, Serialize)]
struct SearchHit {
    document_id: Uuid,
    path: Option<String>,
    start_line: Option<i32>,
    end_line: Option<i32>,
    kind: String,
    title: String,
    source: String,
    score: f64,
    snippet: String,
}

fn ok(content: Vec<String>) -> Result<CallToolResult, ErrorData> {
    Ok(CallToolResult::success(
        content.into_iter().map(ContentBlock::text).collect(),
    ))
}

/// A tool-level error: the caller sees the message and the call is marked as failed.
fn failed(message: impl Into<String>) -> Result<CallToolResult, ErrorData> {
    Ok(CallToolResult::error(vec![ContentBlock::text(message)]))
}

/// An application error as a tool error. Internal errors are logged and reported without detail.
fn app_failed(e: AppError) -> Result<CallToolResult, ErrorData> {
    if let AppError::Internal(inner) = &e {
        tracing::error!(error = ?inner, "mcp tool failed");
    }
    failed(e.message())
}

/// The caller the auth middleware stored on the request.
fn caller_of(parts: &Parts) -> Result<Caller, ErrorData> {
    parts
        .extensions
        .get::<Caller>()
        .cloned()
        .ok_or_else(|| ErrorData::internal_error("request was not authenticated", None))
}

#[tool_router]
impl LoreMcp {
    #[tool(
        description = "Find raw passages in the indexed code, documents and agent sessions. Returns a JSON array of hits with path, line range, kind, title, source, score and a snippet (the first 40 lines).",
        annotations(read_only_hint = true)
    )]
    async fn search(
        &self,
        Extension(parts): Extension<Parts>,
        Parameters(args): Parameters<SearchArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let caller = caller_of(&parts)?;
        let k = args.k.map_or(DEFAULT_SEARCH_K, |k| {
            crate::search::clamp_k(usize::try_from(k).unwrap_or(0))
        });
        let query = SearchQuery {
            tenant_id: caller.tenant_id,
            q: args.query,
            k,
            kinds: args.kind.filter(|kinds| !kinds.is_empty()),
            source: None,
        };
        let hits = match search(&self.state.pool, self.state.embedder.clone(), query).await {
            Ok(hits) => hits,
            Err(e) => return app_failed(e),
        };
        let items: Vec<SearchHit> = hits
            .into_iter()
            .map(|h| SearchHit {
                document_id: h.document_id,
                path: h.path,
                start_line: h.start_line,
                end_line: h.end_line,
                kind: h.kind,
                title: h.title,
                source: h.source,
                score: h.score,
                snippet: snippet(&h.text),
            })
            .collect();
        match serde_json::to_string(&items) {
            Ok(json) => ok(vec![json]),
            Err(e) => app_failed(AppError::Internal(e.into())),
        }
    }

    #[tool(
        description = "Answer a question about the indexed material in a few sentences, with citations. Returns the answer and its sources, then the full response as JSON.",
        annotations(read_only_hint = true)
    )]
    async fn ask(
        &self,
        Extension(parts): Extension<Parts>,
        Parameters(args): Parameters<AskArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let caller = caller_of(&parts)?;
        let mode = match args.mode.unwrap_or_default() {
            AskMode::Fast => Mode::Fast,
            AskMode::Deep => Mode::Deep,
        };
        let req = AskRequest {
            question: args.question,
            mode,
            k: None,
            kind: None,
            source: None,
        };
        let res = match answer::ask(&self.state, caller.tenant_id, req).await {
            Ok(res) => res,
            Err(e) => return app_failed(e),
        };
        let json = match serde_json::to_string(&res) {
            Ok(json) => json,
            Err(e) => return app_failed(AppError::Internal(e.into())),
        };
        ok(vec![answer_text(&res.answer, &res.citations), json])
    }

    #[tool(
        description = "Read one document in full (up to 100,000 characters), by the document_id of a search hit or an ask citation.",
        annotations(read_only_hint = true)
    )]
    async fn get_document(
        &self,
        Extension(parts): Extension<Parts>,
        Parameters(args): Parameters<GetDocumentArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let caller = caller_of(&parts)?;
        let Ok(id) = Uuid::parse_str(args.document_id.trim()) else {
            return failed("document_id must be a UUID");
        };
        match document_text(&self.state.pool, caller.tenant_id, id).await {
            Some((row, text)) => ok(vec![render_document(&row, &text)]),
            None => failed("document not found"),
        }
    }

    #[tool(
        description = "List the indexed sources with their kind, the commit a repo is indexed at, and the number of documents each holds.",
        annotations(read_only_hint = true)
    )]
    async fn list_sources(
        &self,
        Extension(parts): Extension<Parts>,
    ) -> Result<CallToolResult, ErrorData> {
        let caller = caller_of(&parts)?;
        let sources = match list_source_summaries(&self.state.pool, caller.tenant_id).await {
            Ok(sources) => sources,
            Err(e) => return app_failed(e.into()),
        };
        match serde_json::to_string(&sources) {
            Ok(json) => ok(vec![json]),
            Err(e) => app_failed(AppError::Internal(e.into())),
        }
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for LoreMcp {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("lore", env!("CARGO_PKG_VERSION")))
            .with_instructions(INSTRUCTIONS)
    }
}

/// The answer, a blank line, `Sources:` and one line per citation.
fn answer_text(answer: &str, citations: &[Citation]) -> String {
    let mut out = format!("{answer}\n\nSources:\n");
    if citations.is_empty() {
        out.push_str("(none)");
    } else {
        let lines: Vec<String> = citations.iter().map(citation_line).collect();
        out.push_str(&lines.join("\n"));
    }
    out
}

/// `[1] src/billing/plans.rs:3-10 (https://...)`: the path with its lines, else the title, and the
/// link when there is one.
fn citation_line(c: &Citation) -> String {
    let place = match (&c.path, c.start_line, c.end_line) {
        (Some(path), Some(start), Some(end)) => format!("{path}:{start}-{end}"),
        (Some(path), _, _) => path.clone(),
        (None, _, _) => c
            .title
            .clone()
            .or_else(|| c.document_id.map(|id| id.to_string()))
            .unwrap_or_default(),
    };
    match &c.url {
        Some(url) => format!("[{}] {place} ({url})", c.label),
        None => format!("[{}] {place}", c.label),
    }
}

/// `# <title>\n<kind> · <path or uri>\n\n` and the text, cut to [`MAX_DOCUMENT_CHARS`]
/// characters in all.
fn render_document(row: &DocumentRow, text: &str) -> String {
    let place = row.path.as_deref().or(row.uri.as_deref());
    let mut out = format!("# {}\n{}", row.title, row.kind);
    if let Some(place) = place {
        out.push_str(" · ");
        out.push_str(place);
    }
    out.push_str("\n\n");
    out.push_str(text);
    match out.char_indices().nth(MAX_DOCUMENT_CHARS) {
        Some((cut, _)) => {
            out.truncate(cut);
            out
        }
        None => out,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(path: Option<&str>, uri: Option<&str>) -> DocumentRow {
        DocumentRow {
            id: Uuid::nil(),
            source_id: Uuid::nil(),
            source: "sample".to_string(),
            kind: "code".to_string(),
            title: "plans.rs".to_string(),
            uri: uri.map(str::to_string),
            path: path.map(str::to_string),
        }
    }

    fn cite(label: &str) -> Citation {
        Citation {
            label: label.to_string(),
            document_id: None,
            kind: None,
            title: None,
            path: None,
            start_line: None,
            end_line: None,
            url: None,
        }
    }

    #[test]
    fn document_header_has_title_kind_and_path() {
        let s = render_document(
            &row(Some("src/billing/plans.rs"), Some("https://x/y")),
            "body",
        );
        assert_eq!(s, "# plans.rs\ncode · src/billing/plans.rs\n\nbody");
    }

    #[test]
    fn document_header_falls_back_to_the_uri_then_to_the_kind() {
        let s = render_document(&row(None, Some("https://x/y")), "body");
        assert_eq!(s, "# plans.rs\ncode · https://x/y\n\nbody");
        let s = render_document(&row(None, None), "body");
        assert_eq!(s, "# plans.rs\ncode\n\nbody");
    }

    #[test]
    fn document_is_cut_at_100000_characters_header_included() {
        let header = "# plans.rs\ncode · p\n\n";
        let r = row(Some("p"), None);
        let exact = "a".repeat(MAX_DOCUMENT_CHARS - header.chars().count());
        let s = render_document(&r, &exact);
        assert_eq!(s.chars().count(), MAX_DOCUMENT_CHARS);
        assert!(s.ends_with('a'));

        let s = render_document(&r, &format!("{exact}bcd"));
        assert_eq!(s.chars().count(), MAX_DOCUMENT_CHARS);
        assert!(!s.contains('b'));
    }

    #[test]
    fn document_cut_counts_characters_not_bytes() {
        let r = row(None, None);
        let s = render_document(&r, &"é".repeat(MAX_DOCUMENT_CHARS));
        assert_eq!(s.chars().count(), MAX_DOCUMENT_CHARS);
        assert!(s.len() > MAX_DOCUMENT_CHARS);
        assert!(s.ends_with('é'));
    }

    #[test]
    fn sources_list_one_line_per_citation() {
        let mut a = cite("1");
        a.path = Some("src/billing/plans.rs".to_string());
        a.start_line = Some(3);
        a.end_line = Some(10);
        a.url = Some("https://git/x".to_string());
        let mut b = cite("2");
        b.title = Some("Standup notes".to_string());
        let mut c = cite("3");
        c.path = Some("README.md".to_string());
        assert_eq!(
            answer_text("Three plans.", &[a, b, c]),
            "Three plans.\n\nSources:\n[1] src/billing/plans.rs:3-10 (https://git/x)\n[2] Standup notes\n[3] README.md"
        );
    }

    #[test]
    fn sources_without_citations_say_none() {
        assert_eq!(answer_text("No idea.", &[]), "No idea.\n\nSources:\n(none)");
    }
}
