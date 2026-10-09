//! Session documents: the spans of one agent session rendered as a markdown transcript, indexed
//! like any other document so `search` and `ask` can find what an agent did.
//!
//! `render` is pure. `process_due_sessions` is the worker step: it picks the sessions that have
//! gone quiet, renders them and indexes the result.

use std::collections::HashSet;
use std::sync::LazyLock;

use anyhow::Result;
use chrono::{DateTime, SecondsFormat, Utc};
use regex::Regex;
use serde::Serialize;
use serde_json::Value;
use sha2::{Digest, Sha256};
use sqlx::PgPool;
use uuid::Uuid;

use crate::chunk::chunk_markdown;
use crate::http::AppState;
use crate::otlp::{SpanRow, TRUNCATED_SUFFIX};
use crate::store::{self, NewDocument};

/// A rendered session: what gets indexed.
#[derive(Debug, Clone, PartialEq)]
pub struct SessionDoc {
    pub title: String,
    pub text: String,
    pub metadata: Value,
}

/// The `metadata` of a session document.
#[derive(Serialize)]
struct SessionMetadata {
    session_id: String,
    trace_ids: Vec<String>,
    agent: String,
    models: Vec<String>,
    span_count: usize,
    error_count: usize,
    cost_usd: Option<f64>,
    rius_workspace_id: Option<String>,
    started_at: Option<String>,
    ended_at: Option<String>,
}

const LLM_OPERATIONS: [&str; 3] = ["chat", "text_completion", "generate_content"];
const USER_TEXT_MAX: usize = 2000;
const MODEL_TEXT_MAX: usize = 2000;
const TOOL_ARGS_MAX: usize = 500;
const TOOL_RESULT_MAX: usize = 1000;
const TITLE_MAX: usize = 80;

/// A span the client has opened but not finished (`glassflow.span.pending` is `true`). It is
/// replaced by the finished span, so it is left out of the document.
pub fn is_pending(span: &SpanRow) -> bool {
    span.attributes.get("glassflow.span.pending") == Some(&Value::Bool(true))
}

/// The attribute `key` as text. Strings are used as they are and other values as JSON; blank
/// strings and `null` count as missing.
fn attr_text(span: &SpanRow, key: &str) -> Option<String> {
    match span.attributes.get(key)? {
        Value::Null => None,
        Value::String(s) => (!s.trim().is_empty()).then(|| s.clone()),
        other => Some(other.to_string()),
    }
}

fn attr_str<'a>(span: &'a SpanRow, key: &str) -> Option<&'a str> {
    span.attributes
        .get(key)?
        .as_str()
        .filter(|s| !s.trim().is_empty())
}

/// The first `max` characters of `s`.
fn cut(s: &str, max: usize) -> String {
    s.chars().take(max).collect()
}

/// Keeps text from a span from becoming markdown structure of the document: a line that starts
/// with `#`, a backtick or `~` gets a backslash, so it cannot open a section or an unbalanced
/// code fence that would swallow the headings after it.
fn defang(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for (i, line) in s.lines().enumerate() {
        if i > 0 {
            out.push('\n');
        }
        if line.trim_start().starts_with(['#', '`', '~']) {
            out.push('\\');
        }
        out.push_str(line);
    }
    out
}

/// The messages of a `gen_ai.*.messages` attribute. `cut` is true when the attribute was a JSON
/// string that no longer parses (ingest cuts strings at 64 KiB) and `items` are only what could
/// be read from its start.
struct Messages {
    items: Vec<Value>,
    cut: bool,
}

static ROLE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#""role"\s*:\s*"([^"\\]*)""#).expect("the role pattern is valid"));
static TEXT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#""(?:content|text)"\s*:\s*"((?:[^"\\]|\\.)*)"#).expect("the text pattern is valid")
});

/// The text of a JSON string body that may end in the middle of an escape.
fn unescape_json(raw: &str) -> String {
    let parse = |s: &str| serde_json::from_str::<String>(&format!("\"{s}\"")).ok();
    parse(raw)
        .or_else(|| raw.rfind('\\').and_then(|i| parse(&raw[..i])))
        .unwrap_or_else(|| raw.to_string())
}

/// A message from the end of a cut history, where the element itself is cut: its role and the
/// start of its first text.
fn partial_message(rest: &str) -> Option<Value> {
    let role = ROLE.captures(rest)?.get(1)?.as_str();
    let text = TEXT
        .captures(rest)
        .and_then(|c| c.get(1))
        .map(|m| unescape_json(m.as_str()))
        .unwrap_or_default();
    Some(serde_json::json!({ "role": role, "content": text }))
}

