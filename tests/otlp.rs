mod common;

use std::io::Write;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use common::{body_json, create_tenant_key, test_router, test_state};
use lore::otlp::decode;
use opentelemetry_proto::tonic::collector::trace::v1::ExportTraceServiceResponse;
use prost::Message;
use serde_json::Value;
use sqlx::PgPool;
use tower::ServiceExt;
use uuid::Uuid;

const CLAUDE_CODE: &str = include_str!("fixtures/otlp/claude_code_session.json");
const GENAI: &str = include_str!("fixtures/otlp/otel_genai_session.json");

struct Env {
    router: Router,
    tenant: Uuid,
    key: String,
    pool: PgPool,
    _data: tempfile::TempDir,
}

impl Env {
    async fn new(pool: PgPool) -> Env {
        let data = tempfile::tempdir().unwrap();
        let router = test_router(test_state(pool.clone(), data.path()));
        let (tenant, key) = create_tenant_key(&router, "acme", &["ingest", "read"]).await;
        Env {
            router,
            tenant,
            key,
            pool,
            _data: data,
        }
    }

    async fn post(
        &self,
        key: Option<&str>,
        content_type: &str,
        encoding: Option<&str>,
        body: Vec<u8>,
    ) -> axum::response::Response {
        let mut req = Request::post("/v1/traces").header("content-type", content_type);
        if let Some(key) = key {
            req = req.header("authorization", format!("Bearer {key}"));
        }
        if let Some(enc) = encoding {
            req = req.header("content-encoding", enc);
        }
        let req = req.body(Body::from(body)).unwrap();
        self.router.clone().oneshot(req).await.unwrap()
    }

    async fn post_json(&self, body: &str) -> axum::response::Response {
        self.post(
            Some(&self.key),
            "application/json",
            None,
            body.as_bytes().to_vec(),
        )
        .await
    }

    async fn span_count(&self, tenant: Uuid) -> i64 {
        sqlx::query_scalar("SELECT count(*) FROM spans WHERE tenant_id = $1")
            .bind(tenant)
            .fetch_one(&self.pool)
            .await
            .unwrap()
    }
}

fn protobuf_of(json: &str) -> Vec<u8> {
    decode("application/json", None, json.as_bytes())
        .unwrap()
        .encode_to_vec()
}

fn gzip(bytes: &[u8]) -> Vec<u8> {
    let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    enc.write_all(bytes).unwrap();
    enc.finish().unwrap()
}

