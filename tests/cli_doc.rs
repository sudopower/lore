mod common;

use common::{create_tenant_key, serve_router, test_router, test_state};
use serde_json::Value;
use sqlx::PgPool;
use tokio::process::Command;

/// A server on a local port with one tenant, and keys for it.
struct Env {
    base: String,
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
        let (_, key) = create_tenant_key(&router, "acme", &["ingest", "read"]).await;
        let (_, read_key) = create_tenant_key(&router, "reader", &["read"]).await;
        let base = serve_router(router).await;
        // The server needs no files here, so the data dir can go.
        drop(data);
        Env {
            base,
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

    async fn documents(&self) -> i64 {
        sqlx::query_scalar("SELECT count(*) FROM documents")
            .fetch_one(&self.pool)
            .await
            .unwrap()
    }

    async fn document_id(&self, external_id: &str) -> String {
        let id: uuid::Uuid = sqlx::query_scalar("SELECT id FROM documents WHERE external_id = $1")
            .bind(external_id)
            .fetch_one(&self.pool)
            .await
            .unwrap();
        id.to_string()
    }
}

const PUT: [&str; 7] = ["doc", "put", "--source", "notes", "--id", "n1", "--title"];

#[sqlx::test(migrations = "./migrations")]
async fn put_get_and_rm_round_trip(pool: PgPool) {
    let env = Env::new(pool).await;
    let mut args = PUT.to_vec();
    args.extend(["Review", "--text", "The zorblax review moved."]);
    let run = env.lore(Some(&env.key), &args).await;
    assert!(run.ok, "{}", run.stderr);
    let report: Value = serde_json::from_str(&run.stdout).unwrap();
    assert_eq!(report["upserted"], 1);
    assert_eq!(report["unchanged"], 0);

    // The same put again is skipped.
    let run = env.lore(Some(&env.key), &args).await;
    let report: Value = serde_json::from_str(&run.stdout).unwrap();
    assert_eq!(report["unchanged"], 1);

    let id = env.document_id("n1").await;
    let run = env.lore(Some(&env.key), &["doc", "get", &id]).await;
    assert!(run.ok, "{}", run.stderr);
    assert_eq!(run.stdout, "The zorblax review moved.\n");

    let run = env
        .lore(
            Some(&env.key),
            &["doc", "rm", "--source", "notes", "--id", "n1"],
        )
        .await;
    assert!(run.ok, "{}", run.stderr);
    assert_eq!(env.documents().await, 0);

    // Removing it again is a 404.
    let run = env
        .lore(
            Some(&env.key),
            &["doc", "rm", "--source", "notes", "--id", "n1"],
        )
        .await;
    assert!(!run.ok);
    assert!(run.stderr.contains("404"), "{}", run.stderr);
    let run = env.lore(Some(&env.key), &["doc", "get", &id]).await;
    assert!(!run.ok);
    assert!(run.stderr.contains("404"), "{}", run.stderr);
}

#[sqlx::test(migrations = "./migrations")]
async fn put_reads_a_file_with_kind_and_uri(pool: PgPool) {
    let env = Env::new(pool).await;
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("guide.md");
    std::fs::write(&file, "# Guide\nHow the zorblax works.\n").unwrap();

    let run = env
        .lore(
            Some(&env.key),
            &[
                "doc",
                "put",
                "--source",
                "wiki",
                "--id",
                "team/guide v2?",
                "--title",
                "Guide",
                "--file",
                file.to_str().unwrap(),
                "--kind",
                "doc",
                "--uri",
                "https://example.com/guide",
            ],
        )
        .await;
    assert!(run.ok, "{}", run.stderr);
    let (kind, uri): (String, Option<String>) =
        sqlx::query_as("SELECT kind, uri FROM documents WHERE external_id = 'team/guide v2?'")
            .fetch_one(&env.pool)
            .await
            .unwrap();
    assert_eq!(kind, "doc");
    assert_eq!(uri.as_deref(), Some("https://example.com/guide"));

    // The id has a slash, a space and a question mark; rm must reach it.
    let run = env
        .lore(
            Some(&env.key),
            &["doc", "rm", "--source", "wiki", "--id", "team/guide v2?"],
        )
        .await;
    assert!(run.ok, "{}", run.stderr);
    assert_eq!(env.documents().await, 0);
}

#[sqlx::test(migrations = "./migrations")]
async fn put_needs_exactly_one_of_file_and_text(pool: PgPool) {
    let env = Env::new(pool).await;
    let mut args = PUT.to_vec();
    args.push("T");
    let run = env.lore(Some(&env.key), &args).await;
    assert!(!run.ok);
    assert!(run.stderr.contains("required"), "{}", run.stderr);

    args.extend(["--text", "x", "--file", "/tmp/whatever"]);
    let run = env.lore(Some(&env.key), &args).await;
    assert!(!run.ok);
    assert!(run.stderr.contains("cannot be used"), "{}", run.stderr);
    assert_eq!(env.documents().await, 0);
}

#[sqlx::test(migrations = "./migrations")]
async fn failures_are_reported(pool: PgPool) {
    let env = Env::new(pool).await;
    let mut args = PUT.to_vec();
    args.extend(["T", "--text", "x"]);

    // A read-only key is a 403.
    let run = env.lore(Some(&env.read_key), &args).await;
    assert!(!run.ok);
    assert!(run.stderr.contains("403"), "{}", run.stderr);

    let run = env.lore(None, &args).await;
    assert!(!run.ok);
    assert!(run.stderr.contains("--key"), "{}", run.stderr);

    // A bad kind is refused before anything is sent.
    let mut bad = args.clone();
    bad.extend(["--kind", "session"]);
    let run = env.lore(Some(&env.key), &bad).await;
    assert!(!run.ok);

    // A missing file is reported, nothing is sent.
    let mut missing = PUT.to_vec();
    missing.extend(["T", "--file", "/tmp/lore-no-such-doc-file.md"]);
    let run = env.lore(Some(&env.key), &missing).await;
    assert!(!run.ok);
    assert!(run.stdout.is_empty());

    // The server's 400 for a bad source name comes through.
    let run = env
        .lore(
            Some(&env.key),
            &[
                "doc", "put", "--source", "a b", "--id", "n", "--title", "T", "--text", "x",
            ],
        )
        .await;
    assert!(!run.ok);
    assert!(run.stderr.contains("400"), "{}", run.stderr);
    assert_eq!(env.documents().await, 0);
}
