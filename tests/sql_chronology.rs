mod common;

use axum::http::StatusCode;
use common::{
    bundle, commit_all, create_repo, create_tenant_key, fixture_repo, git, head, put_bundle, send,
    test_router, test_state,
};
use lore::chunk::chunk_file;
use lore::http::AppState;
use lore::search::{Hit, SearchQuery, search};
use lore::store::{IndexedChunk, NewDocument, ensure_source, index_text, replace_chunks};
use serde_json::json;
use sha2::{Digest, Sha256};
use sqlx::PgPool;
use uuid::Uuid;

async fn sql(state: &AppState, tenant: Uuid, source: &str, path: &str, text: &str) {
    let source = ensure_source(&state.pool, tenant, "repo", source, None)
        .await
        .unwrap();
    index_text(
        state,
        tenant,
        &source,
        NewDocument {
            tenant_id: tenant,
            source_id: source.id,
            // Document IDs and metadata paths are independent store inputs. Keep the ID small
            // when testing full metadata paths beyond the existing document-ID B-tree limit.
            external_id: if path.len() > 2048 {
                format!("path-{:x}", Sha256::digest(path.as_bytes()))
            } else {
                path.into()
            },
            kind: "code".into(),
            title: path.into(),
            uri: None,
            metadata: json!({"path": path}),
            content_hash: text.into(),
            body: None,
        },
        chunk_file(path, text),
        path,
    )
    .await
    .unwrap();
}

async fn find(state: &AppState, tenant: Uuid, q: &str, k: usize) -> Vec<Hit> {
    find_in(state, tenant, q, k, Some("app")).await
}

async fn find_in(
    state: &AppState,
    tenant: Uuid,
    q: &str,
    k: usize,
    source: Option<&str>,
) -> Vec<Hit> {
    search(
        &state.pool,
        state.embedder.clone(),
        SearchQuery {
            tenant_id: tenant,
            q: q.into(),
            k,
            kinds: Some(vec!["code".into()]),
            source: source.map(str::to_string),
        },
    )
    .await
    .unwrap()
}

const CREATE: &str = "CREATE TABLE Accounts (status text CHECK (status IN ('pending', 'active', 'locked', 'closed', 'trial', 'paused')));";
const ALTER: &str = "ALTER TABLE public.accounts DROP CONSTRAINT accounts_status_check;\nALTER TABLE public.accounts ADD CONSTRAINT accounts_status_check CHECK (status IN ('pending', 'active', 'locked', 'closed', 'trial', 'paused', 'archived'));";