#[sqlx::test(migrations = "./migrations")]
async fn json_fixture_is_stored(pool: PgPool) {
    let env = Env::new(pool).await;
    let res = env.post_json(CLAUDE_CODE).await;
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(res.headers()["content-type"], "application/json");
    let bytes = axum::body::to_bytes(res.into_body(), 1024).await.unwrap();
    assert_eq!(&bytes[..], b"{}");

    assert_eq!(env.span_count(env.tenant).await, 5);
    let sessions: Vec<String> =
        sqlx::query_scalar("SELECT DISTINCT session_id FROM spans WHERE tenant_id = $1")
            .bind(env.tenant)
            .fetch_all(&env.pool)
            .await
            .unwrap();
    assert_eq!(sessions, vec!["sess-fixture-1".to_string()]);

    // The root: no parent, INTERNAL, five minutes long, the resource attributes stored.
    let (parent, kind, secs, resource): (Option<String>, String, f64, Value) = sqlx::query_as(
        "SELECT parent_span_id, kind, EXTRACT(EPOCH FROM end_time - start_time)::float8, resource
         FROM spans WHERE tenant_id = $1 AND name = 'claude_code.session'",
    )
    .bind(env.tenant)
    .fetch_one(&env.pool)
    .await
    .unwrap();
    assert_eq!(parent, None);
    assert_eq!(kind, "INTERNAL");
    assert_eq!(secs, 300.0);
    assert_eq!(resource["service.name"], "claude-code");
    assert_eq!(resource["service.instance.id"], "fixture-host");

    // The failing tool call: hex ids, ERROR status, its event, and a child of the root.
    let (trace, parent, status, message, events): (
        String,
        Option<String>,
        String,
        Option<String>,
        Value,
    ) = sqlx::query_as(
        "SELECT trace_id, parent_span_id, status_code, status_message, events
             FROM spans WHERE tenant_id = $1 AND name = 'execute_tool Grep'",
    )
    .bind(env.tenant)
    .fetch_one(&env.pool)
    .await
    .unwrap();
    assert_eq!(trace, "5b8efff798038103d269b633813fc60c");
    assert_eq!(parent.as_deref(), Some("a1b2c3d4e5f60001"));
    assert_eq!(status, "ERROR");
    assert_eq!(message.as_deref(), Some("ripgrep exited with code 2"));
    assert_eq!(events[0]["name"], "exception");
    assert_eq!(events[0]["attributes"]["exception.type"], "ToolError");

    // Ints are JSON numbers.
    let tokens: i64 = sqlx::query_scalar(
        "SELECT (attributes->>'gen_ai.usage.input_tokens')::bigint FROM spans
         WHERE tenant_id = $1 AND attributes->>'gen_ai.usage.input_tokens' IS NOT NULL",
    )
    .bind(env.tenant)
    .fetch_one(&env.pool)
    .await
    .unwrap();
    assert_eq!(tokens, 1200);
    let kind: String = sqlx::query_scalar(
        "SELECT jsonb_typeof(attributes->'gen_ai.usage.input_tokens') FROM spans
         WHERE tenant_id = $1 AND attributes ? 'gen_ai.usage.input_tokens'",
    )
    .bind(env.tenant)
    .fetch_one(&env.pool)
    .await
    .unwrap();
    assert_eq!(kind, "number");

    // The tenant's traces source exists.
    let sources: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM sources WHERE tenant_id = $1 AND kind = 'traces' AND name = 'otlp'",
    )
    .bind(env.tenant)
    .fetch_one(&env.pool)
    .await
    .unwrap();
    assert_eq!(sources, 1);
}

#[sqlx::test(migrations = "./migrations")]
async fn protobuf_and_gzip_are_idempotent(pool: PgPool) {
    let env = Env::new(pool).await;
    assert_eq!(env.post_json(CLAUDE_CODE).await.status(), StatusCode::OK);

    let proto = protobuf_of(CLAUDE_CODE);
    let res = env
        .post(
            Some(&env.key),
            "application/x-protobuf",
            None,
            proto.clone(),
        )
        .await;
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(res.headers()["content-type"], "application/x-protobuf");
    let bytes = axum::body::to_bytes(res.into_body(), 1024).await.unwrap();
    assert!(
        bytes.is_empty(),
        "an empty ExportTraceServiceResponse is zero bytes"
    );
    ExportTraceServiceResponse::decode(&bytes[..]).unwrap();
    assert_eq!(env.span_count(env.tenant).await, 5);

    let res = env
        .post(
            Some(&env.key),
            "application/x-protobuf",
            Some("gzip"),
            gzip(&proto),
        )
        .await;
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(env.span_count(env.tenant).await, 5);

    // gzip works for JSON too.
    let res = env
        .post(
            Some(&env.key),
            "application/json; charset=utf-8",
            Some("gzip"),
            gzip(CLAUDE_CODE.as_bytes()),
        )
        .await;
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(res.headers()["content-type"], "application/json");
    assert_eq!(env.span_count(env.tenant).await, 5);
}

#[sqlx::test(migrations = "./migrations")]
async fn a_resent_span_is_updated_in_place(pool: PgPool) {
    let env = Env::new(pool).await;
    assert_eq!(env.post_json(CLAUDE_CODE).await.status(), StatusCode::OK);
    let changed = CLAUDE_CODE.replace("ripgrep exited with code 2", "changed message");
    assert_eq!(env.post_json(&changed).await.status(), StatusCode::OK);
    let message: String = sqlx::query_scalar(
        "SELECT status_message FROM spans WHERE tenant_id = $1 AND name = 'execute_tool Grep'",
    )
    .bind(env.tenant)
    .fetch_one(&env.pool)
    .await
    .unwrap();
    assert_eq!(message, "changed message");
    assert_eq!(env.span_count(env.tenant).await, 5);
}

