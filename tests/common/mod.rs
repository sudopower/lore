#![allow(dead_code)]

use std::path::Path;
use std::process::Command;
use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use lore::config::Config;
use lore::embed::HashEmbedder;
use lore::http::{AppState, router};
use serde_json::{Value, json};
use sqlx::PgPool;
use tower::ServiceExt;
use uuid::Uuid;

pub fn test_config(data_dir: &Path) -> Config {
    Config::from_lookup(|k| match k {
        "LORE_DATA_DIR" => Some(data_dir.to_string_lossy().into_owned()),
        "LORE_ADMIN_TOKEN" => Some("t".to_string()),
        _ => None,
    })
    .expect("test config is valid")
}

pub fn test_state(pool: PgPool, data_dir: &Path) -> AppState {
    AppState {
        pool,
        config: Arc::new(test_config(data_dir)),
        embedder: Arc::new(HashEmbedder),
    }
}

pub fn test_router(state: AppState) -> Router {
    router(state)
}

/// Creates a tenant and one key with the given scopes through the admin routes (admin token
/// `"t"`). Returns the tenant id and the plaintext key.
pub async fn create_tenant_key(router: &Router, name: &str, scopes: &[&str]) -> (Uuid, String) {
    let req = Request::post("/v1/admin/tenants")
        .header("authorization", "Bearer t")
        .header("content-type", "application/json")
        .body(Body::from(json!({ "name": name }).to_string()))
        .unwrap();
    let res = router.clone().oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::CREATED, "create tenant {name}");
    let tenant: Value = body_json(res).await;
    let tenant_id: Uuid = tenant["id"].as_str().unwrap().parse().unwrap();

    let req = Request::post(format!("/v1/admin/tenants/{tenant_id}/keys"))
        .header("authorization", "Bearer t")
        .header("content-type", "application/json")
        .body(Body::from(json!({ "scopes": scopes }).to_string()))
        .unwrap();
    let res = router.clone().oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::CREATED, "create key for {name}");
    let key: Value = body_json(res).await;
    (tenant_id, key["key"].as_str().unwrap().to_string())
}

/// Runs `git` in `dir` and returns its stdout. The host's git configuration (signing, hooks,
/// templates, `init.defaultBranch`) is ignored, so the helpers behave the same on every machine.
pub fn git(dir: impl AsRef<Path>, args: &[&str]) -> String {
    let out = Command::new("git")
        .current_dir(dir.as_ref())
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .args([
            "-c",
            "commit.gpgsign=false",
            "-c",
            "core.hooksPath=/dev/null",
        ])
        .args(args)
        .output()
        .expect("git runs");
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).expect("git output is utf-8")
}

fn copy_dir(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).expect("create fixture dir");
    for entry in std::fs::read_dir(from).expect("read fixture dir") {
        let entry = entry.expect("fixture entry");
        let dest = to.join(entry.file_name());
        if entry.file_type().expect("fixture entry type").is_dir() {
            copy_dir(&entry.path(), &dest);
        } else {
            std::fs::copy(entry.path(), &dest).expect("copy fixture file");
        }
    }
}

/// The sample repo (`tests/fixtures/sample-repo`) copied into a temp dir and committed on `main`
/// as `init`.
pub fn fixture_repo() -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("temp dir");
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/sample-repo");
    copy_dir(&src, dir.path());
    git(&dir, &["init", "-q", "-b", "main"]);
    git(&dir, &["config", "user.email", "t@t"]);
    git(&dir, &["config", "user.name", "t"]);
    commit_all(&dir, "init");
    dir
}

/// Stages every change in `dir` and commits it.
pub fn commit_all(dir: impl AsRef<Path>, msg: &str) {
    let dir = dir.as_ref();
    git(dir, &["add", "-A"]);
    git(dir, &["commit", "-q", "-m", msg]);
}

/// `git bundle create` with `range` (for example `["main"]` or `["<sha>..main"]`); returns the
/// bundle bytes.
pub fn bundle(dir: impl AsRef<Path>, range: &[&str]) -> Vec<u8> {
    let out = tempfile::tempdir().expect("temp dir");
    let file = out.path().join("repo.bundle");
    let file_arg = file.to_string_lossy().into_owned();
    let mut args = vec!["bundle", "create", "-q", file_arg.as_str()];
    args.extend_from_slice(range);
    git(dir, &args);
    std::fs::read(&file).expect("read bundle")
}

/// The full sha of `HEAD`.
pub fn head(dir: impl AsRef<Path>) -> String {
    git(dir, &["rev-parse", "HEAD"]).trim().to_string()
}

pub async fn body_json(res: axum::response::Response) -> Value {
    let bytes = axum::body::to_bytes(res.into_body(), 1024 * 1024)
        .await
        .unwrap();
    serde_json::from_slice(&bytes).unwrap_or(Value::Null)
}
