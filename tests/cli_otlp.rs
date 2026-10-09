mod common;

use std::path::PathBuf;

use common::{create_tenant_key, serve_router, test_router, test_state};
use sqlx::PgPool;
use tokio::process::Command;
use uuid::Uuid;

const FIXTURE: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/otlp/claude_code_session.json"
);

/// A server on a local port with one tenant, and keys for it.
struct Env {
    base: String,
    tenant: Uuid,
    key: String,
    read_key: String,
    pool: PgPool,
}

/// What a `lore` run printed and whether it exited zero.
struct Run {
    ok: bool,
    stdout: String,
    stderr: String,
}

impl Env {
    async fn new(pool: PgPool) -> Env {
        let data = tempfile::tempdir().unwrap();
        let router = test_router(test_state(pool.clone(), data.path()));
        let (tenant, key) = create_tenant_key(&router, "acme", &["ingest", "read"]).await;
        let (_, read_key) = create_tenant_key(&router, "reader", &["read"]).await;
        let base = serve_router(router).await;
        // The server needs no files here, so the data dir can go.
        drop(data);
        Env {
            base,
            tenant,
            key,
            read_key,
            pool,
        }
    }

    /// Runs the built binary with `--url`, `--key` (when given) and the arguments.
    async fn lore(&self, key: Option<&str>, args: &[&str]) -> Run {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_lore"));
        cmd.args(["--url", &self.base]);
        if let Some(key) = key {
            cmd.args(["--key", key]);
        }
        let out = cmd
            .args(args)
            .env_remove("LORE_URL")
            .env_remove("LORE_KEY")
            .env_remove("LORE_ADMIN_TOKEN")
            .output()
            .await
            .expect("the lore binary runs");
        Run {
            ok: out.status.success(),
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        }
    }

    async fn span_count(&self) -> i64 {
        sqlx::query_scalar("SELECT count(*) FROM spans WHERE tenant_id = $1")
            .bind(self.tenant)
            .fetch_one(&self.pool)
            .await
            .unwrap()
    }
}

#[sqlx::test(migrations = "./migrations")]
async fn send_posts_json_protobuf_and_gzip(pool: PgPool) {
    let env = Env::new(pool).await;
    for extra in [
        &[][..],
        &["--protobuf"],
        &["--gzip"],
        &["--protobuf", "--gzip"],
    ] {
        let mut args = vec!["otlp", "send", FIXTURE];
        args.extend_from_slice(extra);
        let run = env.lore(Some(&env.key), &args).await;
        assert!(run.ok, "{extra:?} failed: {}", run.stderr);
        assert_eq!(run.stdout.trim(), "HTTP 200", "{extra:?}");
        assert_eq!(env.span_count().await, 5, "{extra:?}");
    }
}

#[sqlx::test(migrations = "./migrations")]
async fn a_non_2xx_reply_is_a_failure(pool: PgPool) {
    let env = Env::new(pool).await;

    // A read-only key: the server answers 403, the status is still printed.
    let run = env
        .lore(Some(&env.read_key), &["otlp", "send", FIXTURE])
        .await;
    assert!(!run.ok);
    assert_eq!(run.stdout.trim(), "HTTP 403");
    assert!(run.stderr.contains("403"), "{}", run.stderr);

    // A wrong key is a 401.
    let run = env.lore(Some("lk_wrong"), &["otlp", "send", FIXTURE]).await;
    assert!(!run.ok);
    assert_eq!(run.stdout.trim(), "HTTP 401");
    assert_eq!(env.span_count().await, 0);
}

#[sqlx::test(migrations = "./migrations")]
async fn missing_input_is_a_failure(pool: PgPool) {
    let env = Env::new(pool).await;

    let missing: PathBuf = std::env::temp_dir().join("lore-no-such-otlp-file.json");
    let run = env
        .lore(Some(&env.key), &["otlp", "send", missing.to_str().unwrap()])
        .await;
    assert!(!run.ok);
    assert!(run.stdout.is_empty());

    let run = env.lore(None, &["otlp", "send", FIXTURE]).await;
    assert!(!run.ok);
    assert!(run.stderr.contains("--key"), "{}", run.stderr);

    // Not JSON: with --protobuf the CLI refuses to send it.
    let dir = tempfile::tempdir().unwrap();
    let bad = dir.path().join("bad.json");
    std::fs::write(&bad, "not json").unwrap();
    let run = env
        .lore(
            Some(&env.key),
            &["otlp", "send", bad.to_str().unwrap(), "--protobuf"],
        )
        .await;
    assert!(!run.ok);
    assert!(run.stdout.is_empty());

    // Sent as JSON, the server answers 400 and the CLI fails.
    let run = env
        .lore(Some(&env.key), &["otlp", "send", bad.to_str().unwrap()])
        .await;
    assert!(!run.ok);
    assert_eq!(run.stdout.trim(), "HTTP 400");
}
