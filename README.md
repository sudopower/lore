# lore

Ask questions of your agents' traces, your coding sessions and your codebases, and get answers with citations.

- **fast** mode: one retrieval and one model call, an answer in seconds (for standups and meetings).
- **deep** mode: an agent loop with `search`, `grep` and `read_file` tools, under a time budget.

Inputs: OpenTelemetry GenAI traces over OTLP/HTTP, git repositories, and a plain documents API.
Surfaces: a `lore` CLI, an MCP server, and a small built-in web page.

Written in Rust. Status: early development; the server, health checks and Postgres wiring exist.

## Development

You need Rust 1.95 (pinned in `rust-toolchain.toml`) and Docker. Postgres 17 with pgvector runs in Docker, never on the host.

```bash
make db-up      # Postgres + pgvector on localhost:5434 (user, password, db: lore)
make run        # lore serve on http://localhost:8080 (hash embedder, fake model)
make test       # cargo test --all-targets (DB tests read DATABASE_URL, set by the Makefile)
make lint       # cargo fmt --check and cargo clippy -D warnings
make check      # lint + test: the pre-push gate, and what CI runs
make image VERSION=dev   # docker build, tagged lore:dev
```

`docker compose up -d --build` starts both Postgres and lore in containers.
Check it with `curl localhost:8080/healthz`, which returns `{"status":"ok","db":"ok"}`.
`/livez` is the liveness probe and does not touch the database.

## Configuration

lore reads these environment variables (all in `src/config.rs`).

| Variable | Default | Meaning |
|---|---|---|
| `LORE_DATABASE_URL` | `postgres://lore:lore@localhost:5434/lore` | Postgres connection string |
| `LORE_LISTEN_ADDR` | `0.0.0.0:8080` | Address the HTTP server binds |
| `LORE_DATA_DIR` | `./data` | Directory for repos and downloaded models |
| `LORE_ADMIN_TOKEN` | unset | Admin bearer token. On the server, an empty value counts as unset and the admin API answers 403. The CLI reads it as the default of `--admin-token` |
| `LORE_LOG` | `info` | Log filter, in `tracing` `EnvFilter` syntax |
| `LORE_LOG_FORMAT` | `text` | `text` or `json`. Anything else is a startup error |
| `LORE_EMBEDDER` | `fastembed` | `fastembed` (bge-small-en-v1.5 on CPU) or `hash` (deterministic, for tests and `make run`). Anything else is a startup error. The first start with `fastembed` downloads about 130 MB into `$LORE_DATA_DIR/models` before the server listens |
| `LORE_EMBED_MODEL` | `bge-small-en-v1.5` | Embedding model. Only the default is supported in v1; any other value is a startup error |
| `LORE_MAX_BUNDLE_BYTES` | `536870912` (512 MiB) | Largest accepted body of `PUT /v1/repos/{id}/bundle`; a bigger upload is a 413. Must be a positive integer, otherwise a startup error |
| `LORE_VERSION` | the crate version | Version string reported by the server |
| `LORE_URL` | `http://localhost:8080` | CLI only: base URL of the server, the default of the global `--url` flag |
| `LORE_KEY` | unset | CLI only: tenant API key, the default of the global `--key` flag |

## Tenants and keys

lore is multi-tenant. Each tenant has API keys with the scopes `ingest` and `read`. Keys look like `lk_` followed by 40 hex characters, and only their SHA-256 hash is stored, so a key is shown once, when it is created. An admin token (`LORE_ADMIN_TOKEN` on the server) creates tenants and keys. Without it the admin API answers 403.

The CLI talks to the server over HTTP. `--url`, `--key` and `--admin-token` are global flags (env `LORE_URL`, `LORE_KEY`, `LORE_ADMIN_TOKEN`), and every subcommand prints the server's JSON.

```bash
export LORE_ADMIN_TOKEN=dev-admin               # with `make run` going in another shell
lore admin tenant create --name demo            # {"id": "...", "name": "demo"}
lore admin tenant list
lore admin key create --tenant <id>             # {"id": "...", "key": "lk_...", ...}; add --scopes read to limit it
lore admin key list --tenant <id>               # prefixes only, never the key or its hash
lore admin key revoke <key-id>                  # the key stops working at once
lore --key lk_... me                            # {"tenant_id": "...", "tenant_name": "demo", "scopes": [...]}
```

Tenant names are 1 to 64 characters of `a-z`, `0-9` and `-`. Send keys as `Authorization: Bearer lk_...`.

## Repos

A repo source is pushed as a git bundle. The server keeps a bare mirror and a checkout under `$LORE_DATA_DIR/repos/<source id>/`, and needs `git` on its path (the Docker image installs it). Both routes need an `ingest` key; listing needs `read`.

```bash
curl -X POST localhost:8080/v1/repos -H "Authorization: Bearer $LORE_KEY" \
  -H 'content-type: application/json' -d '{"name": "sample"}'      # {"id": "...", "name": "sample", ...}
curl localhost:8080/v1/repos -H "Authorization: Bearer $LORE_KEY"
git bundle create /tmp/repo.bundle HEAD
curl -X PUT "localhost:8080/v1/repos/<id>/bundle?ref=$(git rev-parse HEAD)&full=true" \
  -H "Authorization: Bearer $LORE_KEY" --data-binary @/tmp/repo.bundle
```

`ref` is the full 40 character lowercase sha of the bundle's `HEAD`. With `full=true` the bundle holds the whole history and the tree is re-indexed, deleting documents of files that are gone. With `full=false` the bundle may be incremental (`git bundle create out.bundle <last-sha>..HEAD`) and only changed files are indexed. If the server no longer has the last indexed commit, or the bundle needs commits it lacks, the answer is 409 (`need_full_bundle`) and the client sends a full bundle. The answer is a report with `indexed_commit`, `files_indexed`, `files_unchanged`, `files_skipped`, `files_deleted` and `chunks`.

Symlinks, submodules, binary or non-UTF-8 files, files over 256 KiB, and lock files, build output and vendored directories are skipped and counted in `files_skipped`.