#[sqlx::test(migrations = "./migrations")]
async fn sessions_are_queued_once(pool: PgPool) {
    let env = Env::new(pool).await;
    for _ in 0..2 {
        assert_eq!(env.post_json(CLAUDE_CODE).await.status(), StatusCode::OK);
    }
    assert_eq!(env.post_json(GENAI).await.status(), StatusCode::OK);

    let queued: Vec<String> = sqlx::query_scalar(
        "SELECT session_id FROM session_queue WHERE tenant_id = $1 ORDER BY session_id",
    )
    .bind(env.tenant)
    .fetch_all(&env.pool)
    .await
    .unwrap();
    assert_eq!(queued, vec!["sess-fixture-1", "sess-fixture-2"]);
    assert_eq!(env.span_count(env.tenant).await, 7);
}

#[sqlx::test(migrations = "./migrations")]
async fn a_resent_session_moves_its_queue_time_forward(pool: PgPool) {
    let env = Env::new(pool).await;
    assert_eq!(env.post_json(CLAUDE_CODE).await.status(), StatusCode::OK);
    sqlx::query(
        "UPDATE session_queue SET last_span_at = now() - interval '1 hour' WHERE tenant_id = $1",
    )
    .bind(env.tenant)
    .execute(&env.pool)
    .await
    .unwrap();

    assert_eq!(env.post_json(CLAUDE_CODE).await.status(), StatusCode::OK);

    let age_secs: f64 = sqlx::query_scalar(
        "SELECT extract(epoch FROM now() - last_span_at)::float8 FROM session_queue
         WHERE tenant_id = $1 AND session_id = 'sess-fixture-1'",
    )
    .bind(env.tenant)
    .fetch_one(&env.pool)
    .await
    .unwrap();
    assert!(
        age_secs < 60.0,
        "last_span_at was not refreshed: {age_secs}s old"
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn genai_fixture_keeps_message_strings(pool: PgPool) {
    let env = Env::new(pool).await;
    assert_eq!(env.post_json(GENAI).await.status(), StatusCode::OK);
    let inputs: Vec<String> = sqlx::query_scalar(
        "SELECT attributes->>'gen_ai.input.messages' FROM spans
         WHERE tenant_id = $1 ORDER BY start_time",
    )
    .bind(env.tenant)
    .fetch_all(&env.pool)
    .await
    .unwrap();
    assert_eq!(inputs.len(), 2);
    let first: Value = serde_json::from_str(&inputs[0]).unwrap();
    let second: Value = serde_json::from_str(&inputs[1]).unwrap();
    assert_eq!(first.as_array().unwrap().len(), 1);
    assert_eq!(second.as_array().unwrap().len(), 3);
    assert_eq!(second[0], first[0], "the second turn repeats the first");
    assert_eq!(second[1]["role"], "assistant");
    assert_eq!(
        second[2]["parts"][0]["content"],
        "And which plan has the most seats?"
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn tenants_do_not_share_spans(pool: PgPool) {
    let env = Env::new(pool).await;
    let (other, other_key) = create_tenant_key(&env.router, "other", &["ingest"]).await;
    assert_eq!(env.post_json(CLAUDE_CODE).await.status(), StatusCode::OK);
    let res = env
        .post(
            Some(&other_key),
            "application/json",
            None,
            CLAUDE_CODE.as_bytes().to_vec(),
        )
        .await;
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(env.span_count(env.tenant).await, 5);
    assert_eq!(env.span_count(other).await, 5);
}

#[sqlx::test(migrations = "./migrations")]
async fn bad_requests(pool: PgPool) {
    let env = Env::new(pool).await;
    let body = CLAUDE_CODE.as_bytes().to_vec();

    let res = env
        .post(Some(&env.key), "text/plain", None, body.clone())
        .await;
    assert_eq!(res.status(), StatusCode::UNSUPPORTED_MEDIA_TYPE);
    let res = env
        .post(Some(&env.key), "application/json", Some("br"), body.clone())
        .await;
    assert_eq!(res.status(), StatusCode::UNSUPPORTED_MEDIA_TYPE);

    for (ct, enc, bad) in [
        ("application/json", None, b"{not json".to_vec()),
        ("application/x-protobuf", None, vec![0xFF, 0xFF, 0xFF]),
        ("application/json", Some("gzip"), body.clone()),
    ] {
        let res = env.post(Some(&env.key), ct, enc, bad).await;
        assert_eq!(res.status(), StatusCode::BAD_REQUEST, "{ct} {enc:?}");
        let v = body_json(res).await;
        assert!(v["error"].is_string());
    }
    assert_eq!(env.span_count(env.tenant).await, 0);
}

#[sqlx::test(migrations = "./migrations")]
async fn auth_is_enforced(pool: PgPool) {
    let env = Env::new(pool).await;
    let (_, read_key) = create_tenant_key(&env.router, "reader", &["read"]).await;
    let body = CLAUDE_CODE.as_bytes().to_vec();

    let res = env
        .post(Some(&read_key), "application/json", None, body.clone())
        .await;
    assert_eq!(res.status(), StatusCode::FORBIDDEN);
    let res = env.post(None, "application/json", None, body.clone()).await;
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
    let res = env
        .post(Some("lk_nope"), "application/json", None, body)
        .await;
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
}

#[sqlx::test(migrations = "./migrations")]
async fn gzip_over_64_mib_is_a_413(pool: PgPool) {
    let env = Env::new(pool).await;
    // A run of zeros compresses to about 64 KiB.
    let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    let zeros = vec![0u8; 1024 * 1024];
    for _ in 0..65 {
        enc.write_all(&zeros).unwrap();
    }
    let zipped = enc.finish().unwrap();
    assert!(zipped.len() < 16 * 1024 * 1024);

    let res = env
        .post(
            Some(&env.key),
            "application/x-protobuf",
            Some("gzip"),
            zipped,
        )
        .await;
    assert_eq!(res.status(), StatusCode::PAYLOAD_TOO_LARGE);
    let v = body_json(res).await;
    assert_eq!(v["error"], "payload too large");
}

#[sqlx::test(migrations = "./migrations")]
async fn a_body_over_16_mib_is_a_413(pool: PgPool) {
    let env = Env::new(pool).await;
    let res = env
        .post(
            Some(&env.key),
            "application/x-protobuf",
            None,
            vec![0u8; 16 * 1024 * 1024 + 1],
        )
        .await;
    assert_eq!(res.status(), StatusCode::PAYLOAD_TOO_LARGE);
}

#[sqlx::test(migrations = "./migrations")]
async fn odd_but_valid_spans_are_stored(pool: PgPool) {
    let env = Env::new(pool).await;
    // The same span twice in one request, a NUL in a string, a span with no parent, no status,
    // no session (so the session is the trace id), and a 70 KB string.
    let long = "x".repeat(70 * 1024);
    let body = format!(
        r#"{{"resourceSpans":[{{"scopeSpans":[{{"spans":[
        {{"traceId":"0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f","spanId":"1111111111111111","name":"first","kind":2,
          "startTimeUnixNano":"1791450000000000000","endTimeUnixNano":"1791450001000000000"}},
        {{"traceId":"0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f","spanId":"1111111111111111","name":"second\u0000","kind":2,
          "startTimeUnixNano":"1791450000000000000","endTimeUnixNano":"1791450001000000000",
          "attributes":[{{"key":"big","value":{{"stringValue":"{long}"}}}}]}}
        ]}}]}}]}}"#
    );
    let res = env.post_json(&body).await;
    assert_eq!(res.status(), StatusCode::OK);
    let (name, kind, status, session, big): (String, String, String, String, String) =
        sqlx::query_as(
            "SELECT name, kind, status_code, session_id, attributes->>'big' FROM spans
             WHERE tenant_id = $1",
        )
        .bind(env.tenant)
        .fetch_one(&env.pool)
        .await
        .unwrap();
    assert_eq!(
        name, "second\u{FFFD}",
        "the last copy wins and the NUL is replaced"
    );
    assert_eq!(kind, "SERVER");
    assert_eq!(status, "UNSET");
    assert_eq!(session, "0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f");
    assert!(big.ends_with("\u{2026}[truncated]"));
    assert_eq!(big.len(), 64 * 1024 + "\u{2026}[truncated]".len());
}
