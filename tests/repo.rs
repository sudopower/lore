mod common;

use std::path::{Path, PathBuf};
use std::time::Duration;

use axum::Router;
use axum::body::{Body, Bytes};
use axum::http::{Request, StatusCode};
use common::{
    body_json, bundle, commit_all, create_repo, create_tenant_key, fixture_repo, git, head,
    push_fixture, put_bundle, send, test_router, test_state, test_state_with_limit,
};
use lore::http::AppState;
use serde_json::{Value, json};
use sqlx::PgPool;
use tempfile::TempDir;
use tower::ServiceExt;
use uuid::Uuid;

struct Env {
    router: Router,
    state: AppState,
    data: TempDir,
    key: String,
}

async fn env_with_limit(pool: PgPool, limit: Option<u64>) -> Env {
    let data = tempfile::tempdir().unwrap();
    let state = test_state_with_limit(pool, data.path(), limit);
    let router = test_router(state.clone());
    let (_, key) = create_tenant_key(&router, "acme", &["ingest", "read"]).await;
    Env {
        router,
        state,
        data,
        key,
    }
}

async fn env(pool: PgPool) -> Env {
    env_with_limit(pool, None).await
}

/// The external ids of the documents of a source, sorted.
async fn doc_paths(pool: &PgPool, source_id: Uuid) -> Vec<String> {
    sqlx::query_scalar("SELECT external_id FROM documents WHERE source_id = $1 ORDER BY 1")
        .bind(source_id)
        .fetch_all(pool)
        .await
        .unwrap()
}

async fn chunk_count(pool: &PgPool, source_id: Uuid) -> i64 {
    sqlx::query_scalar(
        "SELECT count(*) FROM chunks c JOIN documents d ON d.id = c.document_id
         WHERE d.source_id = $1",
    )
    .bind(source_id)
    .fetch_one(pool)
    .await
    .unwrap()
}

/// Files and directories left in `<data_dir>/tmp`.
fn tmp_entries(data: &Path) -> usize {
    std::fs::read_dir(data.join("tmp")).map_or(0, |d| d.count())
}

fn checkout_dir(data: &Path, source_id: Uuid) -> PathBuf {
    data.join("repos")
        .join(source_id.to_string())
        .join("checkout")
}

fn mirror_dir(data: &Path, source_id: Uuid) -> PathBuf {
    data.join("repos")
        .join(source_id.to_string())
        .join("mirror.git")
}

/// A bundle of what `repo` gained since `prev`.
fn incremental(repo: &TempDir, prev: &str) -> Vec<u8> {
    bundle(repo, &["HEAD", &format!("^{prev}")])
}

fn full_bundle(repo: &TempDir) -> Vec<u8> {
    bundle(repo, &["HEAD"])
}

async fn push(
    env: &Env,
    source_id: Uuid,
    bytes: Vec<u8>,
    sha: &str,
    full: bool,
) -> (StatusCode, Value) {
    put_bundle(&env.router, &env.key, source_id, bytes, sha, full).await
}

/// Pushes the incremental bundle for the repo's current HEAD and expects 200.
async fn push_incremental(env: &Env, source_id: Uuid, repo: &TempDir, prev: &str) -> Value {
    let sha = head(repo);
    let (status, report) = push(env, source_id, incremental(repo, prev), &sha, false).await;
    assert_eq!(status, StatusCode::OK, "{report}");
    report
}

