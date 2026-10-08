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
| `LORE_ADMIN_TOKEN` | unset | Admin bearer token. An empty value counts as unset |
| `LORE_LOG` | `info` | Log filter, in `tracing` `EnvFilter` syntax |
| `LORE_LOG_FORMAT` | `text` | `text` or `json`. Anything else is a startup error |
| `LORE_VERSION` | the crate version | Version string reported by the server |