/// The messages that can be read from a JSON array that was cut: every complete element at its
/// start, then the role and the start of the text of the element that was cut.
fn salvage_messages(s: &str) -> Vec<Value> {
    let s = s.strip_suffix(TRUNCATED_SUFFIX).unwrap_or(s);
    let mut rest = s.trim_start().strip_prefix('[').unwrap_or(s);
    let mut items = Vec::new();
    loop {
        rest = rest.trim_start_matches(|c: char| c.is_whitespace() || c == ',');
        if rest.is_empty() || rest.starts_with(']') {
            break;
        }
        let mut stream = serde_json::Deserializer::from_str(rest).into_iter::<Value>();
        match stream.next() {
            Some(Ok(value)) => {
                let used = stream.byte_offset();
                if value.is_object() {
                    items.push(value);
                }
                rest = &rest[used..];
            }
            _ => {
                items.extend(partial_message(rest));
                break;
            }
        }
    }
    items
}

/// A JSON array of messages, from an attribute that holds it as a JSON string or as an array.
/// `None` when the attribute is neither.
fn parse_messages(value: &Value) -> Option<Messages> {
    match value {
        Value::Array(a) => Some(Messages {
            items: a.clone(),
            cut: false,
        }),
        Value::String(s) => match serde_json::from_str::<Value>(s) {
            Ok(Value::Array(items)) => Some(Messages { items, cut: false }),
            Ok(_) => None,
            Err(_) if s.trim_start().starts_with('[') => Some(Messages {
                items: salvage_messages(s),
                cut: true,
            }),
            Err(_) => None,
        },
        _ => None,
    }
}