#[sqlx::test(migrations = "./migrations")]
async fn current_values_search_and_both_answer_modes_include_the_change(pool: PgPool) {
    let mut state = test_state(pool, &std::env::temp_dir());
    // Capture the real fast-mode prompt while retaining the deterministic local fake model.
    let captured = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
    state.llm = std::sync::Arc::new(CapturePrompt(captured.clone()));
    let router = test_router(state.clone());
    let (tenant, key) = create_tenant_key(&router, "sql", &["read"]).await;
    sql(&state, tenant, "app", "migrations/001_accounts.sql", CREATE).await;
    sql(&state, tenant, "app", "migrations/002_archived.sql", ALTER).await;
    let hits = find(&state, tenant, "which statuses can an account be in?", 5).await;
    assert_eq!(hits.len(), 2);
    assert_eq!(hits[0].path.as_deref(), Some("migrations/002_archived.sql"));
    assert_eq!(hits[1].path.as_deref(), Some("migrations/001_accounts.sql"));
    assert_eq!(hits[1].later_changes, ["app:migrations/002_archived.sql"]);
    assert!(hits[0].text.contains("'archived'"));
    let (status, body) = send(
        &router,
        "GET",
        "/v1/search?q=accounts&k=5&source=app",
        &key,
        Vec::new(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let wire = body["hits"].as_array().unwrap();
    assert_eq!(wire[0]["path"], "migrations/002_archived.sql");
    assert_eq!(wire[1]["path"], "migrations/001_accounts.sql");
    assert!(
        wire.iter()
            .all(|hit| hit.get("later_changes").is_none() && hit.get("sql_subjects").is_none())
    );
    let stored: Vec<String> =
        sqlx::query_scalar("SELECT sql_subjects FROM chunks WHERE document_id = $1")
            .bind(hits[1].document_id)
            .fetch_one(&state.pool)
            .await
            .unwrap();
    assert_eq!(stored, ["table:\"public\".\"accounts\""]);
    for mode in ["sources", "fast"] {
        let (status, body) = send(&router, "POST", "/v1/ask", &key, json!({
            "question": "which statuses can an account be in?", "source": "app", "mode": mode, "k": 5,
        }).to_string().into_bytes()).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert!(
            body["answer"]
                .as_str()
                .unwrap()
                .contains("later SQL changes in `app:migrations/002_archived.sql`"),
            "{body}"
        );
        assert!(
            body["citations"]
                .as_array()
                .unwrap()
                .iter()
                .any(|c| c["path"] == "migrations/002_archived.sql")
        );
    }
    {
        let prompt = captured.lock().unwrap();
        assert!(prompt.contains("[2] migrations/001_accounts.sql:1-1 (code); later SQL changes"));
        assert!(prompt.contains("'archived'"));
    }
    let smallest = find(&state, tenant, "account statuses", 1).await;
    assert_eq!(smallest.len(), 1);
    assert_eq!(
        smallest[0].path.as_deref(),
        Some("migrations/002_archived.sql")
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn collapsed_sql_alias_keeps_its_own_source_chronology(pool: PgPool) {
    let state = test_state(pool, &std::env::temp_dir());
    let (tenant, _) = create_tenant_key(&test_router(state.clone()), "sql-alias", &["read"]).await;
    sql(&state, tenant, "a", "001.sql", CREATE).await;
    sql(&state, tenant, "b", "001.sql", CREATE).await;
    sql(&state, tenant, "b", "003.sql", ALTER).await;
    let hits = find_in(&state, tenant, "account statuses", 5, None).await;
    assert_eq!(hits.len(), 2, "{hits:?}");
    assert_eq!(hits[0].source, "b");
    assert_eq!(hits[0].path.as_deref(), Some("003.sql"));
    assert_eq!(hits[1].path.as_deref(), Some("001.sql"));
    assert_eq!(hits[1].source, "a");
    assert_eq!(hits[1].also_in, ["b"]);
    assert_eq!(hits[1].later_changes, ["b:003.sql"]);
    let scoped_a = find_in(&state, tenant, "account statuses", 5, Some("a")).await;
    assert_eq!(scoped_a.len(), 1);
    assert!(scoped_a[0].later_changes.is_empty());
    assert!(scoped_a[0].also_in.is_empty());
    let scoped_b = find_in(&state, tenant, "account statuses", 5, Some("b")).await;
    assert_eq!(scoped_b.len(), 2);
    assert_eq!(scoped_b[0].source, "b");
    assert_eq!(scoped_b[0].path.as_deref(), Some("003.sql"));
    assert_eq!(scoped_b[1].path.as_deref(), Some("001.sql"));
    assert_eq!(scoped_b[1].later_changes, ["b:003.sql"]);
    assert!(scoped_b.iter().all(|h| h.also_in.is_empty()));
}

#[sqlx::test(migrations = "./migrations")]
async fn cross_source_duplicates_do_not_erase_repeated_migration_positions(pool: PgPool) {
    let state = test_state(pool, &std::env::temp_dir());
    let (tenant, _) =
        create_tenant_key(&test_router(state.clone()), "sql-history", &["read"]).await;
    sql(&state, tenant, "a", "001.sql", CREATE).await;
    sql(&state, tenant, "a", "002.sql", CREATE).await;
    sql(&state, tenant, "b", "001.sql", CREATE).await;
    sql(&state, tenant, "b", "003.sql", ALTER).await;
    for k in [1, 2, 3, 5] {
        let hits = find_in(&state, tenant, "account statuses", k, None).await;
        assert!(hits.len() <= k, "{k}: {hits:?}");
        if let Some(old) = hits
            .iter()
            .position(|h| h.path.as_deref() == Some("001.sql"))
        {
            assert!(
                hits[..old]
                    .iter()
                    .any(|h| h.source == "a" && h.path.as_deref() == Some("002.sql")),
                "{k}: {hits:?}"
            );
            assert!(
                hits[..old]
                    .iter()
                    .any(|h| h.source == "b" && h.path.as_deref() == Some("003.sql")),
                "{k}: {hits:?}"
            );
            assert!(hits[old].later_changes.contains(&"a:002.sql".into()));
            assert!(hits[old].later_changes.contains(&"b:003.sql".into()));
        }
        if k == 5 {
            assert_eq!(hits.len(), 3, "{hits:?}");
            assert!(hits.iter().any(|h| h.source == "a"
                && h.path.as_deref() == Some("002.sql")
                && h.later_changes.is_empty()));
            assert!(
                hits.iter()
                    .any(|h| h.path.as_deref() == Some("001.sql") && h.also_in == ["b"])
            );
        }
    }
}

#[sqlx::test(migrations = "./migrations")]
async fn collapsed_newest_companions_keep_both_sources_and_warnings(pool: PgPool) {
    let state = test_state(pool, &std::env::temp_dir());
    let (tenant, _) =
        create_tenant_key(&test_router(state.clone()), "sql-companions", &["read"]).await;
    for source in ["a", "b"] {
        sql(&state, tenant, source, "001.sql", CREATE).await;
        sql(&state, tenant, source, "003.sql", ALTER).await;
    }
    let hits = find_in(&state, tenant, "account statuses", 5, None).await;
    assert_eq!(hits.len(), 2, "{hits:?}");
    assert_eq!(hits[0].path.as_deref(), Some("003.sql"));
    assert_eq!(hits[1].path.as_deref(), Some("001.sql"));
    assert_eq!(hits[0].also_in.len(), 1);
    assert_eq!(hits[1].also_in.len(), 1);
    assert!(hits[1].later_changes.contains(&"a:003.sql".into()));
    assert!(hits[1].later_changes.contains(&"b:003.sql".into()));
}

struct CapturePrompt(std::sync::Arc<std::sync::Mutex<String>>);

#[async_trait::async_trait]
impl lore::llm::Llm for CapturePrompt {
    fn name(&self) -> &str {
        "capture"
    }
    fn supports_tools(&self) -> bool {
        false
    }
    async fn complete(&self, req: lore::llm::LlmRequest) -> anyhow::Result<lore::llm::LlmResponse> {
        let prompt = req
            .messages
            .iter()
            .flat_map(|m| &m.content)
            .filter_map(|b| {
                if let lore::llm::Block::Text { text } = b {
                    Some(text.as_str())
                } else {
                    None
                }
            })
            .collect::<Vec<_>>()
            .join("\n");
        *self.0.lock().unwrap() = prompt;
        lore::llm::Llm::complete(&lore::llm::FakeLlm::new(), req).await
    }
}

#[sqlx::test(migrations = "./migrations")]
async fn newest_uses_path_not_ingest_time_or_numeric_sort_and_deleted_files_disappear(
    pool: PgPool,
) {
    let state = test_state(pool, &std::env::temp_dir());
    let (tenant, _) = create_tenant_key(&test_router(state.clone()), "order", &["read"]).await;
    // Insert newest first, then "10": bytewise path ordering puts "9" after "10".
    sql(
        &state,
        tenant,
        "app",
        "migrations/9_drop.sql",
        "DROP TABLE accounts;",
    )
    .await;
    sql(&state, tenant, "app", "migrations/10_alter.sql", ALTER).await;
    sql(&state, tenant, "app", "migrations/01_create.sql", CREATE).await;
    let hits = find(&state, tenant, "account statuses", 5).await;
    assert_eq!(
        hits[0].path.as_deref(),
        Some("migrations/9_drop.sql"),
        "{hits:?}"
    );
    let id = hits[0].document_id;
    sqlx::query("DELETE FROM documents WHERE id = $1")
        .bind(id)
        .execute(&state.pool)
        .await
        .unwrap();
    let hits = find(&state, tenant, "account statuses", 5).await;
    assert_eq!(
        hits[0].path.as_deref(),
        Some("migrations/10_alter.sql"),
        "{hits:?}"
    );
    assert!(
        hits.iter()
            .all(|h| h.later_changes.iter().all(|p| !p.contains("9_drop")))
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn subjects_do_not_cross_schema_kind_source_or_tenant(pool: PgPool) {
    let state = test_state(pool, &std::env::temp_dir());
    let router = test_router(state.clone());
    let (tenant, _) = create_tenant_key(&router, "one", &["read"]).await;
    let (other, _) = create_tenant_key(&router, "two", &["read"]).await;
    sql(&state, tenant, "app", "001.sql", CREATE).await;
    sql(
        &state,
        tenant,
        "app",
        "z_audit.sql",
        "ALTER TABLE audit.accounts ADD COLUMN x int;",
    )
    .await;
    sql(
        &state,
        tenant,
        "app",
        "z_type.sql",
        "ALTER TYPE accounts ADD VALUE 'extra';",
    )
    .await;
    sql(&state, tenant, "other", "z_other.sql", ALTER).await;
    sql(&state, other, "app", "z_tenant.sql", ALTER).await;
    let hits = find(&state, tenant, "account statuses", 5).await;
    let original = hits
        .iter()
        .find(|h| h.path.as_deref() == Some("001.sql"))
        .unwrap();
    assert!(original.later_changes.is_empty(), "{hits:?}");
    assert!(hits.iter().all(|h| h.source == "app"));
    assert!(
        hits.iter()
            .all(|h| !["z_other.sql", "z_tenant.sql"].contains(&h.path.as_deref().unwrap()))
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn enum_alter_multi_object_drop_and_case_folded_names_are_followed(pool: PgPool) {
    let state = test_state(pool, &std::env::temp_dir());
    let (tenant, _) = create_tenant_key(&test_router(state.clone()), "names", &["read"]).await;
    sql(&state, tenant, "app", "001.sql", "CREATE TYPE app.\"shipment_stage\" AS ENUM ('queued', 'sent');\nCREATE TABLE retired (state text CHECK (state IN ('old', 'unused')));").await;
    sql(
        &state,
        tenant,
        "app",
        "002.sql",
        "ALTER TYPE APP.shipment_stage ADD VALUE 'delivered';\nDROP TABLE unrelated, retired;",
    )
    .await;
    let hits = find(&state, tenant, "shipment stage enum values", 5).await;
    let old_enum = hits
        .iter()
        .find(|h| h.text.contains("CREATE TYPE"))
        .unwrap();
    assert_eq!(old_enum.later_changes, ["app:002.sql"]);
    let old_table = hits
        .iter()
        .find(|h| h.text.contains("CREATE TABLE"))
        .unwrap();
    assert_eq!(old_table.later_changes, ["app:002.sql"]);
    let newest_enum = hits
        .iter()
        .position(|h| h.text.contains("ADD VALUE"))
        .unwrap();
    let original_enum = hits
        .iter()
        .position(|h| h.text.contains("CREATE TYPE"))
        .unwrap();
    assert!(newest_enum < original_enum, "{hits:?}");
    assert!(
        hits.iter()
            .position(|h| h.text.contains("DROP TABLE"))
            .unwrap()
            < hits
                .iter()
                .position(|h| h.text.contains("CREATE TABLE"))
                .unwrap()
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn quoted_case_does_not_link_distinct_objects_despite_folded_subject_keys(pool: PgPool) {
    let state = test_state(pool, &std::env::temp_dir());
    let (tenant, _) = create_tenant_key(&test_router(state.clone()), "quoted", &["read"]).await;
    sql(
        &state,
        tenant,
        "app",
        "001.sql",
        "CREATE TABLE \"Accounts\" (status text CHECK (status IN ('special', 'private')));",
    )
    .await;
    sql(&state, tenant, "app", "002.sql", ALTER).await;
    let hits = find(&state, tenant, "account statuses", 5).await;
    let original = hits
        .iter()
        .find(|h| h.path.as_deref() == Some("001.sql"))
        .unwrap();
    assert!(original.later_changes.is_empty(), "{hits:?}");
    let rows: Vec<(String, String)> = sqlx::query_as(
        "SELECT subject, identity FROM sql_subject_lookup WHERE tenant_id = $1 ORDER BY path",
    )
    .bind(tenant)
    .fetch_all(&state.pool)
    .await
    .unwrap();
    assert_eq!(rows[0].0, rows[1].0);
    assert_ne!(rows[0].1, rows[1].1);
    sql(
        &state,
        tenant,
        "app",
        "003.sql",
        "ALTER TABLE \"Accounts\" ADD COLUMN quoted_only int;",
    )
    .await;
    let hits = find(&state, tenant, "account statuses", 5).await;
    let original = hits
        .iter()
        .find(|h| h.path.as_deref() == Some("001.sql"))
        .unwrap();
    assert_eq!(original.later_changes, ["app:003.sql"]);
}

#[sqlx::test(migrations = "./migrations")]
async fn lookup_rows_replace_delete_and_rollback_with_chunks(pool: PgPool) {
    let state = test_state(pool, &std::env::temp_dir());
    let (tenant, _) = create_tenant_key(&test_router(state.clone()), "cleanup", &["read"]).await;
    sql(&state, tenant, "app", "001.sql", CREATE).await;
    sql(&state, tenant, "app", "002.sql", ALTER).await;
    let doc: Uuid = sqlx::query_scalar(
        "SELECT id FROM documents WHERE tenant_id = $1 AND external_id = '002.sql'",
    )
    .bind(tenant)
    .fetch_one(&state.pool)
    .await
    .unwrap();
    let mut tx = state.pool.begin().await.unwrap();
    let replacement = chunk_file("002.sql", "CREATE TABLE temporary (id int);").remove(0);
    let indexed = IndexedChunk {
        lexical: replacement.text.clone(),
        embedding: state.embedder.embed_query(&replacement.text).unwrap(),
        chunk: replacement,
    };
    replace_chunks(&mut tx, tenant, doc, &[indexed])
        .await
        .unwrap();
    let inside: Vec<String> = sqlx::query_scalar("SELECT identity FROM sql_subject_lookup WHERE chunk_id IN (SELECT id FROM chunks WHERE document_id = $1)")
        .bind(doc).fetch_all(&mut *tx).await.unwrap();
    assert_eq!(inside, ["table:\"public\".\"temporary\""]);
    // Readers on another connection still see all-old SQL subjects while replacement is pending.
    let hits = find(&state, tenant, "account statuses", 5).await;
    assert!(hits.iter().any(|h| h.later_changes == ["app:002.sql"]));
    tx.rollback().await.unwrap();
    let hits = find(&state, tenant, "account statuses", 5).await;
    assert!(hits.iter().any(|h| h.later_changes == ["app:002.sql"]));
    sql(
        &state,
        tenant,
        "app",
        "002.sql",
        "CREATE TABLE unrelated (id int);",
    )
    .await;
    let hits = find(&state, tenant, "account statuses", 5).await;
    assert!(hits.iter().all(|h| h.later_changes.is_empty()));
    let stale: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM sql_subject_lookup WHERE identity LIKE '%temporary%'",
    )
    .fetch_one(&state.pool)
    .await
    .unwrap();
    assert_eq!(stale, 0);
    sqlx::query("DELETE FROM sources WHERE tenant_id = $1")
        .bind(tenant)
        .execute(&state.pool)
        .await
        .unwrap();
    let remaining: i64 =
        sqlx::query_scalar("SELECT count(*) FROM sql_subject_lookup WHERE tenant_id = $1")
            .bind(tenant)
            .fetch_one(&state.pool)
            .await
            .unwrap();
    assert_eq!(remaining, 0);
}

fn random_ascii(len: usize) -> String {
    let mut value = String::new();
    while value.len() < len {
        value.push_str(&Uuid::new_v4().simple().to_string());
    }
    value.truncate(len);
    value
}

#[sqlx::test(migrations = "./migrations")]
async fn long_identity_and_path_boundaries_keep_chronology_complete(pool: PgPool) {
    let state = test_state(pool, &std::env::temp_dir());
    let router = test_router(state.clone());
    for identity_bytes in [256, 257, 4096] {
        for path_bytes in [2048, 2049, 16384] {
            let name = random_ascii(identity_bytes - "table:\"public\".\"\"".len());
            let create = format!(
                "CREATE TABLE \"{name}\" (state text CHECK (state IN ('open', 'closed')));"
            );
            assert_eq!(
                chunk_file("000.sql", &create)[0].sql_identities[0].len(),
                identity_bytes
            );
            let (tenant, _) = create_tenant_key(
                &router,
                &format!("bounds-{identity_bytes}-{path_bytes}"),
                &["read"],
            )
            .await;
            let latest = format!("z/{}.sql", random_ascii(path_bytes - 6));
            assert_eq!(latest.len(), path_bytes);
            sql(&state, tenant, "app", "000.sql", &create).await;
            sql(
                &state,
                tenant,
                "app",
                &latest,
                &format!("ALTER TABLE \"{name}\" ADD COLUMN latest_marker int;"),
            )
            .await;
            let hits = find(&state, tenant, "what are the current state values?", 5).await;
            assert_eq!(hits.len(), 2, "{identity_bytes}/{path_bytes}: {hits:?}");
            assert!(
                hits[0].text.contains("latest_marker"),
                "{identity_bytes}/{path_bytes}"
            );
            assert_eq!(hits[1].later_changes, [format!("app:{latest}")]);
            sqlx::query("DELETE FROM tenants WHERE id = $1")
                .bind(tenant)
                .execute(&state.pool)
                .await
                .unwrap();
            let left: i64 =
                sqlx::query_scalar("SELECT count(*) FROM sql_subject_lookup WHERE tenant_id = $1")
                    .bind(tenant)
                    .fetch_one(&state.pool)
                    .await
                    .unwrap();
            assert_eq!(left, 0);
        }
    }
}

#[sqlx::test(migrations = "./migrations")]
async fn subjects_after_sixty_four_and_hash_collisions_do_not_lose_or_mislink_changes(
    pool: PgPool,
) {
    let state = test_state(pool, &std::env::temp_dir());
    let router = test_router(state.clone());
    for count in [64, 65, 100] {
        let (tenant, _) = create_tenant_key(&router, &format!("subjects-{count}"), &["read"]).await;
        let create = (0..count)
            .map(|i| format!("CREATE TABLE t{i} (state text); "))
            .collect::<String>();
        sql(&state, tenant, "app", "001.sql", &create).await;
        sql(
            &state,
            tenant,
            "app",
            "002.sql",
            &format!("ALTER TABLE t{} ADD COLUMN latest int;", count - 1),
        )
        .await;
        let hits = find(&state, tenant, "what state columns are there?", 5).await;
        assert_eq!(hits[0].path.as_deref(), Some("002.sql"));
        assert_eq!(hits[1].later_changes, ["app:002.sql"]);
    }
    let (tenant, _) = create_tenant_key(&router, "collisions", &["read"]).await;
    sql(&state, tenant, "app", "001.sql", CREATE).await;
    sql(
        &state,
        tenant,
        "app",
        "002.sql",
        "ALTER TABLE unrelated ADD COLUMN x int;",
    )
    .await;
    let overflow = format!("z/{}.sql", random_ascii(2049));
    sql(
        &state,
        tenant,
        "app",
        &overflow,
        "ALTER TABLE unrelated ADD COLUMN y int;",
    )
    .await;
    // Deliberately force equal hash buckets. Full identity equality, not hash equality, decides.
    sqlx::query("UPDATE sql_subject_lookup SET identity_hash = (SELECT identity_hash FROM sql_subject_lookup WHERE tenant_id = $1 AND identity = 'table:\"public\".\"accounts\"' LIMIT 1) WHERE tenant_id = $1")
        .bind(tenant).execute(&state.pool).await.unwrap();
    let hits = find(&state, tenant, "account statuses", 5).await;
    let original = hits
        .iter()
        .find(|h| h.path.as_deref() == Some("001.sql"))
        .unwrap();
    assert!(original.later_changes.is_empty(), "{hits:?}");
}

#[sqlx::test(migrations = "./migrations")]
async fn real_bundle_ingest_rejects_invalid_paths_atomically_and_indexes_valid_sql(pool: PgPool) {
    let data = tempfile::tempdir().unwrap();
    let state = test_state(pool, data.path());
    let router = test_router(state.clone());
    let (tenant, key) = create_tenant_key(&router, "long-git-path", &["ingest", "read"]).await;
    let repo = fixture_repo();
    let long = format!(
        "migrations/z{}.sql",
        random_ascii(16384 - "migrations/z.sql".len())
    );
    for (path, text) in [("migrations/001.sql", CREATE), (long.as_str(), ALTER)] {
        let file = data.path().join("ddl.sql");
        std::fs::write(&file, text).unwrap();
        let blob = git(&repo, &["hash-object", "-w", file.to_str().unwrap()]);
        let entry = format!("100644,{},{}", blob.trim(), path);
        git(&repo, &["update-index", "--add", "--cacheinfo", &entry]);
    }
    let tree = git(&repo, &["write-tree"]);
    let commit = git(
        &repo,
        &[
            "commit-tree",
            tree.trim(),
            "-p",
            &head(&repo),
            "-m",
            "Add SQL chronology paths",
        ],
    );
    git(&repo, &["update-ref", "HEAD", commit.trim()]);
    let (status, source) = create_repo(&router, &key, "app").await;
    assert_eq!(status, StatusCode::OK);
    let source_id: Uuid = source["id"].as_str().unwrap().parse().unwrap();
    let (status, report) = put_bundle(
        &router,
        &key,
        source_id,
        bundle(&repo, &["HEAD"]),
        &head(&repo),
        true,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{report}");
    let rows: i64 =
        sqlx::query_scalar("SELECT count(*) FROM sql_subject_lookup WHERE tenant_id = $1")
            .bind(tenant)
            .fetch_one(&state.pool)
            .await
            .unwrap();
    assert_eq!(rows, 0);
    let indexed: Option<String> =
        sqlx::query_scalar("SELECT indexed_commit FROM sources WHERE id = $1")
            .bind(source_id)
            .fetch_one(&state.pool)
            .await
            .unwrap();
    assert!(indexed.is_none());

    // Git's existing strict fsck rejects the oversized component above. A valid nested
    // path stays within this host's checkout limits, while still exceeding API ID limits.
    let repo = fixture_repo();
    let long = format!("migrations/{}latest.sql", "nested_component/".repeat(40));
    assert!(long.len() > 512 && long.len() < 800);
    let initial = repo.path().join("migrations/001.sql");
    let latest = repo.path().join(&long);
    std::fs::create_dir_all(latest.parent().unwrap()).unwrap();
    std::fs::write(initial, CREATE).unwrap();
    std::fs::write(latest, ALTER).unwrap();
    commit_all(&repo, "Fixture: Add valid SQL migration paths");
    let (status, report) = put_bundle(
        &router,
        &key,
        source_id,
        bundle(&repo, &["HEAD"]),
        &head(&repo),
        true,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{report}");
    let hits = find(&state, tenant, "which statuses can an account be in?", 5).await;
    let original = hits
        .iter()
        .find(|h| h.path.as_deref() == Some("migrations/001.sql"))
        .unwrap();
    assert_eq!(original.later_changes, [format!("app:{long}")]);
    assert!(
        hits.iter()
            .position(|h| h.text.contains("'archived'"))
            .unwrap()
            < hits
                .iter()
                .position(|h| h.path.as_deref() == Some("migrations/001.sql"))
                .unwrap()
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn newest_compares_normal_and_overflow_paths_and_last_chunk_in_a_file(pool: PgPool) {
    let state = test_state(pool, &std::env::temp_dir());
    let (tenant, _) =
        create_tenant_key(&test_router(state.clone()), "mixed-paths", &["read"]).await;
    sql(&state, tenant, "app", "a.sql", CREATE).await;
    let overflow = format!("m{}.sql", random_ascii(2049));
    sql(&state, tenant, "app", &overflow, ALTER).await;
    sql(&state, tenant, "app", "z.sql", "ALTER TABLE accounts ADD COLUMN x int;\nCREATE TABLE unrelated (id int);\nDROP TABLE accounts;").await;
    let hits = find(&state, tenant, "account statuses", 5).await;
    assert!(hits[0].text.contains("DROP TABLE accounts"), "{hits:?}");
    let original = hits
        .iter()
        .find(|h| h.path.as_deref() == Some("a.sql"))
        .unwrap();
    assert_eq!(original.later_changes, ["app:z.sql"]);
    let mut ids: Vec<i64> = hits.iter().map(|h| h.chunk_id).collect();
    ids.sort_unstable();
    ids.dedup();
    assert_eq!(ids.len(), hits.len());
    sqlx::query("DELETE FROM documents WHERE tenant_id = $1 AND external_id = 'z.sql'")
        .bind(tenant)
        .execute(&state.pool)
        .await
        .unwrap();
    let hits = find(&state, tenant, "account statuses", 5).await;
    let original = hits
        .iter()
        .find(|h| h.path.as_deref() == Some("a.sql"))
        .unwrap();
    assert_eq!(original.later_changes, [format!("app:{overflow}")]);
    assert!(hits[0].text.contains("'archived'"));
}
