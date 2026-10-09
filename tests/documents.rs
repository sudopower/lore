mod common;

use axum::Router;
use axum::body::{Body, Bytes};
use axum::http::{Request, StatusCode};
use common::{body_json, create_tenant_key, push_fixture, send, test_router, test_state};
use lore::chunk::{chunk_markdown_split, embed_text};
use lore::embed::{Embedder, HashEmbedder};
use lore::http::documents::BODY_LIMIT;
use pgvector::Vector;
use serde_json::{Value, json};
use sqlx::PgPool;
use tower::ServiceExt;

const NOTE: &str = "The quarterly zorblax review moved to Thursdays.";

struct Env {
    pool: PgPool,
    router: Router,
    key: String,
    _data: tempfile::TempDir,
}

impl Env {
    async fn new(pool: PgPool) -> Env {
        let data = tempfile::tempdir().unwrap();
        let router = test_router(test_state(pool.clone(), data.path()));
        let (_, key) = create_tenant_key(&router, "acme", &["ingest", "read"]).await;
        Env {
            pool,
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

impl Env {
    /// The id of the document with this external id, read from the database.
    async fn id_of(&self, external_id: &str) -> String {
        let id: uuid::Uuid = sqlx::query_scalar("SELECT id FROM documents WHERE external_id = $1")
            .bind(external_id)
            .fetch_one(&self.pool)
            .await
            .unwrap();
        id.to_string()
    }

    /// What `GET /v1/documents/{id}` says the text of this document is.
    async fn text_of(&self, external_id: &str) -> String {
        let id = self.id_of(external_id).await;
        let (status, doc) = self
            .call(
                "GET",
                &format!("/v1/documents/{id}"),
                &self.key,
                Value::Null,
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{doc}");
        doc["text"].as_str().unwrap().to_string()
    }

    /// The texts of all chunks of the document, in order.
    async fn chunk_texts(&self, external_id: &str) -> Vec<String> {
        sqlx::query_scalar(
            "SELECT c.text FROM chunks c JOIN documents d ON d.id = c.document_id
             WHERE d.external_id = $1 ORDER BY c.ord",
        )
        .bind(external_id)
        .fetch_all(&self.pool)
        .await
        .unwrap()
    }

    async fn count(&self, table: &str) -> i64 {
        sqlx::query_scalar(&format!("SELECT count(*) FROM {table}"))
            .fetch_one(&self.pool)
            .await
            .unwrap()
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

const MIB: usize = 1024 * 1024;

fn doc_with(external_id: &str, extra: Value) -> Value {
    let mut d = json!({ "external_id": external_id, "title": "T", "text": "x" });
    d.as_object_mut()
        .unwrap()
        .extend(extra.as_object().unwrap().clone());
    d
}

fn request(source: &str, doc: Value) -> Value {
    json!({ "source": source, "documents": [doc] })
}

// F1: limits on title, metadata and chunk count

#[sqlx::test(migrations = "./migrations")]
async fn the_title_is_limited_to_1024_bytes(pool: PgPool) {
    let env = Env::new(pool).await;
    let at = request("s", doc_with("a", json!({ "title": "t".repeat(1024) })));
    let (status, reply) = env.put(at).await;
    assert_eq!(status, StatusCode::OK, "{reply}");

    let over = request("s", doc_with("b", json!({ "title": "t".repeat(1025) })));
    let (status, reply) = env.put(over).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{reply}");
    assert!(reply["error"].as_str().unwrap().contains("1024"), "{reply}");

    // Bytes count, not characters: 513 two-byte characters are 1026 bytes.
    let wide = request("s", doc_with("c", json!({ "title": "\u{e9}".repeat(513) })));
    let (status, _) = env.put(wide).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(env.count("documents").await, 1);
}

#[sqlx::test(migrations = "./migrations")]
async fn the_uri_is_limited_to_2048_bytes_and_nothing_of_the_request_is_written(pool: PgPool) {
    let env = Env::new(pool).await;
    let at = request("s", doc_with("a", json!({ "uri": "u".repeat(2048) })));
    let (status, reply) = env.put(at).await;
    assert_eq!(status, StatusCode::OK, "{reply}");
    for table in ["documents", "chunks", "sources"] {
        assert_eq!(env.count(table).await, 1, "{table}");
    }

    // A good document before and after the bad one: none of the three is written.
    for bad_uri in ["u".repeat(2049), "\u{e9}".repeat(1025)] {
        let body = json!({
            "source": "other",
            "documents": [
                doc_with("first", json!({})),
                doc_with("bad", json!({ "uri": bad_uri })),
                doc_with("last", json!({})),
            ]
        });
        let (status, reply) = env.put(body).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{reply}");
        let message = reply["error"].as_str().unwrap();
        assert!(
            message.contains("documents[1].uri") && message.contains("2048"),
            "{message}"
        );
        for table in ["documents", "chunks", "sources"] {
            assert_eq!(env.count(table).await, 1, "{table}");
        }
    }
}

#[sqlx::test(migrations = "./migrations")]
async fn the_metadata_is_limited_to_64_kib(pool: PgPool) {
    let env = Env::new(pool).await;
    // `{"k":"..."}` is 8 bytes around the value.
    let metadata = |n: usize| json!({ "k": "m".repeat(n) });
    let (status, reply) = env
        .put(request(
            "s",
            doc_with("a", json!({ "metadata": metadata(64 * 1024 - 8) })),
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{reply}");

    let (status, reply) = env
        .put(request(
            "s",
            doc_with("b", json!({ "metadata": metadata(64 * 1024 - 7) })),
        ))
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{reply}");
    assert!(
        reply["error"].as_str().unwrap().contains("64 KiB"),
        "{reply}"
    );
    assert_eq!(env.count("documents").await, 1);
}

#[sqlx::test(migrations = "./migrations")]
async fn metadata_path_is_limited_to_2048_bytes_and_comes_back_whole_at_the_limit(pool: PgPool) {
    let env = Env::new(pool).await;
    let note =
        |id: &str, path: Value| doc_with(id, json!({ "text": NOTE, "metadata": { "path": path } }));
    let whole = "p".repeat(2048);
    let (status, reply) = env.put(request("s", note("a", json!(whole)))).await;
    assert_eq!(status, StatusCode::OK, "{reply}");
    let hits = env.hits("zorblax").await;
    assert_eq!(hits[0]["path"], json!(whole));

    for bad in [
        json!("p".repeat(60 * 1024)),
        json!("\u{e9}".repeat(1025)),
        json!(["x".repeat(2048)]),
    ] {
        let body = json!({
            "source": "other",
            "documents": [doc_with("first", json!({})), note("bad", bad)],
        });
        let (status, reply) = env.put(body).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{reply}");
        let message = reply["error"].as_str().unwrap();
        assert!(
            message.contains("documents[1].metadata.path") && message.contains("2048"),
            "{message}"
        );
        assert_eq!(env.count("documents").await, 1);
        assert_eq!(env.count("sources").await, 1);
    }
}

#[sqlx::test(migrations = "./migrations")]
async fn search_cuts_a_stored_path_to_2048_bytes_on_a_char_boundary(pool: PgPool) {
    let env = Env::new(pool).await;
    let (status, reply) = env.put(one("n1", NOTE)).await;
    assert_eq!(status, StatusCode::OK, "{reply}");
    let set_path = |path: String| {
        let pool = env.pool.clone();
        async move {
            sqlx::query("UPDATE documents SET metadata = jsonb_build_object('path', $1::text)")
                .bind(path)
                .execute(&pool)
                .await
                .unwrap();
        }
    };

    // Paths stored before the limit existed can be as long as the metadata allows.
    set_path("p".repeat(60 * 1024)).await;
    let hits = env.hits("zorblax").await;
    assert_eq!(hits[0]["path"], json!("p".repeat(2048)));

    // 'a' then 2-byte characters: byte 2048 falls inside a character, so the cut is at 2047.
    set_path(format!("a{}", "\u{e9}".repeat(1500))).await;
    let hits = env.hits("zorblax").await;
    let path = hits[0]["path"].as_str().unwrap();
    assert_eq!(path.len(), 2047);
    assert!(path.starts_with("a\u{e9}\u{e9}") && path.ends_with('\u{e9}'));

    // 4-byte characters: 512 of them are exactly 2048 bytes.
    set_path("\u{1F600}".repeat(700)).await;
    let hits = env.hits("zorblax").await;
    assert_eq!(hits[0]["path"], json!("\u{1F600}".repeat(512)));

    set_path("src/lib.rs".to_string()).await;
    let hits = env.hits("zorblax").await;
    assert_eq!(hits[0]["path"], json!("src/lib.rs"));
}

#[sqlx::test(migrations = "./migrations")]
async fn more_than_1000_chunks_is_400_and_nothing_of_the_request_is_written(pool: PgPool) {
    let env = Env::new(pool).await;
    let body = json!({
        "source": "pair",
        "documents": [
            doc_with("good", json!({})),
            doc_with("bad", json!({ "text": "#\n".repeat(1001) })),
        ]
    });
    let (status, reply) = env.put(body).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{reply}");
    let message = reply["error"].as_str().unwrap();
    assert!(
        message.contains("1000") && message.contains("chunks"),
        "{message}"
    );
    for table in ["documents", "chunks", "sources"] {
        assert_eq!(env.count(table).await, 0, "{table}");
    }

    // A whole MiB of bare headings, which would be half a million chunks, is refused too.
    let text = "#\n".repeat(MIB / 2);
    assert_eq!(text.len(), MIB);
    let (status, reply) = env
        .put(request("s", doc_with("huge", json!({ "text": text }))))
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{reply}");
    assert_eq!(env.count("documents").await, 0);

    // Exactly 1000 is fine.
    let (status, reply) = env
        .put(request(
            "s",
            doc_with("limit", json!({ "text": "#\n".repeat(1000) })),
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{reply}");
    assert_eq!(reply["chunks"], 1000);
    assert_eq!(env.count("chunks").await, 1000);
}

#[sqlx::test(migrations = "./migrations")]
async fn the_chunk_limit_counts_the_pieces_of_a_split_line(pool: PgPool) {
    let env = Env::new(pool).await;
    let line = "z".repeat(20_000);
    let text = |headings: usize| format!("{}{line}", "#\n".repeat(headings));
    let count = |headings: usize| {
        chunk_markdown_split(&text(headings), "T", usize::MAX)
            .unwrap()
            .len()
    };
    // The long line adds a fixed number of chunks (three pieces and perhaps a heading window).
    let extra = count(0);
    assert!(extra >= 3, "{extra}");
    let fits = 1000 - extra;
    assert_eq!(count(fits), 1000);

    let (status, reply) = env
        .put(request(
            "s",
            doc_with("fits", json!({ "text": text(fits) })),
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{reply}");
    assert_eq!(reply["chunks"], 1000);

    let (status, reply) = env
        .put(request(
            "s",
            doc_with("over", json!({ "text": text(fits + 1) })),
        ))
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{reply}");
    assert_eq!(env.count("documents").await, 1);
}

// F2: all accepted text is in some searchable chunk

#[sqlx::test(migrations = "./migrations")]
async fn the_end_of_a_very_long_single_line_is_searchable(pool: PgPool) {
    let env = Env::new(pool).await;
    let words: Vec<String> = (0..4000).map(|i| format!("w{i}")).collect();
    let line = format!("{} farendword", words.join(" "));
    assert!(line.chars().count() > 20_000);
    let (status, reply) = env.put(one("wide", &line)).await;
    assert_eq!(status, StatusCode::OK, "{reply}");
    assert!(reply["chunks"].as_u64().unwrap() >= 3, "{reply}");

    // The word is at the very end of the line, past the old 8000 character cut.
    let in_index: Vec<String> = sqlx::query_scalar(
        "SELECT text FROM chunks WHERE tsv @@ plainto_tsquery('english', 'farendword')",
    )
    .fetch_all(&env.pool)
    .await
    .unwrap();
    assert_eq!(in_index.len(), 1);
    assert!(in_index[0].ends_with("farendword"));
    let hits = env.hits("farendword").await;
    assert!(
        hits.iter()
            .any(|h| h["snippet"].as_str().unwrap().contains("farendword")),
        "{hits:?}"
    );

    for text in env.chunk_texts("wide").await {
        assert!(text.chars().count() <= 8000);
    }
    assert_eq!(env.text_of("wide").await, line);
}

#[sqlx::test(migrations = "./migrations")]
async fn every_line_of_a_long_section_is_in_some_chunk(pool: PgPool) {
    let env = Env::new(pool).await;
    let rows: Vec<String> = (0..300)
        .map(|i| format!("row{i:03} {}", "x".repeat(93)))
        .collect();
    assert!(rows.iter().all(|r| r.len() == 100));
    let text = rows.join("\n");
    let (status, reply) = env.put(one("rows", &text)).await;
    assert_eq!(status, StatusCode::OK, "{reply}");

    let chunks = env.chunk_texts("rows").await;
    assert!(chunks.len() > 3);
    for c in &chunks {
        assert!(
            c.chars().count() <= 8000,
            "{} characters",
            c.chars().count()
        );
    }
    for r in &rows {
        assert!(chunks.iter().any(|c| c.contains(r.as_str())), "{r}");
    }
    // The last row is searchable through the full-text index.
    let found: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM chunks WHERE tsv @@ plainto_tsquery('english', 'row299')",
    )
    .fetch_one(&env.pool)
    .await
    .unwrap();
    assert!(found >= 1);
}

#[sqlx::test(migrations = "./migrations")]
async fn a_long_line_without_spaces_is_split_on_char_boundaries(pool: PgPool) {
    let env = Env::new(pool).await;
    let line = "é€𝄞x".repeat(5000);
    let (status, reply) = env.put(one("multibyte", &line)).await;
    assert_eq!(status, StatusCode::OK, "{reply}");

    let chunks = env.chunk_texts("multibyte").await;
    assert_eq!(chunks.len(), 3);
    assert_eq!(chunks[0].chars().count(), 8000);
    assert!(chunks.iter().all(|c| c.chars().count() <= 8000));
    assert_eq!(chunks.concat(), line);
    // All pieces come from one line, so they share its line number.
    let lines: Vec<(Option<i32>, Option<i32>)> =
        sqlx::query_as("SELECT start_line, end_line FROM chunks ORDER BY ord")
            .fetch_all(&env.pool)
            .await
            .unwrap();
    assert_eq!(lines, vec![(Some(1), Some(1)); 3]);
}

#[sqlx::test(migrations = "./migrations")]
async fn a_megabyte_on_one_line_is_accepted_and_complete(pool: PgPool) {
    let env = Env::new(pool).await;
    let line = "abcdefghi ".repeat(MIB / 10);
    let (status, reply) = env.put(one("mib", &line)).await;
    assert_eq!(status, StatusCode::OK, "{reply}");
    let chunks = env.chunk_texts("mib").await;
    assert!(
        chunks.len() > 100 && chunks.len() <= 1000,
        "{}",
        chunks.len()
    );
    assert_eq!(chunks.concat(), line);
}

#[sqlx::test(migrations = "./migrations")]
async fn a_megabyte_heading_line_does_not_multiply_into_every_chunk(pool: PgPool) {
    let env = Env::new(pool).await;
    // One heading line of distinct words, as long as the text limit allows. Its text used to be
    // the symbol of every chunk, which overflowed the tsvector and gave a 500.
    let mut text = String::from("# ");
    let mut i = 0;
    while text.len() + 12 < MIB {
        text.push_str(&format!("w{i} "));
        i += 1;
    }
    assert!(text.len() > MIB - 20 && text.len() <= MIB);
    let (status, reply) = env.put(one("heading", &text)).await;
    assert_eq!(status, StatusCode::OK, "{reply}");
    assert!(reply["chunks"].as_u64().unwrap() > 100, "{reply}");

    let longest: i32 = sqlx::query_scalar(
        "SELECT coalesce(max(octet_length(s)), 0)::int FROM chunks, unnest(symbols) s",
    )
    .fetch_one(&env.pool)
    .await
    .unwrap();
    assert!(longest > 0 && longest <= 1024, "{longest}");
    // The heading line itself is all still there, and searchable.
    assert_eq!(env.chunk_texts("heading").await.concat(), text);
    assert!(!env.hits(&format!("w{}", i - 1)).await.is_empty());
}

// F3: GET returns the text exactly as sent

#[sqlx::test(migrations = "./migrations")]
async fn get_returns_exactly_the_text_that_was_sent(pool: PgPool) {
    let env = Env::new(pool).await;
    let long_line = format!("{} end", "word ".repeat(5000));
    let many_lines: String = (0..400).map(|i| format!("line {i}\n")).collect();
    let texts = [
        ("crlf", "# Head\r\n\r\nbody one\r\nbody two\r\n".to_string()),
        ("blank", "\n\n# H\n\n\nbody\n\n\n".to_string()),
        ("trailing", "one\ntwo\n".to_string()),
        ("no-trailing", "one\ntwo".to_string()),
        ("spaces", "  indented  \n\t\n  \n".to_string()),
        ("long-line", format!("{long_line}\nshort\n{long_line}\n")),
        ("many-lines", many_lines),
        ("unicode", "é€𝄞\n\n𝄞 x\r\n".to_string()),
        ("blank-only", "\n \n\t\n".to_string()),
        ("empty", String::new()),
        ("bare-cr", "a\rb\rc".to_string()),
    ];
    for (id, text) in &texts {
        let (status, reply) = env.put(one(id, text)).await;
        assert_eq!(status, StatusCode::OK, "{id}: {reply}");
    }
    for (id, text) in &texts {
        assert_eq!(&env.text_of(id).await, text, "{id}");
    }
}

#[sqlx::test(migrations = "./migrations")]
async fn replacing_a_document_replaces_the_text_get_returns(pool: PgPool) {
    let env = Env::new(pool).await;
    env.put(one("n1", "first\r\n\r\ntext\n")).await;
    assert_eq!(env.text_of("n1").await, "first\r\n\r\ntext\n");

    // The same request again changes nothing.
    let (_, report) = env.put(one("n1", "first\r\n\r\ntext\n")).await;
    assert_eq!(report["unchanged"], 1);
    assert_eq!(env.text_of("n1").await, "first\r\n\r\ntext\n");

    let (_, report) = env.put(one("n1", "second\n\n\ntext")).await;
    assert_eq!(report["upserted"], 1);
    assert_eq!(env.text_of("n1").await, "second\n\n\ntext");
    let stored: Option<String> = sqlx::query_scalar("SELECT body FROM documents")
        .fetch_one(&env.pool)
        .await
        .unwrap();
    assert_eq!(stored.as_deref(), Some("second\n\n\ntext"));
    assert_eq!(env.count("documents").await, 1);
}

#[sqlx::test(migrations = "./migrations")]
async fn a_deleted_document_takes_its_text_with_it(pool: PgPool) {
    let env = Env::new(pool).await;
    env.put(one("n1", NOTE)).await;
    let id = env.id_of("n1").await;
    let (status, _) = env
        .call("DELETE", "/v1/documents/notes/n1", &env.key, Value::Null)
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, _) = env
        .call("GET", &format!("/v1/documents/{id}"), &env.key, Value::Null)
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(env.count("documents").await, 0);
    assert_eq!(env.count("chunks").await, 0);
}

#[sqlx::test(migrations = "./migrations")]
async fn the_text_of_one_tenant_is_not_readable_by_another(pool: PgPool) {
    let env = Env::new(pool).await;
    env.put(one("n1", "secret\r\ntext")).await;
    let id = env.id_of("n1").await;
    let (_, other) = create_tenant_key(&env.router, "globex", &["ingest", "read"]).await;
    let (status, reply) = env
        .call("GET", &format!("/v1/documents/{id}"), &other, Value::Null)
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(!reply.to_string().contains("secret"));
    // The same external id in the other tenant has its own text.
    env.call("POST", "/v1/documents", &other, one("n1", "other text"))
        .await;
    assert_eq!(env.text_of("n1").await.len(), "secret\r\ntext".len());
}

// F4: dot segments

#[sqlx::test(migrations = "./migrations")]
async fn ids_with_dot_segments_and_dot_sources_are_400(pool: PgPool) {
    let env = Env::new(pool).await;
    for id in [".", "..", "a/../b", "a/./b", "../a", "a/..", "./a", "a/."] {
        let (status, reply) = env.put(one(id, NOTE)).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{id}: {reply}");
    }
    for source in ["..", "."] {
        let (status, reply) = env.put(request(source, doc_with("a", json!({})))).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{source}: {reply}");
    }
    assert_eq!(env.count("documents").await, 0);
    assert_eq!(env.count("sources").await, 0);

    for id in ["a.b", "..a", ".hidden", "a/.b", "a..b", "a/b."] {
        let (status, reply) = env.put(one(id, NOTE)).await;
        assert_eq!(status, StatusCode::OK, "{id}: {reply}");
    }
    for source in ["a.b", "..a", ".hidden", "a.."] {
        let (status, reply) = env.put(request(source, doc_with("a", json!({})))).await;
        assert_eq!(status, StatusCode::OK, "{source}: {reply}");
    }
}

// F5: the gaps the review found

fn put_request(content_length: Option<usize>, body: Body, key: &str) -> Request<Body> {
    let mut req = Request::post("/v1/documents")
        .header("authorization", format!("Bearer {key}"))
        .header("content-type", "application/json");
    if let Some(n) = content_length {
        req = req.header("content-length", n.to_string());
    }
    req.body(body).unwrap()
}

#[sqlx::test(migrations = "./migrations")]
async fn a_streamed_body_over_the_limit_is_413(pool: PgPool) {
    let env = Env::new(pool).await;
    // One shared MiB of whitespace, sent once more than fits. Cloning `Bytes` does not copy, so
    // this holds one MiB of memory. A wrong mapping of the limit error would be a 400 instead.
    let chunk = Bytes::from(vec![b' '; MIB]);
    let parts = BODY_LIMIT / MIB + 1;
    let stream =
        futures::stream::iter((0..parts).map(move |_| Ok::<Bytes, std::io::Error>(chunk.clone())));
    let req = put_request(None, Body::from_stream(stream), &env.key);
    assert!(req.headers().get("content-length").is_none());
    let res = env.router.clone().oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(
        body_json(res).await,
        json!({ "error": "payload too large" })
    );
    assert_eq!(env.count("documents").await, 0);
}

#[sqlx::test(migrations = "./migrations")]
async fn a_declared_length_over_the_limit_is_413_with_the_json_error(pool: PgPool) {
    let env = Env::new(pool).await;
    let req = put_request(Some(BODY_LIMIT + 1), Body::empty(), &env.key);
    let res = env.router.clone().oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(
        res.headers()["content-type"],
        "application/json",
        "the 413 is the standard error body"
    );
    assert_eq!(
        body_json(res).await,
        json!({ "error": "payload too large" })
    );

    // A declared length of exactly the limit is not refused for its size: the empty body then
    // fails to parse, which is a 400.
    let req = put_request(Some(BODY_LIMIT), Body::empty(), &env.key);
    let res = env.router.clone().oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::BAD_REQUEST);
}

#[sqlx::test(migrations = "./migrations")]
async fn the_title_reaches_the_chunker_and_the_embedder(pool: PgPool) {
    let env = Env::new(pool).await;
    let title = "Zyxquartz handbook";
    let plain = "plain body words and nothing else";
    let headed = "# Intro\nheaded body words";
    let sent = json!({
        "source": "notes",
        "documents": [
            { "external_id": "plain", "title": title, "text": plain },
            { "external_id": "headed", "title": title, "text": headed },
        ]
    });
    let (status, reply) = env.put(sent).await;
    assert_eq!(status, StatusCode::OK, "{reply}");

    // Text without a heading is a section named after the title, so the title is a symbol and a
    // word of the full-text index. It is in none of the chunk texts.
    let symbols: Vec<String> = sqlx::query_scalar(
        "SELECT symbols[1] FROM chunks c JOIN documents d ON d.id = c.document_id
         WHERE d.external_id = 'plain'",
    )
    .fetch_all(&env.pool)
    .await
    .unwrap();
    assert_eq!(symbols, vec![title.to_string()]);
    let found: Vec<String> = sqlx::query_scalar(
        "SELECT d.external_id FROM chunks c JOIN documents d ON d.id = c.document_id
         WHERE c.tsv @@ plainto_tsquery('english', 'zyxquartz')",
    )
    .fetch_all(&env.pool)
    .await
    .unwrap();
    assert_eq!(found, vec!["plain".to_string()]);
    assert!(!plain.contains("Zyxquartz"));

    // The embedding is the one of `embed_text(title, chunk)` for both documents; the headed one
    // has the title only there, since its symbol is the heading.
    for (id, text) in [("plain", plain), ("headed", headed)] {
        let chunks = chunk_markdown_split(text, title, usize::MAX).unwrap();
        assert_eq!(chunks.len(), 1, "{id}");
        let want = HashEmbedder
            .embed_query(&embed_text(title, &chunks[0]))
            .unwrap();
        let stored: Vector = sqlx::query_scalar(
            "SELECT c.embedding FROM chunks c JOIN documents d ON d.id = c.document_id
             WHERE d.external_id = $1",
        )
        .bind(id)
        .fetch_one(&env.pool)
        .await
        .unwrap();
        assert_eq!(stored.as_slice(), want.as_slice(), "{id}");
    }
    let headed_symbols: Vec<String> = sqlx::query_scalar(
        "SELECT symbols[1] FROM chunks c JOIN documents d ON d.id = c.document_id
         WHERE d.external_id = 'headed'",
    )
    .fetch_all(&env.pool)
    .await
    .unwrap();
    assert_eq!(headed_symbols, vec!["Intro".to_string()]);
}

#[sqlx::test(migrations = "./migrations")]
async fn a_changed_uri_is_written_and_returned(pool: PgPool) {
    let env = Env::new(pool).await;
    let with_uri = |uri: &str| {
        let mut body = zorblax();
        body["documents"][0]["uri"] = json!(uri);
        body
    };
    let (_, report) = env.put(with_uri("https://example.com/old")).await;
    assert_eq!(report["upserted"], 1);
    let (_, report) = env.put(with_uri("https://example.com/old")).await;
    assert_eq!(report["unchanged"], 1);

    let (_, report) = env.put(with_uri("https://example.com/new")).await;
    assert_eq!(report["upserted"], 1, "{report}");
    assert_eq!(report["unchanged"], 0, "{report}");
    let hits = env.hits("zorblax").await;
    assert_eq!(hits[0]["uri"], "https://example.com/new");
    let id = env.id_of("n1").await;
    let (_, doc) = env
        .call("GET", &format!("/v1/documents/{id}"), &env.key, Value::Null)
        .await;
    assert_eq!(doc["uri"], "https://example.com/new");
}

#[sqlx::test(migrations = "./migrations")]
async fn a_nul_in_the_uri_or_a_metadata_key_is_400(pool: PgPool) {
    let env = Env::new(pool).await;
    let cases = [
        ("uri", json!({ "uri": "https://example.com/a\u{0}b" })),
        ("metadata key", json!({ "metadata": { "k\u{0}": 1 } })),
        (
            "nested metadata key",
            json!({ "metadata": { "a": [{ "b\u{0}": 1 }] } }),
        ),
        ("metadata value", json!({ "metadata": { "a": "b\u{0}" } })),
    ];
    for (name, extra) in cases {
        let (status, reply) = env.put(request("s", doc_with("a", extra))).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{name}: {reply}");
    }
    assert_eq!(env.count("documents").await, 0);
}

#[sqlx::test(migrations = "./migrations")]
async fn delete_does_not_touch_the_documents_of_a_repo_source(pool: PgPool) {
    let env = Env::new(pool).await;
    let (_, _repo) = push_fixture(&env.router, &env.key).await;
    let before = env.count("documents").await;
    assert!(before > 0);
    let ids: Vec<String> = sqlx::query_scalar(
        "SELECT d.external_id FROM documents d JOIN sources s ON s.id = d.source_id
         WHERE s.name = 'sample' AND s.kind = 'repo'",
    )
    .fetch_all(&env.pool)
    .await
    .unwrap();
    assert_eq!(ids.len() as i64, before);
    for id in &ids {
        let (status, _) = env
            .call(
                "DELETE",
                &format!("/v1/documents/sample/{id}"),
                &env.key,
                Value::Null,
            )
            .await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{id}");
    }
    assert_eq!(env.count("documents").await, before);
}