/// The text of one message: its `text` parts joined by newlines, or a string `content`.
fn message_text(message: &Value) -> String {
    let from_parts = message
        .get("parts")
        .and_then(Value::as_array)
        .map(|parts| {
            parts
                .iter()
                .filter(|p| p.get("type").and_then(Value::as_str) == Some("text"))
                .filter_map(|p| p.get("content").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default();
    if !from_parts.trim().is_empty() {
        return from_parts;
    }
    message
        .get("content")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

fn role_of(message: &Value) -> &str {
    message
        .get("role")
        .and_then(Value::as_str)
        .unwrap_or("user")
}

/// System and developer messages are instructions, not something the user said.
fn is_instruction(message: &Value) -> bool {
    matches!(role_of(message), "system" | "developer")
}

/// The `(role, text)` pairs the user side of an LLM call adds: the messages after the last
/// assistant message, without system messages, or `input.value` as one `user` message when there
/// are no input messages. When the history was cut, only the last user turn that can still be
/// read is returned, or `input.value` when there is none.
fn user_messages(span: &SpanRow) -> Vec<(String, String)> {
    let parsed = span
        .attributes
        .get("gen_ai.input.messages")
        .and_then(parse_messages);
    let from_input_value = || match attr_text(span, "input.value") {
        Some(v) => vec![("user".to_string(), v)],
        None => Vec::new(),
    };
    let Some(Messages { items, cut }) = parsed else {
        return from_input_value();
    };
    let pairs = |messages: &[Value]| -> Vec<(String, String)> {
        messages
            .iter()
            .filter(|m| !is_instruction(m))
            .map(|m| (role_of(m).to_string(), message_text(m)))
            .filter(|(_, text)| !text.trim().is_empty())
            .collect()
    };
    if cut {
        let last_user = pairs(&items).into_iter().rfind(|(role, _)| role == "user");
        return last_user.map_or_else(from_input_value, |m| vec![m]);
    }
    let start = items
        .iter()
        .rposition(|m| m.get("role").and_then(Value::as_str) == Some("assistant"))
        .map_or(0, |i| i + 1);
    pairs(&items[start..])
}

/// What the model said: every output message, or `output.value`.
fn model_text(span: &SpanRow) -> Option<String> {
    if let Some(value) = span.attributes.get("gen_ai.output.messages") {
        match parse_messages(value) {
            Some(Messages { items, cut }) => {
                let text = items
                    .iter()
                    .map(message_text)
                    .filter(|t| !t.trim().is_empty())
                    .collect::<Vec<_>>()
                    .join("\n");
                if !text.is_empty() {
                    return Some(text);
                }
                // A cut history with nothing readable in it is shown as it is.
                if let Some(raw) = value.as_str().filter(|s| cut && !s.trim().is_empty()) {
                    return Some(raw.to_string());
                }
            }
            None => {
                if let Some(raw) = value.as_str().filter(|s| !s.trim().is_empty()) {
                    return Some(raw.to_string());
                }
            }
        }
    }
    attr_text(span, "output.value")
}

fn is_tool(span: &SpanRow) -> bool {
    attr_str(span, "gen_ai.operation.name") == Some("execute_tool")
        || attr_str(span, "gen_ai.tool.name").is_some()
}

fn is_llm(span: &SpanRow) -> bool {
    attr_str(span, "gen_ai.operation.name").is_some_and(|op| LLM_OPERATIONS.contains(&op))
        || [
            "input.value",
            "output.value",
            "gen_ai.input.messages",
            "gen_ai.output.messages",
        ]
        .iter()
        .any(|k| attr_text(span, k).is_some())
}

fn is_error(span: &SpanRow) -> bool {
    span.status_code == "ERROR"
}

/// `exception: <type>: <message>` for each `exception` event of the span.
fn exception_lines(span: &SpanRow) -> Vec<String> {
    let Some(events) = span.events.as_array() else {
        return Vec::new();
    };
    events
        .iter()
        .filter(|e| e.get("name").and_then(Value::as_str) == Some("exception"))
        .map(|e| {
            let get = |key: &str| {
                e.get("attributes")
                    .and_then(|a| a.get(key))
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
            };
            let mut line = "exception".to_string();
            for part in [get("exception.type"), get("exception.message")]
                .into_iter()
                .flatten()
            {
                line.push_str(": ");
                line.push_str(part);
            }
            defang(&line)
        })
        .collect()
}

/// The lines of one span's section after its heading.
fn section_body(span: &SpanRow) -> Vec<String> {
    let mut lines = Vec::new();
    if is_tool(span) {
        let name = attr_str(span, "gen_ai.tool.name").unwrap_or("unknown");
        let args = attr_text(span, "gen_ai.tool.call.arguments")
            .or_else(|| attr_text(span, "input.value"));
        let result =
            attr_text(span, "gen_ai.tool.call.result").or_else(|| attr_text(span, "output.value"));
        let mut line = format!("tool {name}:");
        if let Some(args) = args {
            line.push(' ');
            line.push_str(&cut(&args, TOOL_ARGS_MAX));
        }
        if let Some(result) = result {
            line.push_str(" \u{2192} ");
            line.push_str(&cut(&result, TOOL_RESULT_MAX));
        }
        lines.push(defang(&line));
    } else if is_llm(span) {
        // The cut counts message text only, not the `role: ` prefixes.
        let mut budget = USER_TEXT_MAX;
        let mut block = Vec::new();
        for (role, text) in user_messages(span) {
            if budget == 0 {
                break;
            }
            let text = cut(&text, budget);
            budget -= text.chars().count();
            block.push(format!("{role}: {text}"));
        }
        if !block.is_empty() {
            lines.push(defang(&block.join("\n")));
        }
        if let Some(text) = model_text(span) {
            lines.push(defang(&format!(
                "assistant: {}",
                cut(&text, MODEL_TEXT_MAX)
            )));
        }
    }
    lines.extend(exception_lines(span));
    lines
}

fn single_line(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn rfc3339(t: DateTime<Utc>) -> String {
    t.to_rfc3339_opts(SecondsFormat::Secs, true)
}

fn cost_of(span: &SpanRow) -> Option<f64> {
    match span.attributes.get("rius.cost_usd")? {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
    .filter(|c| c.is_finite())
}

/// Renders the spans of one session as a markdown transcript. The spans may come in any order.
/// Pending spans are left out of everything.
pub fn render(spans: &[SpanRow]) -> SessionDoc {
    let mut all: Vec<&SpanRow> = spans.iter().collect();
    all.sort_by(|a, b| (a.start_time, &a.span_id).cmp(&(b.start_time, &b.span_id)));
    let session_id = all
        .first()
        .map(|s| s.session_id.clone())
        .unwrap_or_default();
    let spans: Vec<&SpanRow> = all.into_iter().filter(|s| !is_pending(s)).collect();

    let agent = spans
        .iter()
        .find_map(|s| attr_str(s, "gen_ai.agent.name"))
        .or_else(|| {
            spans
                .iter()
                .find_map(|s| s.resource.get("service.name").and_then(Value::as_str))
                .filter(|n| !n.trim().is_empty())
        })
        .unwrap_or("unknown")
        .to_string();

    let mut models: Vec<String> = Vec::new();
    for s in &spans {
        let model =
            attr_str(s, "gen_ai.request.model").or_else(|| attr_str(s, "gen_ai.response.model"));
        if let Some(m) = model
            && !models.iter().any(|x| x == m)
        {
            models.push(m.to_string());
        }
    }

    let mut trace_ids: Vec<String> = Vec::new();
    let mut seen = HashSet::new();
    for s in &spans {
        if seen.insert(s.trace_id.as_str()) {
            trace_ids.push(s.trace_id.clone());
        }
    }

    let error_count = spans.iter().filter(|s| is_error(s)).count();
    let costs: Vec<f64> = spans.iter().filter_map(|s| cost_of(s)).collect();
    let cost = (!costs.is_empty()).then(|| {
        let sum: f64 = costs.iter().sum();
        (sum * 10_000.0).round() / 10_000.0
    });
    let started = spans.iter().map(|s| s.start_time).min();
    let ended = spans.iter().map(|s| s.end_time).max();
    let workspace = spans
        .iter()
        .find_map(|s| s.resource.get("rius.workspace.id").and_then(Value::as_str))
        .map(str::to_string);

    let mut text = format!("# Session {session_id}\n");
    text.push_str(&format!("agent: {}\n", single_line(&agent)));
    let models_line = if models.is_empty() {
        "unknown".to_string()
    } else {
        models.join(", ")
    };
    text.push_str(&format!("models: {}\n", single_line(&models_line)));
    let cost_line = cost.map_or("unknown".to_string(), |c| format!("${c:.4}"));
    text.push_str(&format!(
        "spans: {} \u{b7} errors: {error_count} \u{b7} cost: {cost_line}\n",
        spans.len()
    ));
    text.push_str(&format!(
        "started: {} \u{b7} ended: {}\n",
        started.map_or("unknown".to_string(), rfc3339),
        ended.map_or("unknown".to_string(), rfc3339)
    ));

    let mut title_text: Option<String> = None;
    for span in &spans {
        if !(is_llm(span) || is_tool(span) || is_error(span)) {
            continue;
        }
        let body = section_body(span);
        if title_text.is_none() && !is_tool(span) {
            title_text = user_messages(span)
                .into_iter()
                .find(|(role, _)| role == "user")
                .map(|(_, t)| t);
        }
        text.push_str(&format!(
            "\n## {} {} [span {}]",
            span.start_time.format("%H:%M:%S"),
            single_line(&span.name),
            span.span_id
        ));
        if is_error(span) {
            text.push_str(" ERROR");
            if let Some(msg) = span.status_message.as_deref().map(single_line)
                && !msg.is_empty()
            {
                text.push_str(&format!(": {msg}"));
            }
        }
        text.push('\n');
        for line in body {
            text.push_str(&line);
            text.push('\n');
        }
    }

    let title = title_text
        .map(|t| cut(single_line(&t).as_str(), TITLE_MAX).trim().to_string())
        .filter(|t| !t.is_empty())
        .or_else(|| {
            spans
                .iter()
                .find(|s| s.parent_span_id.is_none())
                .map(|s| single_line(&s.name))
                .filter(|n| !n.is_empty())
        })
        .unwrap_or_else(|| format!("Session {session_id}"));

    let metadata = SessionMetadata {
        session_id,
        trace_ids,
        agent,
        models,
        span_count: spans.len(),
        error_count,
        cost_usd: cost,
        rius_workspace_id: workspace,
        started_at: started.map(rfc3339),
        ended_at: ended.map(rfc3339),
    };
    SessionDoc {
        title,
        text,
        metadata: serde_json::to_value(metadata).unwrap_or(Value::Null),
    }
}

/// Hash of everything that is indexed, so an unchanged session is not embedded again.
fn content_hash(doc: &SessionDoc) -> String {
    let mut h = Sha256::new();
    h.update(doc.title.as_bytes());
    h.update([0]);
    h.update(doc.text.as_bytes());
    h.update([0]);
    h.update(doc.metadata.to_string().as_bytes());
    hex::encode(h.finalize())
}

#[derive(sqlx::FromRow)]
struct SpanRecord {
    trace_id: String,
    span_id: String,
    parent_span_id: Option<String>,
    session_id: String,
    name: String,
    kind: String,
    start_time: DateTime<Utc>,
    end_time: DateTime<Utc>,
    status_code: String,
    status_message: Option<String>,
    resource: Value,
    attributes: Value,
    events: Value,
}

impl From<SpanRecord> for SpanRow {
    fn from(r: SpanRecord) -> Self {
        SpanRow {
            trace_id: r.trace_id,
            span_id: r.span_id,
            parent_span_id: r.parent_span_id,
            session_id: r.session_id,
            name: r.name,
            kind: r.kind,
            start_time: r.start_time,
            end_time: r.end_time,
            status_code: r.status_code,
            status_message: r.status_message,
            resource: r.resource.into(),
            attributes: r.attributes,
            events: r.events,
        }
    }
}

async fn load_spans(
    pool: &PgPool,
    tenant_id: Uuid,
    session_id: &str,
) -> sqlx::Result<Vec<SpanRow>> {
    let records: Vec<SpanRecord> = sqlx::query_as(
        "SELECT trace_id, span_id, parent_span_id, session_id, name, kind, start_time, end_time,
                status_code, status_message, resource, attributes, events
         FROM spans WHERE tenant_id = $1 AND session_id = $2
         ORDER BY start_time, span_id",
    )
    .bind(tenant_id)
    .bind(session_id)
    .fetch_all(pool)
    .await?;
    Ok(records.into_iter().map(SpanRow::from).collect())
}

/// Removes the queue row of a session, but only if no span arrived since `last_span_at` was
/// read. Returns whether the row was removed.
pub async fn dequeue_if_unchanged(
    pool: &PgPool,
    tenant_id: Uuid,
    session_id: &str,
    last_span_at: DateTime<Utc>,
) -> sqlx::Result<bool> {
    let res = sqlx::query(
        "DELETE FROM session_queue WHERE tenant_id = $1 AND session_id = $2 AND last_span_at = $3",
    )
    .bind(tenant_id)
    .bind(session_id)
    .bind(last_span_at)
    .execute(pool)
    .await?;
    Ok(res.rows_affected() > 0)
}

/// Renders and indexes one session, then dequeues it unless new spans arrived meanwhile.
/// Returns whether the document was written (false: unchanged or nothing to index).
async fn process_session(
    state: &AppState,
    tenant_id: Uuid,
    session_id: &str,
    last_span_at: DateTime<Utc>,
) -> Result<bool> {
    let spans = load_spans(&state.pool, tenant_id, session_id).await?;
    let mut indexed = false;
    if spans.iter().any(|s| !is_pending(s)) {
        let doc = render(&spans);
        let source = store::ensure_source(&state.pool, tenant_id, "traces", "otlp", None).await?;
        let hash = content_hash(&doc);
        let known = store::document_hash(&state.pool, source.id, session_id).await?;
        if known.as_deref() != Some(hash.as_str()) {
            let chunks = chunk_markdown(&doc.text, "session");
            let new_doc = NewDocument {
                tenant_id,
                source_id: source.id,
                external_id: session_id.to_string(),
                kind: "session".to_string(),
                title: doc.title.clone(),
                uri: None,
                metadata: doc.metadata,
                content_hash: hash,
                body: None,
            };
            store::index_text(state, tenant_id, &source, new_doc, chunks, &doc.title).await?;
            indexed = true;
        }
    }
    dequeue_if_unchanged(&state.pool, tenant_id, session_id, last_span_at).await?;
    Ok(indexed)
}

/// Indexes up to `limit` sessions that have had no new span for `session_quiet_secs`, oldest
/// first. No row lock is held while rendering or embedding; a queue row is removed only when
/// its `last_span_at` is still the value that was read, so spans that arrive meanwhile queue
/// the session again. A session that fails is logged and left in the queue for the next run.
/// Returns how many sessions were taken off the queue.
pub async fn process_due_sessions(state: &AppState, limit: i64) -> Result<usize> {
    let quiet = state.config.session_quiet_secs as f64;
    let due: Vec<(Uuid, String, DateTime<Utc>)> = sqlx::query_as(
        "SELECT tenant_id, session_id, last_span_at FROM session_queue
         WHERE last_span_at < now() - make_interval(secs => $1)
         ORDER BY last_span_at LIMIT $2",
    )
    .bind(quiet)
    .bind(limit)
    .fetch_all(&state.pool)
    .await?;

    let mut done = 0;
    for (tenant_id, session_id, last_span_at) in due {
        match process_session(state, tenant_id, &session_id, last_span_at).await {
            Ok(_) => done += 1,
            Err(e) => {
                tracing::warn!(%tenant_id, session_id, error = %e, "session was not indexed")
            }
        }
    }
    Ok(done)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::otlp::{decode, to_rows};
    use serde_json::json;

    const CLAUDE_CODE: &str = include_str!("../tests/fixtures/otlp/claude_code_session.json");
    const GENAI: &str = include_str!("../tests/fixtures/otlp/otel_genai_session.json");

    fn rows(fixture: &str) -> Vec<SpanRow> {
        let req = decode("application/json", None, fixture.as_bytes()).unwrap();
        to_rows(&req).unwrap().rows
    }

    fn span(id: &str, secs: i64, attributes: Value) -> SpanRow {
        SpanRow {
            trace_id: "5b8efff798038103d269b633813fc60c".to_string(),
            span_id: id.to_string(),
            parent_span_id: None,
            session_id: "s1".to_string(),
            name: format!("span {id}"),
            kind: "INTERNAL".to_string(),
            start_time: DateTime::from_timestamp(1_791_450_000 + secs, 0).unwrap(),
            end_time: DateTime::from_timestamp(1_791_450_001 + secs, 0).unwrap(),
            status_code: "UNSET".to_string(),
            status_message: None,
            resource: Value::Object(Default::default()).into(),
            attributes,
            events: json!([]),
        }
    }

    fn count(haystack: &str, needle: &str) -> usize {
        haystack.matches(needle).count()
    }

    #[test]
    fn claude_code_session_is_rendered() {
        let doc = render(&rows(CLAUDE_CODE));
        let t = &doc.text;
        assert!(
            t.starts_with("# Session sess-fixture-1\nagent: claude-code\n"),
            "{t}"
        );
        assert!(t.contains("models: claude-opus-5-5\n"), "{t}");
        assert!(
            t.contains("spans: 5 \u{b7} errors: 1 \u{b7} cost: unknown\n"),
            "{t}"
        );
        assert!(t.contains("user: How many plans do we have"), "{t}");
        assert!(t.contains("tool Grep:"), "{t}");
        assert!(t.contains("ERROR: ripgrep exited with code 2"), "{t}");
        assert!(t.contains("exception: ToolError: regex parse error"), "{t}");
        assert!(t.contains("There are three plans"), "{t}");
        assert_eq!(
            doc.title,
            "How many plans do we have and what are their limits?"
        );
        assert_eq!(doc.metadata["error_count"], 1);
        assert_eq!(doc.metadata["span_count"], 5);
        assert_eq!(doc.metadata["session_id"], "sess-fixture-1");
        assert_eq!(doc.metadata["agent"], "claude-code");
        assert!(doc.metadata["cost_usd"].is_null());
        assert!(doc.metadata["rius_workspace_id"].is_null());
        assert_eq!(doc.metadata["trace_ids"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn sections_follow_time_order_and_skip_plain_spans() {
        let doc = render(&rows(CLAUDE_CODE));
        let headings: Vec<&str> = doc.text.lines().filter(|l| l.starts_with("## ")).collect();
        assert_eq!(headings.len(), 4, "{headings:?}");
        assert!(headings[0].ends_with("chat claude-opus-5-5 [span a1b2c3d4e5f60002]"));
        assert!(headings[1].ends_with(
            "execute_tool Grep [span a1b2c3d4e5f60003] ERROR: ripgrep exited with code 2"
        ));
        assert!(headings[2].ends_with("execute_tool Read [span a1b2c3d4e5f60004]"));
        assert!(headings[3].ends_with("chat claude-opus-5-5 [span a1b2c3d4e5f60005]"));
        assert!(doc.text.contains(
            "tool Read: {\"file_path\":\"src/billing/plans.rs\"} \u{2192} pub enum Plan"
        ));
        // The input order does not matter.
        let mut shuffled = rows(CLAUDE_CODE);
        shuffled.reverse();
        assert_eq!(render(&shuffled), doc);
    }

    #[test]
    fn genai_messages_keep_only_what_is_new() {
        let doc = render(&rows(GENAI));
        let t = &doc.text;
        assert_eq!(count(t, "How many plans do we have?"), 1, "{t}");
        assert_eq!(count(t, "And which plan has the most seats?"), 1, "{t}");
        assert!(
            t.contains("assistant: Team has the most seats, with 25."),
            "{t}"
        );
        assert!(
            t.contains("assistant: We have three plans: Free, Pro and Team."),
            "{t}"
        );
        assert!(t.contains("agent: genai-fixture-app\n"), "{t}");
        assert!(t.contains("models: gpt-5\n"), "{t}");
        assert_eq!(doc.title, "How many plans do we have?");
    }

    fn chat_with_input(history: Value) -> SpanRow {
        span(
            "aaaa000000000001",
            0,
            json!({
                "gen_ai.operation.name": "chat",
                "gen_ai.input.messages": history,
            }),
        )
    }

    #[test]
    fn a_system_message_is_not_the_user_turn() {
        let history = json!([
            { "role": "system", "parts": [{ "type": "text", "content": "You are a careful assistant." }] },
            { "role": "user", "parts": [{ "type": "text", "content": "What is the seat limit?" }] },
        ]);
        let doc = render(&[chat_with_input(history.clone())]);
        assert!(
            doc.text.contains("user: What is the seat limit?"),
            "{}",
            doc.text
        );
        assert!(!doc.text.contains("careful assistant"), "{}", doc.text);
        assert!(!doc.text.contains("system:"), "{}", doc.text);
        assert_eq!(doc.title, "What is the seat limit?");
        // The history may also arrive as a JSON string.
        let as_string = render(&[chat_with_input(Value::String(history.to_string()))]);
        assert_eq!(as_string.text, doc.text);
    }

    #[test]
    fn a_long_system_message_does_not_use_up_the_user_text() {
        let history = json!([
            { "role": "system", "content": "S".repeat(5000) },
            { "role": "user", "content": "Where is the billing code?" },
        ]);
        let doc = render(&[chat_with_input(history)]);
        assert!(
            doc.text.contains("user: Where is the billing code?"),
            "{}",
            doc.text
        );
        assert!(!doc.text.contains("SSSS"), "{}", doc.text);
        assert_eq!(doc.title, "Where is the billing code?");
    }

    #[test]
    fn a_history_cut_at_64_kib_still_shows_a_user_turn() {
        let mut messages = vec![json!({ "role": "system", "content": "Be brief. ".repeat(500) })];
        for n in 0..120 {
            messages.push(
                json!({ "role": "user", "content": format!("question {n} {}", "q".repeat(900)) }),
            );
            messages.push(json!({ "role": "assistant", "content": format!("answer {n} {}", "a".repeat(900)) }));
        }
        messages.push(json!({ "role": "user", "content": "the final question" }));
        let full = Value::Array(messages).to_string();
        assert!(full.len() > 64 * 1024);
        let stored = crate::otlp::clean_string(&full);
        assert!(stored.ends_with(TRUNCATED_SUFFIX));
        assert!(serde_json::from_str::<Value>(&stored).is_err());

        let doc = render(&[chat_with_input(Value::String(stored.clone()))]);
        let last_label = Regex::new(r"question (\d+) ")
            .unwrap()
            .captures_iter(&stored)
            .last()
            .map(|c| c[1].to_string())
            .expect("some question survives the cut");
        assert!(
            doc.text
                .contains(&format!("user: question {last_label} qqq")),
            "{}",
            doc.text
        );
        assert_eq!(count(&doc.text, "user: "), 1, "{}", doc.text);
        for raw in ["\"role\"", "\"content\"", "Be brief", "[truncated]", "{\""] {
            assert!(!doc.text.contains(raw), "{raw} in {}", doc.text);
        }
        assert_eq!(
            doc.title,
            format!("question {last_label} {}", "q".repeat(900))
                .chars()
                .take(80)
                .collect::<String>()
        );
    }

    #[test]
    fn a_cut_history_without_a_readable_user_turn_uses_input_value() {
        let stored = format!(
            "[{{\"role\":\"system\",\"content\":\"{}{TRUNCATED_SUFFIX}",
            "x".repeat(100)
        );
        let mut s = chat_with_input(Value::String(stored));
        s.attributes["input.value"] = json!("typed question");
        let doc = render(&[s]);
        assert!(doc.text.contains("user: typed question"), "{}", doc.text);
        assert!(!doc.text.contains("xxxx"), "{}", doc.text);
    }

    #[test]
    fn salvage_reads_whole_elements_and_the_start_of_the_cut_one() {
        let cut = r#"[{"role":"system","content":"s"},{"role":"user","content":"one"},{"role":"user","parts":[{"type":"text","content":"say \"hi\" \u00"#;
        let items = salvage_messages(&format!("{cut}{TRUNCATED_SUFFIX}"));
        assert_eq!(items.len(), 3, "{items:?}");
        assert_eq!(items[1]["content"], "one");
        assert_eq!(items[2]["role"], "user");
        assert_eq!(items[2]["content"], "say \"hi\" ");
        assert!(salvage_messages("[").is_empty());
        assert!(salvage_messages("[{\"ro").is_empty());
    }

    #[test]
    fn pending_spans_are_left_out() {
        let mut spans = rows(CLAUDE_CODE);
        let mut pending = span(
            "ffff000000000001",
            10,
            json!({
                "glassflow.span.pending": true,
                "gen_ai.operation.name": "chat",
                "gen_ai.request.model": "pending-model",
                "input.value": "unfinished question"
            }),
        );
        pending.session_id = "sess-fixture-1".to_string();
        pending.status_code = "ERROR".to_string();
        spans.push(pending);
        assert_eq!(render(&spans), render(&rows(CLAUDE_CODE)));
        let doc = render(&spans);
        assert!(!doc.text.contains("unfinished"));
        assert!(!doc.text.contains("pending-model"));
        assert_eq!(doc.metadata["span_count"], 5);
    }

    #[test]
    fn cost_is_summed_to_four_decimals() {
        let a = span("a", 0, json!({ "rius.cost_usd": 0.01234 }));
        let b = span("b", 1, json!({ "rius.cost_usd": "0.5" }));
        let doc = render(&[a, b]);
        assert!(doc.text.contains("cost: $0.5123\n"), "{}", doc.text);
        assert_eq!(doc.metadata["cost_usd"], 0.5123);
    }

    #[test]
    fn header_falls_back_to_unknown() {
        let doc = render(&[span("a", 0, json!({}))]);
        assert!(
            doc.text.contains("agent: unknown\nmodels: unknown\n"),
            "{}",
            doc.text
        );
        assert!(doc.text.contains("errors: 0"));
        // No section: not an LLM call, a tool call or an error. The title is the root span.
        assert!(!doc.text.contains("## "));
        assert_eq!(doc.title, "span a");
    }

    #[test]
    fn title_falls_back_to_the_session_id() {
        let mut s = span("a", 0, json!({}));
        s.parent_span_id = Some("0000000000000001".to_string());
        assert_eq!(render(&[s]).title, "Session s1");
    }

    #[test]
    fn title_is_one_line_of_at_most_80_chars() {
        let long = format!("first line\n{}", "x".repeat(200));
        let s = span("a", 0, json!({ "input.value": long }));
        let title = render(&[s]).title;
        assert_eq!(title.chars().count(), 80);
        assert!(title.starts_with("first line xxx"));
    }

    #[test]
    fn long_texts_are_cut() {
        let s = span(
            "a",
            0,
            json!({
                "input.value": "u".repeat(3000),
                "output.value": "m".repeat(3000),
            }),
        );
        let t = render(&[s]).text;
        assert!(t.contains(&format!("user: {}\n", "u".repeat(2000))));
        assert!(t.contains(&format!("assistant: {}\n", "m".repeat(2000))));
        let tool = span(
            "b",
            1,
            json!({
                "gen_ai.tool.name": "T",
                "gen_ai.tool.call.arguments": "a".repeat(900),
                "gen_ai.tool.call.result": "r".repeat(2000),
            }),
        );
        let t = render(&[tool]).text;
        assert!(t.contains(&format!(
            "tool T: {} \u{2192} {}\n",
            "a".repeat(500),
            "r".repeat(1000)
        )));
    }

    #[test]
    fn text_cannot_open_sections_or_fences() {
        let s = span(
            "a",
            0,
            json!({ "input.value": "line\n## fake heading\n```\nopen fence" }),
        );
        let doc = render(&[s]);
        assert_eq!(doc.text.lines().filter(|l| l.starts_with("## ")).count(), 1);
        assert!(!doc.text.lines().any(|l| l.starts_with("```")));
        assert!(doc.text.contains("\n\\## fake heading\n"));
    }

    #[test]
    fn workspace_id_comes_from_the_resource() {
        let mut s = span("a", 0, json!({}));
        s.resource = json!({ "rius.workspace.id": "ws-1", "service.name": "svc" }).into();
        let doc = render(&[s]);
        assert_eq!(doc.metadata["rius_workspace_id"], "ws-1");
        assert_eq!(doc.metadata["agent"], "svc");
    }

    #[test]
    fn content_hash_changes_with_the_text() {
        let a = render(&rows(CLAUDE_CODE));
        let mut b = a.clone();
        assert_eq!(content_hash(&a), content_hash(&b));
        b.text.push('x');
        assert_ne!(content_hash(&a), content_hash(&b));
    }
}
