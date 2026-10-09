mod common;

use axum::Router;
use axum::http::StatusCode;
use common::{create_tenant_key, send, test_router, test_state};
use serde_json::{Value, json};
use sqlx::PgPool;

const NOTE: &str = "The quarterly zorblax review moved to Thursdays.";

struct Env {
    router: Router,
    key: String,
    _data: tempfile::TempDir,
}

impl Env {
    async fn new(pool: PgPool) -> Env {
        let data = tempfile::tempdir().unwrap();
        let router = test_router(test_state(pool, data.path()));
        let (_, key) = create_tenant_key(&router, "acme", &["ingest", "read"]).await;
        Env {
            router,
            key,
            _data: data,
        }
    }

    async fn call(&self, method: &str, uri: &str, key: &str, body: Value) -> (StatusCode, Value) {
        let bytes = if body.is_null() {
            Vec::new()
        } else {
            body.to_string().into_bytes()
        };
        send(&self.router, method, uri, key, bytes).await
    }

    async fn put(&self, body: Value) -> (StatusCode, Value) {
        self.call("POST", "/v1/documents", &self.key, body).await
    }

    async fn hits(&self, q: &str) -> Vec<Value> {
        let (status, body) = self
            .call("GET", &format!("/v1/search?q={q}"), &self.key, Value::Null)
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        body["hits"].as_array().unwrap().clone()
    }
}

fn one(external_id: &str, text: &str) -> Value {
    json!({
        "source": "notes",
        "documents": [{ "external_id": external_id, "title": "Review", "text": text }]
    })
}

fn zorblax() -> Value {
    one("n1", NOTE)
}

#[sqlx::test(migrations = "./migrations")]
async fn a_note_is_searchable_and_unchanged_on_a_second_put(pool: PgPool) {
    let env = Env::new(pool).await;
    let (status, body) = env.put(zorblax()).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["upserted"], 1);
    assert_eq!(body["unchanged"], 0);
    assert_eq!(body["chunks"], 1);

    let hits = env.hits("zorblax").await;
    assert_eq!(hits[0]["kind"], "note");
    assert_eq!(hits[0]["source"], "notes");
    assert_eq!(hits[0]["title"], "Review");

    let (_, body) = env.put(zorblax()).await;
    assert_eq!(body["upserted"], 0);
    assert_eq!(body["unchanged"], 1);
    assert_eq!(body["chunks"], 0);
}

#[sqlx::test(migrations = "./migrations")]
async fn changed_text_is_indexed_again(pool: PgPool) {
    let env = Env::new(pool).await;
    env.put(zorblax()).await;
    let (_, body) = env
        .put(one("n1", "Now the quibblefrap review is on Fridays."))
        .await;
    assert_eq!(body["upserted"], 1);
    assert_eq!(body["unchanged"], 0);

    assert!(
        env.hits("quibblefrap").await[0]["snippet"]
            .as_str()
            .unwrap()
            .contains("quibblefrap")
    );
    // The vector side still returns the note, but its old text is gone.
    for hit in env.hits("zorblax").await {
        assert!(!hit["snippet"].as_str().unwrap().contains("zorblax"));
    }
}

#[sqlx::test(migrations = "./migrations")]
async fn a_changed_title_or_kind_is_written_too(pool: PgPool) {
    let env = Env::new(pool).await;
    env.put(zorblax()).await;
    let mut body = zorblax();
    body["documents"][0]["title"] = json!("Renamed");
    body["documents"][0]["kind"] = json!("doc");
    let (_, report) = env.put(body).await;
    assert_eq!(report["upserted"], 1);

    let hits = env.hits("zorblax").await;
    assert_eq!(hits[0]["title"], "Renamed");
    assert_eq!(hits[0]["kind"], "doc");
}