#[sqlx::test(migrations = "./migrations")]
async fn first_push_indexes_the_fixture(pool: PgPool) {
    let env = env(pool.clone()).await;
    let (status, created) = create_repo(&env.router, &env.key, "sample").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(created["indexed_commit"], Value::Null);
    let source_id: Uuid = created["id"].as_str().unwrap().parse().unwrap();

    let repo = fixture_repo();
    let sha = head(&repo);
    let (status, report) = push(&env, source_id, full_bundle(&repo), &sha, true).await;
    assert_eq!(status, StatusCode::OK, "{report}");
    assert_eq!(report["indexed_commit"], sha.as_str());
    assert_eq!(report["files_indexed"], 6);
    assert_eq!(report["files_skipped"], 1);
    assert_eq!(report["files_unchanged"], 0);
    assert_eq!(report["files_deleted"], 0);
    assert!(report["chunks"].as_u64().unwrap() >= 6);

    let mut docs = doc_paths(&pool, source_id).await;
    docs.sort();
    assert_eq!(
        docs,
        [
            "README.md",
            "docs/pricing.md",
            "scripts/seed.py",
            "src/api/handlers.go",
            "src/billing/plans.rs",
            "web/src/settings.tsx",
        ]
    );
    assert_eq!(
        chunk_count(&pool, source_id).await as u64,
        report["chunks"].as_u64().unwrap()
    );

    // Document fields: kind by extension, metadata with path, language and commit, no stored uri.
    let (kind, uri, metadata): (String, Option<String>, Value) = sqlx::query_as(
        "SELECT kind, uri, metadata FROM documents WHERE source_id = $1 AND external_id = $2",
    )
    .bind(source_id)
    .bind("src/billing/plans.rs")
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(kind, "code");
    assert_eq!(uri, None);
    assert_eq!(
        metadata,
        json!({"path": "src/billing/plans.rs", "language": "rust", "commit": sha})
    );
    let kind: String = sqlx::query_scalar(
        "SELECT kind FROM documents WHERE source_id = $1 AND external_id = 'README.md'",
    )
    .bind(source_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(kind, "doc");

    // The listing shows the indexed commit.
    let (status, list) = send(&env.router, "GET", "/v1/repos", &env.key, Vec::new()).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(list["repos"][0]["name"], "sample");
    assert_eq!(list["repos"][0]["indexed_commit"], sha.as_str());

    // The mirror and the checkout exist, and the upload is gone from tmp.
    assert!(mirror_dir(env.data.path(), source_id).join("HEAD").exists());
    assert!(
        checkout_dir(env.data.path(), source_id)
            .join("src/billing/plans.rs")
            .is_file()
    );
    assert_eq!(tmp_entries(env.data.path()), 0);
}

#[sqlx::test(migrations = "./migrations")]
async fn push_fixture_helper_indexes_and_returns_the_repo(pool: PgPool) {
    let env = env(pool.clone()).await;
    let (source_id, repo) = push_fixture(&env.router, &env.key).await;
    assert_eq!(doc_paths(&pool, source_id).await.len(), 6);
    let indexed: Option<String> =
        sqlx::query_scalar("SELECT indexed_commit FROM sources WHERE id = $1")
            .bind(source_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(indexed, Some(head(&repo)));
}

#[sqlx::test(migrations = "./migrations")]
async fn pushing_the_same_commit_again_changes_nothing(pool: PgPool) {
    let env = env(pool.clone()).await;
    let (source_id, repo) = push_fixture(&env.router, &env.key).await;
    let sha = head(&repo);
    let (status, report) = push(&env, source_id, full_bundle(&repo), &sha, true).await;
    assert_eq!(status, StatusCode::OK, "{report}");
    assert_eq!(report["files_indexed"], 0);
    assert_eq!(report["files_unchanged"], 6);
    assert_eq!(report["files_skipped"], 1);
    assert_eq!(report["files_deleted"], 0);
}

#[sqlx::test(migrations = "./migrations")]
async fn incremental_push_indexes_only_the_changed_file(pool: PgPool) {
    let env = env(pool.clone()).await;
    let (source_id, repo) = push_fixture(&env.router, &env.key).await;
    let prev = head(&repo);

    let plans = repo.path().join("src/billing/plans.rs");
    let mut text = std::fs::read_to_string(&plans).unwrap();
    text.push_str("\npub fn extra_seats() -> u32 {\n    5\n}\n");
    std::fs::write(&plans, text).unwrap();
    commit_all(&repo, "more seats");

    let report = push_incremental(&env, source_id, &repo, &prev).await;
    assert_eq!(report["files_indexed"], 1);
    assert_eq!(report["files_deleted"], 0);
    assert_eq!(report["files_skipped"], 0);
    assert_eq!(report["indexed_commit"], head(&repo).as_str());
    assert_eq!(doc_paths(&pool, source_id).await.len(), 6);
    let new_text: String = sqlx::query_scalar(
        "SELECT string_agg(c.text, ' ') FROM chunks c JOIN documents d ON d.id = c.document_id
         WHERE d.source_id = $1 AND d.external_id = 'src/billing/plans.rs'",
    )
    .bind(source_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(new_text.contains("extra_seats"));
    let commit: String = sqlx::query_scalar(
        "SELECT metadata->>'commit' FROM documents
         WHERE source_id = $1 AND external_id = 'src/billing/plans.rs'",
    )
    .bind(source_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(commit, head(&repo));
}

#[sqlx::test(migrations = "./migrations")]
async fn deleting_a_file_deletes_its_document_and_checkout_file(pool: PgPool) {
    let env = env(pool.clone()).await;
    let (source_id, repo) = push_fixture(&env.router, &env.key).await;
    let prev = head(&repo);
    assert!(
        checkout_dir(env.data.path(), source_id)
            .join("scripts/seed.py")
            .exists()
    );

    std::fs::remove_file(repo.path().join("scripts/seed.py")).unwrap();
    commit_all(&repo, "drop seed");

    let report = push_incremental(&env, source_id, &repo, &prev).await;
    assert_eq!(report["files_deleted"], 1);
    assert_eq!(report["files_indexed"], 0);
    let docs = doc_paths(&pool, source_id).await;
    assert_eq!(docs.len(), 5);
    assert!(!docs.contains(&"scripts/seed.py".to_string()));
    assert!(
        !checkout_dir(env.data.path(), source_id)
            .join("scripts/seed.py")
            .exists()
    );
    // Its chunks went with the document.
    let orphans: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM chunks c WHERE NOT EXISTS (SELECT 1 FROM documents d WHERE d.id = c.document_id)",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(orphans, 0);
}

#[sqlx::test(migrations = "./migrations")]
async fn a_full_push_deletes_documents_that_are_no_longer_in_the_tree(pool: PgPool) {
    let env = env(pool.clone()).await;
    let (source_id, repo) = push_fixture(&env.router, &env.key).await;
    std::fs::remove_file(repo.path().join("scripts/seed.py")).unwrap();
    commit_all(&repo, "drop seed");
    let sha = head(&repo);

    let (status, report) = push(&env, source_id, full_bundle(&repo), &sha, true).await;
    assert_eq!(status, StatusCode::OK, "{report}");
    assert_eq!(report["files_deleted"], 1);
    assert_eq!(report["files_indexed"], 0);
    assert_eq!(report["files_unchanged"], 5);
    assert!(
        !doc_paths(&pool, source_id)
            .await
            .contains(&"scripts/seed.py".to_string())
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn a_symlink_is_never_indexed_or_followed(pool: PgPool) {
    let env = env(pool.clone()).await;
    let (source_id, repo) = push_fixture(&env.router, &env.key).await;
    let prev = head(&repo);

    std::os::unix::fs::symlink("/etc/hosts", repo.path().join("leak")).unwrap();
    // A tracked file that becomes a symlink must lose its document.
    std::fs::remove_file(repo.path().join("scripts/seed.py")).unwrap();
    std::os::unix::fs::symlink("/etc/hosts", repo.path().join("scripts/seed.py")).unwrap();
    commit_all(&repo, "links");
    assert_eq!(
        git(&repo, &["ls-files", "-s", "leak"])
            .split(' ')
            .next()
            .unwrap(),
        "120000"
    );

    let report = push_incremental(&env, source_id, &repo, &prev).await;
    assert_eq!(report["files_indexed"], 0);
    assert_eq!(report["files_skipped"], 2, "{report}");
    assert_eq!(report["files_deleted"], 1, "{report}");
    let docs = doc_paths(&pool, source_id).await;
    assert!(!docs.contains(&"leak".to_string()));
    assert!(!docs.contains(&"scripts/seed.py".to_string()));

    // Nothing from the target was stored anywhere.
    let leaked: i64 =
        sqlx::query_scalar("SELECT count(*) FROM chunks WHERE text ILIKE '%localhost%'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(leaked, 0);

    // git wrote the link as a plain file holding the link text, not as a symlink.
    let on_disk = checkout_dir(env.data.path(), source_id).join("leak");
    let meta = std::fs::symlink_metadata(&on_disk).unwrap();
    assert!(!meta.file_type().is_symlink());
    assert_eq!(std::fs::read_to_string(&on_disk).unwrap(), "/etc/hosts");

    // A full push of the same tree counts the links as skipped too.
    let sha = head(&repo);
    let (status, report) = push(&env, source_id, full_bundle(&repo), &sha, true).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(report["files_skipped"], 3);
    assert!(
        !doc_paths(&pool, source_id)
            .await
            .contains(&"leak".to_string())
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn a_submodule_entry_is_skipped(pool: PgPool) {
    let env = env(pool.clone()).await;
    let (source_id, repo) = push_fixture(&env.router, &env.key).await;
    let prev = head(&repo);
    git(
        &repo,
        &[
            "update-index",
            "--add",
            "--cacheinfo",
            &format!("160000,{prev},vendored-lib"),
        ],
    );
    git(&repo, &["commit", "-q", "-m", "add gitlink"]);

    let report = push_incremental(&env, source_id, &repo, &prev).await;
    assert_eq!(report["files_indexed"], 0);
    assert_eq!(report["files_skipped"], 1);
    assert!(
        !doc_paths(&pool, source_id)
            .await
            .contains(&"vendored-lib".to_string())
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn skip_rules_apply_to_paths_sizes_and_content(pool: PgPool) {
    let env = env(pool.clone()).await;
    let (source_id, repo) = push_fixture(&env.router, &env.key).await;
    let prev = head(&repo);

    let write = |path: &str, bytes: &[u8]| {
        let p = repo.path().join(path);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, bytes).unwrap();
    };
    write("node_modules/dep/index.js", b"module.exports = 1;\n");
    write("web/dist/app.js", b"var a = 1;\n");
    write("web/app.min.js", b"var a=1;\n");
    write("assets/logo.svg", b"<svg/>\n");
    write("Cargo.lock", b"# lock\n");
    write("big.txt", "x\n".repeat(200 * 1024).as_bytes());
    // A NUL anywhere in the file: past the first 8 KiB, which a prefix check would miss.
    let mut late_nul = "a line of text\n".repeat(1000).into_bytes();
    late_nul.push(0);
    late_nul.extend_from_slice(b"tail\n");
    write("late-nul.txt", &late_nul);
    write("latin1.txt", b"caf\xe9 au lait\n");
    write("kept/ok.md", b"# Kept\n\nThis file is indexed.\n");
    commit_all(&repo, "skippable files");

    let report = push_incremental(&env, source_id, &repo, &prev).await;
    assert_eq!(report["files_indexed"], 1, "{report}");
    assert_eq!(report["files_skipped"], 8, "{report}");
    let docs = doc_paths(&pool, source_id).await;
    assert!(docs.contains(&"kept/ok.md".to_string()));
    for skipped in [
        "node_modules/dep/index.js",
        "web/dist/app.js",
        "web/app.min.js",
        "assets/logo.svg",
        "Cargo.lock",
        "big.txt",
        "late-nul.txt",
        "latin1.txt",
    ] {
        assert!(
            !docs.contains(&skipped.to_string()),
            "{skipped} was indexed"
        );
    }
}

#[sqlx::test(migrations = "./migrations")]
async fn a_file_that_grows_past_the_limit_loses_its_document(pool: PgPool) {
    let env = env(pool.clone()).await;
    let (source_id, repo) = push_fixture(&env.router, &env.key).await;
    let prev = head(&repo);
    std::fs::write(
        repo.path().join("README.md"),
        "line of readme\n".repeat(30_000),
    )
    .unwrap();
    commit_all(&repo, "huge readme");
    let report = push_incremental(&env, source_id, &repo, &prev).await;
    assert_eq!(report["files_skipped"], 1);
    assert_eq!(report["files_deleted"], 1);
    assert!(
        !doc_paths(&pool, source_id)
            .await
            .contains(&"README.md".to_string())
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn odd_paths_are_read_by_object_not_taken_for_options(pool: PgPool) {
    let env = env(pool.clone()).await;
    let (source_id, repo) = push_fixture(&env.router, &env.key).await;
    let prev = head(&repo);

    let names = [
        "-rf.md",
        "--upload-pack=x.md",
        "with space.md",
        "new\nline.md",
        "dir/-n.rs",
    ];
    for (i, name) in names.iter().enumerate() {
        let p = repo.path().join(name);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, format!("# Title {i}\n\nbody {i}\n")).unwrap();
    }
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-q", "-m", "odd names"]);

    let report = push_incremental(&env, source_id, &repo, &prev).await;
    assert_eq!(report["files_indexed"], 5, "{report}");
    assert_eq!(report["files_skipped"], 0, "{report}");
    let docs = doc_paths(&pool, source_id).await;
    for name in names {
        assert!(
            docs.contains(&name.to_string()),
            "{name:?} missing from {docs:?}"
        );
    }
    let body: String = sqlx::query_scalar(
        "SELECT string_agg(c.text, ' ') FROM chunks c JOIN documents d ON d.id = c.document_id
         WHERE d.source_id = $1 AND d.external_id = '--upload-pack=x.md'",
    )
    .bind(source_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(body.contains("body 1"));
}

#[sqlx::test(migrations = "./migrations")]
async fn a_wrong_ref_is_a_400_and_indexes_nothing(pool: PgPool) {
    let env = env(pool.clone()).await;
    let (_, created) = create_repo(&env.router, &env.key, "sample").await;
    let source_id: Uuid = created["id"].as_str().unwrap().parse().unwrap();
    let repo = fixture_repo();
    let wrong = "0123456789abcdef0123456789abcdef01234567";

    let (status, body) = push(&env, source_id, full_bundle(&repo), wrong, true).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(doc_paths(&pool, source_id).await.is_empty());
    let indexed: Option<String> =
        sqlx::query_scalar("SELECT indexed_commit FROM sources WHERE id = $1")
            .bind(source_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(indexed, None);
    assert_eq!(tmp_entries(env.data.path()), 0);

    // Refs that are not 40 lowercase hex characters never reach git: the source of these pushes
    // never gets a mirror.
    let (_, created) = create_repo(&env.router, &env.key, "untouched").await;
    let untouched: Uuid = created["id"].as_str().unwrap().parse().unwrap();
    let sha = head(&repo);
    for bad in [
        "HEAD".to_string(),
        "main".to_string(),
        sha.to_uppercase(),
        sha[..39].to_string(),
        format!("{sha}0"),
        "--upload-pack=touch%20/tmp/pwned".to_string(),
        format!("{sha}%0a"),
    ] {
        let (status, body) = push(&env, untouched, full_bundle(&repo), &bad, true).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "ref {bad:?}: {body}");
    }
    assert!(
        !env.data
            .path()
            .join("repos")
            .join(untouched.to_string())
            .exists()
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn a_bad_query_is_a_400(pool: PgPool) {
    let env = env(pool.clone()).await;
    let (source_id, repo) = push_fixture(&env.router, &env.key).await;
    let sha = head(&repo);
    for query in ["", "?full=true", &format!("?ref={sha}&full=maybe")] {
        let req = Request::put(format!("/v1/repos/{source_id}/bundle{query}"))
            .header("authorization", format!("Bearer {}", env.key))
            .body(Body::from(full_bundle(&repo)))
            .unwrap();
        let res = env.router.clone().oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::BAD_REQUEST, "query {query:?}");
        assert!(body_json(res).await["error"].is_string());
    }
}

#[sqlx::test(migrations = "./migrations")]
async fn a_lost_mirror_needs_a_full_bundle(pool: PgPool) {
    let env = env(pool.clone()).await;
    let (source_id, repo) = push_fixture(&env.router, &env.key).await;
    let prev = head(&repo);
    std::fs::write(repo.path().join("NOTES.md"), "# Notes\n\nfresh\n").unwrap();
    commit_all(&repo, "notes");
    let sha = head(&repo);

    std::fs::remove_dir_all(env.data.path().join("repos")).unwrap();

    let (status, body) = push(&env, source_id, incremental(&repo, &prev), &sha, false).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body, json!({"error": "need_full_bundle"}));
    // The failed push changed nothing in the index.
    let indexed: Option<String> =
        sqlx::query_scalar("SELECT indexed_commit FROM sources WHERE id = $1")
            .bind(source_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(indexed, Some(prev.clone()));
    assert_eq!(tmp_entries(env.data.path()), 0);

    let (status, report) = push(&env, source_id, full_bundle(&repo), &sha, true).await;
    assert_eq!(status, StatusCode::OK, "{report}");
    assert_eq!(report["indexed_commit"], sha.as_str());
    assert_eq!(report["files_indexed"], 1);
    assert_eq!(report["files_unchanged"], 6);
    assert!(
        doc_paths(&pool, source_id)
            .await
            .contains(&"NOTES.md".to_string())
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn a_mirror_without_the_indexed_commit_needs_a_full_bundle(pool: PgPool) {
    let env = env(pool.clone()).await;
    let (source_id, repo) = push_fixture(&env.router, &env.key).await;
    let _ = repo;
    std::fs::remove_dir_all(env.data.path().join("repos")).unwrap();

    // An unrelated history: the bundle applies on its own, but the indexed commit is not in the
    // fresh mirror, so there is nothing to diff from.
    let other = fixture_repo();
    std::fs::write(other.path().join("NOTES.md"), "# Notes\n").unwrap();
    git(&other, &["add", "-A"]);
    git(&other, &["commit", "-q", "--amend", "-m", "other history"]);
    let sha = head(&other);
    let (status, body) = push(&env, source_id, full_bundle(&other), &sha, false).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["error"], "need_full_bundle");
}

#[sqlx::test(migrations = "./migrations")]
async fn a_body_over_the_limit_is_a_413(pool: PgPool) {
    let env = env_with_limit(pool.clone(), Some(512)).await;
    let (_, created) = create_repo(&env.router, &env.key, "sample").await;
    let source_id: Uuid = created["id"].as_str().unwrap().parse().unwrap();
    let repo = fixture_repo();
    let sha = head(&repo);
    let bytes = full_bundle(&repo);
    assert!(
        bytes.len() > 512,
        "the fixture bundle must exceed the test limit"
    );

    // With a known length.
    let (status, _) = push(&env, source_id, bytes.clone(), &sha, true).await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);

    // Streamed with no length: the limit trips in the middle of the body.
    let chunks: Vec<Result<Bytes, std::io::Error>> = bytes
        .chunks(128)
        .map(|c| Ok(Bytes::copy_from_slice(c)))
        .collect();
    let req = Request::put(format!("/v1/repos/{source_id}/bundle?ref={sha}&full=true"))
        .header("authorization", format!("Bearer {}", env.key))
        .body(Body::from_stream(futures::stream::iter(chunks)))
        .unwrap();
    let res = env.router.clone().oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(body_json(res).await, json!({"error": "payload too large"}));

    assert!(doc_paths(&pool, source_id).await.is_empty());
    assert_eq!(
        tmp_entries(env.data.path()),
        0,
        "temp files must be cleaned up"
    );

    // A body under the limit still works on the same router.
    let (status, _) = push(&env, source_id, vec![b'x'; 100], &sha, true).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[sqlx::test(migrations = "./migrations")]
async fn the_limit_applies_to_the_bundle_route_only(pool: PgPool) {
    let env = env_with_limit(pool.clone(), Some(64)).await;
    let name = "n".repeat(90);
    let (status, body) = create_repo(&env.router, &env.key, &name).await;
    assert_eq!(status, StatusCode::OK, "{body}");
}

#[sqlx::test(migrations = "./migrations")]
async fn uploads_that_are_not_bundles_are_a_400(pool: PgPool) {
    let env = env(pool.clone()).await;
    let (source_id, repo) = push_fixture(&env.router, &env.key).await;
    let sha = head(&repo);
    let good = full_bundle(&repo);

    // A gitfile would make git read another repository on this host.
    let gitfile = format!("gitdir: {}\n", repo.path().join(".git").display()).into_bytes();
    let half = good[..good.len() / 2].to_vec();
    let mut header_only = good[..16].to_vec();
    header_only.extend_from_slice(b"garbage that is not a bundle\n");
    let cases: Vec<(&str, Vec<u8>)> = vec![
        ("empty", Vec::new()),
        ("text", b"hello\n".to_vec()),
        ("gitfile", gitfile),
        ("truncated", half),
        ("header only", header_only),
    ];
    for (name, bytes) in cases {
        // A fresh source each time: a mirror that already has the commit would not need the pack.
        let (_, created) = create_repo(&env.router, &env.key, &name.replace(' ', "-")).await;
        let fresh: Uuid = created["id"].as_str().unwrap().parse().unwrap();
        let (status, body) = push(&env, fresh, bytes, &sha, true).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{name}: {body}");
        assert_eq!(
            tmp_entries(env.data.path()),
            0,
            "{name}: temp file left behind"
        );
    }
    // The index is untouched by the failed pushes.
    assert_eq!(doc_paths(&pool, source_id).await.len(), 6);
}

#[sqlx::test(migrations = "./migrations")]
async fn git_failures_are_500_without_leaking_git_output(pool: PgPool) {
    let env = env(pool.clone()).await;
    let (_, created) = create_repo(&env.router, &env.key, "sample").await;
    let source_id: Uuid = created["id"].as_str().unwrap().parse().unwrap();
    // A mirror directory that is not a repository: git fails on it, which is the server's problem.
    let mirror = mirror_dir(env.data.path(), source_id);
    std::fs::create_dir_all(&mirror).unwrap();
    std::fs::write(mirror.join("HEAD"), "not a ref\n").unwrap();

    let repo = fixture_repo();
    let sha = head(&repo);
    let (status, body) = push(&env, source_id, full_bundle(&repo), &sha, true).await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR, "{body}");
    assert_eq!(body, json!({"error": "internal error"}));
    assert_eq!(tmp_entries(env.data.path()), 0);
}

#[sqlx::test(migrations = "./migrations")]
async fn another_tenants_source_is_a_404(pool: PgPool) {
    let env = env(pool.clone()).await;
    let (source_id, repo) = push_fixture(&env.router, &env.key).await;
    let (_, other_key) = create_tenant_key(&env.router, "other", &["ingest", "read"]).await;
    let sha = head(&repo);

    let (status, body) = put_bundle(
        &env.router,
        &other_key,
        source_id,
        full_bundle(&repo),
        &sha,
        true,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    // The same answer as for an id that does not exist at all.
    let (status, missing) = put_bundle(
        &env.router,
        &other_key,
        Uuid::new_v4(),
        full_bundle(&repo),
        &sha,
        true,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body, missing);

    let (status, list) = send(&env.router, "GET", "/v1/repos", &other_key, Vec::new()).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(list["repos"], json!([]));
    // The other tenant can use the same repo name for its own source.
    let (status, theirs) = create_repo(&env.router, &other_key, "sample").await;
    assert_eq!(status, StatusCode::OK);
    assert_ne!(theirs["id"].as_str().unwrap(), source_id.to_string());
    assert_eq!(theirs["indexed_commit"], Value::Null);
    // And the first tenant's documents did not change.
    assert_eq!(doc_paths(&pool, source_id).await.len(), 6);
}

#[sqlx::test(migrations = "./migrations")]
async fn a_source_that_is_not_a_repo_is_a_404(pool: PgPool) {
    let env = env(pool.clone()).await;
    let (tenant_id, key) = create_tenant_key(&env.router, "notes", &["ingest", "read"]).await;
    let docs = lore::store::ensure_source(&pool, tenant_id, "documents", "wiki", None)
        .await
        .unwrap();
    let repo = fixture_repo();
    let sha = head(&repo);
    let (status, _) = put_bundle(&env.router, &key, docs.id, full_bundle(&repo), &sha, true).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    // Only repo sources are listed.
    let (_, list) = send(&env.router, "GET", "/v1/repos", &key, Vec::new()).await;
    assert_eq!(list["repos"], json!([]));
}

#[sqlx::test(migrations = "./migrations")]
async fn scopes_are_enforced(pool: PgPool) {
    let env = env(pool.clone()).await;
    let (source_id, repo) = push_fixture(&env.router, &env.key).await;
    let (_, read_key) = create_tenant_key(&env.router, "reader", &["read"]).await;
    let (_, ingest_key) = create_tenant_key(&env.router, "writer", &["ingest"]).await;
    let sha = head(&repo);

    let (status, _) = create_repo(&env.router, &read_key, "x").await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _) = put_bundle(
        &env.router,
        &read_key,
        source_id,
        full_bundle(&repo),
        &sha,
        true,
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _) = send(&env.router, "GET", "/v1/repos", &read_key, Vec::new()).await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = send(&env.router, "GET", "/v1/repos", &ingest_key, Vec::new()).await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    // No key at all.
    let res = env
        .router
        .clone()
        .oneshot(Request::post("/v1/repos").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
}

#[sqlx::test(migrations = "./migrations")]
async fn creating_and_listing_repos(pool: PgPool) {
    let env = env(pool.clone()).await;
    let body =
        json!({"name": "web", "web_url_template": "https://git.example/web/blob/{commit}/{path}"});
    let (status, first) = send(
        &env.router,
        "POST",
        "/v1/repos",
        &env.key,
        body.to_string().into_bytes(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{first}");
    assert_eq!(first["name"], "web");
    assert_eq!(
        first["web_url_template"],
        "https://git.example/web/blob/{commit}/{path}"
    );

    // Creating it again returns the same source and keeps the template.
    let (status, again) = create_repo(&env.router, &env.key, "web").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(again["id"], first["id"]);
    assert_eq!(again["web_url_template"], first["web_url_template"]);

    create_repo(&env.router, &env.key, "api").await;
    let (_, list) = send(&env.router, "GET", "/v1/repos", &env.key, Vec::new()).await;
    let names: Vec<&str> = list["repos"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["api", "web"]);

    for bad in ["", "a/b", "a b", "../x", &"a".repeat(101)] {
        let (status, _) = create_repo(&env.router, &env.key, bad).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "name {bad:?}");
    }
    let (status, _) = create_repo(&env.router, &env.key, &"a".repeat(100)).await;
    assert_eq!(status, StatusCode::OK);
    let bad_template = json!({"name": "t", "web_url_template": "javascript:alert(1)"});
    let (status, _) = send(
        &env.router,
        "POST",
        "/v1/repos",
        &env.key,
        bad_template.to_string().into_bytes(),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, _) = send(
        &env.router,
        "POST",
        "/v1/repos",
        &env.key,
        b"not json".to_vec(),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[sqlx::test(migrations = "./migrations")]
async fn concurrent_pushes_to_one_source_are_serialised(pool: PgPool) {
    let env = env(pool.clone()).await;
    let (_, created) = create_repo(&env.router, &env.key, "sample").await;
    let source_id: Uuid = created["id"].as_str().unwrap().parse().unwrap();
    let repo = fixture_repo();
    let sha = head(&repo);
    let bytes = full_bundle(&repo);

    let pushes = (0..4).map(|_| push(&env, source_id, bytes.clone(), &sha, true));
    let results = futures::future::join_all(pushes).await;
    for (status, body) in &results {
        assert_eq!(*status, StatusCode::OK, "{body}");
    }
    // Exactly one of them did the indexing, the others found it unchanged.
    let indexed: u64 = results
        .iter()
        .map(|(_, b)| b["files_indexed"].as_u64().unwrap())
        .sum();
    assert_eq!(indexed, 6);
    assert_eq!(doc_paths(&pool, source_id).await.len(), 6);
}

#[sqlx::test(migrations = "./migrations")]
async fn the_lock_map_has_one_entry_per_source(pool: PgPool) {
    let env = env(pool.clone()).await;
    assert!(env.state.source_locks.is_empty());
    let (first, repo) = push_fixture(&env.router, &env.key).await;
    let sha = head(&repo);
    for _ in 0..3 {
        push(&env, first, full_bundle(&repo), &sha, true).await;
    }
    // Pushes that fail before ingest (bad ref, unknown source, other tenant) add no entries.
    push(&env, first, full_bundle(&repo), "nope", true).await;
    let (_, other_key) = create_tenant_key(&env.router, "other", &["ingest"]).await;
    put_bundle(
        &env.router,
        &other_key,
        first,
        full_bundle(&repo),
        &sha,
        true,
    )
    .await;
    put_bundle(
        &env.router,
        &env.key,
        Uuid::new_v4(),
        full_bundle(&repo),
        &sha,
        true,
    )
    .await;
    assert_eq!(env.state.source_locks.len(), 1);

    let (_, created) = create_repo(&env.router, &env.key, "second").await;
    let second: Uuid = created["id"].as_str().unwrap().parse().unwrap();
    push(&env, second, full_bundle(&repo), &sha, true).await;
    assert_eq!(env.state.source_locks.len(), 2);
}

#[sqlx::test(migrations = "./migrations")]
async fn the_ingest_finishes_when_the_client_goes_away(pool: PgPool) {
    let data = tempfile::tempdir().unwrap();
    let state = test_state(pool.clone(), data.path());
    let router = test_router(state.clone());
    let (_, key) = create_tenant_key(&router, "acme", &["ingest", "read"]).await;
    let (_, created) = create_repo(&router, &key, "sample").await;
    let source_id: Uuid = created["id"].as_str().unwrap().parse().unwrap();
    let repo = fixture_repo();
    let sha = head(&repo);
    let bytes = full_bundle(&repo);

    // Hold the source lock so the ingest cannot start, send the push, wait until the upload is
    // on disk, then drop the request as a disconnecting client would.
    let guard_lock = state.source_locks.get(source_id);
    let guard = guard_lock.lock().await;
    let client = {
        let (router, key, sha) = (router.clone(), key.clone(), sha.clone());
        tokio::spawn(async move { put_bundle(&router, &key, source_id, bytes, &sha, true).await })
    };
    let mut waited = 0;
    while tmp_entries(data.path()) == 0 && waited < 100 {
        tokio::time::sleep(Duration::from_millis(50)).await;
        waited += 1;
    }
    assert_eq!(tmp_entries(data.path()), 1, "the upload should be spooled");
    tokio::time::sleep(Duration::from_millis(300)).await;
    client.abort();
    let _ = client.await;
    drop(guard);

    // The ingest still runs to the end and removes its upload.
    let mut done = false;
    for _ in 0..200 {
        let indexed: Option<String> =
            sqlx::query_scalar("SELECT indexed_commit FROM sources WHERE id = $1")
                .bind(source_id)
                .fetch_one(&pool)
                .await
                .unwrap();
        if indexed.as_deref() == Some(sha.as_str()) && tmp_entries(data.path()) == 0 {
            done = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(done, "the ingest did not finish after the client left");
    assert_eq!(doc_paths(&pool, source_id).await.len(), 6);
}