#[sqlx::test(migrations = "./migrations")]
async fn deleting_a_note_removes_it_from_search(pool: PgPool) {
    let env = Env::new(pool).await;
    env.put(zorblax()).await;
    assert_eq!(env.hits("zorblax").await.len(), 1);

    let (status, _) = env
        .call("DELETE", "/v1/documents/notes/n1", &env.key, Value::Null)
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert!(env.hits("zorblax").await.is_empty());

    let (status, _) = env
        .call("DELETE", "/v1/documents/notes/n1", &env.key, Value::Null)
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[sqlx::test(migrations = "./migrations")]
async fn deleting_something_that_is_not_there_is_404(pool: PgPool) {
    let env = Env::new(pool).await;
    env.put(zorblax()).await;
    for uri in [
        "/v1/documents/notes/other",
        "/v1/documents/no-such-source/n1",
    ] {
        let (status, _) = env.call("DELETE", uri, &env.key, Value::Null).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{uri}");
    }
    assert_eq!(env.hits("zorblax").await.len(), 1);
}

#[sqlx::test(migrations = "./migrations")]
async fn an_external_id_with_slashes_can_be_deleted(pool: PgPool) {
    let env = Env::new(pool).await;
    env.put(one("team/q3 plan/a", NOTE)).await;
    let (status, _) = env
        .call(
            "DELETE",
            "/v1/documents/notes/team/q3%20plan/a",
            &env.key,
            Value::Null,
        )
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert!(env.hits("zorblax").await.is_empty());
}

#[sqlx::test(migrations = "./migrations")]
async fn get_returns_the_document_and_hides_it_from_other_tenants(pool: PgPool) {
    let env = Env::new(pool).await;
    let mut body = zorblax();
    body["documents"][0]["uri"] = json!("https://example.com/n1");
    body["documents"][0]["metadata"] = json!({ "team": "ops" });
    env.put(body).await;
    let id = env.hits("zorblax").await[0]["document_id"]
        .as_str()
        .unwrap()
        .to_string();

    let (status, doc) = env
        .call("GET", &format!("/v1/documents/{id}"), &env.key, Value::Null)
        .await;
    assert_eq!(status, StatusCode::OK, "{doc}");
    assert_eq!(
        doc,
        json!({
            "id": id,
            "kind": "note",
            "title": "Review",
            "uri": "https://example.com/n1",
            "metadata": { "team": "ops" },
            "text": NOTE,
        })
    );

    let (_, other) = create_tenant_key(&env.router, "globex", &["ingest", "read"]).await;
    let (status, _) = env
        .call("GET", &format!("/v1/documents/{id}"), &other, Value::Null)
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    // The other tenant cannot delete it either, even with the same names.
    let (status, _) = env
        .call("DELETE", "/v1/documents/notes/n1", &other, Value::Null)
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(env.hits("zorblax").await.len(), 1);
}

#[sqlx::test(migrations = "./migrations")]
async fn get_of_a_long_document_has_no_repeated_overlap_lines(pool: PgPool) {
    let env = Env::new(pool).await;
    let text = format!(
        "# Plan\n{}",
        (1..=300)
            .map(|n| format!("item {n}"))
            .collect::<Vec<_>>()
            .join("\n")
    );
    let (_, report) = env.put(one("long", &text)).await;
    assert!(report["chunks"].as_u64().unwrap() > 2, "{report}");

    let hits = env.hits("item").await;
    let id = hits[0]["document_id"].as_str().unwrap();
    let (_, doc) = env
        .call("GET", &format!("/v1/documents/{id}"), &env.key, Value::Null)
        .await;
    assert_eq!(doc["text"], text);
}

#[sqlx::test(migrations = "./migrations")]
async fn get_cuts_the_text_at_100000_characters(pool: PgPool) {
    let env = Env::new(pool).await;
    // 30,000 lines of 9 characters is far more than the cut, and stays in the chunk limits.
    let text = "0123456789\n".repeat(30_000);
    env.put(one("big", &text)).await;
    let id = env.hits("0123456789").await[0]["document_id"]
        .as_str()
        .unwrap()
        .to_string();
    let (status, doc) = env
        .call("GET", &format!("/v1/documents/{id}"), &env.key, Value::Null)
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(doc["text"].as_str().unwrap().chars().count(), 100_000);
}

#[sqlx::test(migrations = "./migrations")]
async fn bad_requests_are_400(pool: PgPool) {
    let env = Env::new(pool).await;
    let doc = |extra: Value| {
        let mut d = json!({ "external_id": "n1", "title": "T", "text": "x" });
        d.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        d
    };
    let hundred_and_one: Vec<Value> = (0..101)
        .map(|i| json!({ "external_id": format!("d{i}"), "title": "T", "text": "x" }))
        .collect();
    let cases = [
        (
            "bad kind",
            json!({ "source": "s", "documents": [doc(json!({ "kind": "session" }))] }),
        ),
        ("empty documents", json!({ "source": "s", "documents": [] })),
        (
            "101 documents",
            json!({ "source": "s", "documents": hundred_and_one }),
        ),
        (
            "empty source",
            json!({ "source": "", "documents": [doc(json!({}))] }),
        ),
        (
            "bad source",
            json!({ "source": "a b", "documents": [doc(json!({}))] }),
        ),
        (
            "slash in source",
            json!({ "source": "a/b", "documents": [doc(json!({}))] }),
        ),
        (
            "long source",
            json!({ "source": "s".repeat(101), "documents": [doc(json!({}))] }),
        ),
        (
            "empty id",
            json!({ "source": "s", "documents": [doc(json!({ "external_id": "" }))] }),
        ),
        (
            "empty title",
            json!({ "source": "s", "documents": [doc(json!({ "title": " " }))] }),
        ),
        (
            "metadata not an object",
            json!({ "source": "s", "documents": [doc(json!({ "metadata": [1] }))] }),
        ),
        (
            "NUL in text",
            json!({ "source": "s", "documents": [doc(json!({ "text": "a\u{0}b" }))] }),
        ),
        (
            "text over 1 MiB",
            json!({ "source": "s", "documents": [doc(json!({ "text": "x".repeat(1024 * 1024 + 1) }))] }),
        ),
        ("missing field", json!({ "source": "s" })),
    ];
    for (name, body) in cases {
        let (status, reply) = env.put(body).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{name}: {reply}");
    }

    // Text of exactly 1 MiB is fine.
    let (status, reply) = env.put(one("max", &"x".repeat(1024 * 1024))).await;
    assert_eq!(status, StatusCode::OK, "{reply}");

    // One bad document leaves the good one of the same request unwritten.
    let (status, _) = env
        .put(
            json!({ "source": "pair", "documents": [doc(json!({})), doc(json!({ "kind": "x" }))] }),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(env.hits("x").await.iter().all(|h| h["source"] != "pair"));
}

#[sqlx::test(migrations = "./migrations")]
async fn a_body_that_is_not_json_is_400(pool: PgPool) {
    let env = Env::new(pool).await;
    let (status, _) = send(
        &env.router,
        "POST",
        "/v1/documents",
        &env.key,
        b"nope".to_vec(),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[sqlx::test(migrations = "./migrations")]
async fn the_same_external_id_twice_in_one_request_keeps_the_last(pool: PgPool) {
    let env = Env::new(pool).await;
    let body = json!({
        "source": "notes",
        "documents": [
            { "external_id": "n1", "title": "First", "text": "alpha wombat" },
            { "external_id": "n1", "title": "Second", "text": "beta wombat" },
        ]
    });
    let (status, report) = env.put(body).await;
    assert_eq!(status, StatusCode::OK, "{report}");
    assert_eq!(report["upserted"], 2);

    let hits = env.hits("wombat").await;
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0]["title"], "Second");
}

#[sqlx::test(migrations = "./migrations")]
async fn scopes_are_enforced(pool: PgPool) {
    let env = Env::new(pool).await;
    env.put(zorblax()).await;
    let (_, read_only) = create_tenant_key(&env.router, "reader", &["read"]).await;
    let (_, ingest_only) = create_tenant_key(&env.router, "writer", &["ingest"]).await;

    let (status, _) = env
        .call("POST", "/v1/documents", &read_only, zorblax())
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _) = env
        .call("DELETE", "/v1/documents/notes/n1", &read_only, Value::Null)
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let id = env.hits("zorblax").await[0]["document_id"]
        .as_str()
        .unwrap()
        .to_string();
    let (status, _) = env
        .call(
            "GET",
            &format!("/v1/documents/{id}"),
            &ingest_only,
            Value::Null,
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    let (status, _) = send(
        &env.router,
        "POST",
        "/v1/documents",
        "lk_wrong",
        zorblax().to_string().into_bytes(),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[sqlx::test(migrations = "./migrations")]
async fn the_same_ids_in_two_tenants_do_not_collide(pool: PgPool) {
    let env = Env::new(pool).await;
    let (_, other) = create_tenant_key(&env.router, "globex", &["ingest", "read"]).await;
    env.put(zorblax()).await;
    let (_, report) = env.call("POST", "/v1/documents", &other, zorblax()).await;
    assert_eq!(report["upserted"], 1);
    assert_eq!(report["unchanged"], 0);
}
