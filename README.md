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

## Deploy to kind

The manifests in `deploy/k8s/base/` run lore and a pgvector Postgres on the kind cluster `factory`. You need `kind`, `kubectl`, Docker and `openssl`. Every `kubectl` call in the Makefile passes `--context kind-factory`, so the current context never matters.

```bash
make kind-up                          # creates the cluster `factory` if it does not exist
make image VERSION=dev                # docker build, tagged lore:dev
make kind-load VERSION=dev            # loads the image into the cluster
make kind-deploy VERSION=dev          # namespace and secrets if missing, then apply and wait for the rollout
```

`NS` picks the namespace (default `lore`; use `lore-dev-<name>` for experiments, for example `make kind-deploy NS=lore-dev-t7 VERSION=dev-t7`). `kind-deploy` generates an overlay in `deploy/k8s/overlays/generated/` (gitignored) that sets the image tag, and remembers the previously deployed tag in `.previous-<NS>` there.

| Target | What it does |
|---|---|
| `make kind-secret` | Creates the namespace and the secrets `lore-db` and `lore-env` (random password and admin token) if they are missing. Existing secrets are never changed |
| `make kind-rollback` | Redeploys the previous tag, or runs `kubectl rollout undo` when none is recorded |
| `make kind-logs` | Last 200 log lines of the lore pod |
| `make kind-delete NS=lore-dev-<name>` | Deletes a dev namespace. It refuses `lore` and any name that is not `lore-dev-*` |

The first start downloads the embedding model into the `lore-data` volume, and the startup probe allows 3 minutes for it.

Verify the deployment (replace `lore` with your namespace, and `18080` with `18081` for a dev namespace):

```bash
kubectl --context kind-factory -n lore port-forward svc/lore 18080:80 >/tmp/lore-pf.log 2>&1 &
sleep 3
export LORE_URL=http://localhost:18080
export LORE_ADMIN_TOKEN=$(kubectl --context kind-factory -n lore get secret lore-env -o jsonpath='{.data.LORE_ADMIN_TOKEN}' | base64 -d)
cargo build --release && export PATH="$PWD/target/release:$PATH"
curl -s $LORE_URL/healthz            # {"status":"ok","db":"ok"}
scripts/smoke.sh m1                  # last line: SMOKE M1 OK
```

`scripts/smoke.sh <stage>` (`m1`, or `all` for every stage that exists) needs `LORE_URL`, `LORE_ADMIN_TOKEN`, and `lore`, `jq` and `git` on the path. It creates fresh tenants on each run. It also works against `make run`.

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

The CLI talks to the server over HTTP. `--url`, `--key` and `--admin-token` are global flags (env `LORE_URL`, `LORE_KEY`, `LORE_ADMIN_TOKEN`), and the `admin` and `me` subcommands print the server's JSON.

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

## Index a repo

`lore repo push` indexes the committed HEAD of a local git repo. It needs `git` on the path and an `ingest` key.

```bash
export LORE_URL=http://localhost:8080 LORE_KEY=lk_...
lore repo push . --name sample          # indexed sample at 1a2b3c4: 6 indexed, 0 unchanged, 0 deleted, 1 skipped, 31 chunks
lore repo push ~/code/app --json        # the report as one line of JSON
lore repo list                          # name, short indexed commit and web URL template, one repo per line
```

`PATH` defaults to `.` and can be any directory inside the repo. `--name` defaults to the name of the repo's top level directory.
The first push sends the whole history. Later pushes send only the commits the server lacks, and when the server already has HEAD nothing is sent and the report is all zeros.
If the server lost its copy, or the history was rewritten (for example after a force push), the CLI falls back to a full bundle by itself. `--full` forces one.
Uncommitted changes are not indexed, and the CLI says so on stderr.
`--web-url <template>` stores a URL template for links to files, for example `https://github.com/acme/app/blob/{commit}/{path}`. `repo list --json` prints the server's JSON on one line.

To index on every push to main, run it from CI. A GitHub Actions step (the checkout needs the full history on every push, so keep `fetch-depth: 0`: a shallow clone that lacks the commits the server has makes the push fail):

```yaml
on:
  push:
    branches: [main]
jobs:
  lore:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
        with:
          fetch-depth: 0
      - name: Index the repo in lore
        run: lore --url $LORE_URL --key ${{ secrets.LORE_KEY }} repo push . --name <repo>
        env:
          LORE_URL: ${{ vars.LORE_URL }}
```

The `lore` binary has to be on the runner (build it with `cargo install --path .` or copy a release binary).

## Search

`GET /v1/search` searches the chunks of the key's tenant. It needs a `read` key, and another tenant's documents are never visible: a source name from another tenant just matches nothing.

```bash
curl -G localhost:8080/v1/search -H "Authorization: Bearer $LORE_KEY" \
  --data-urlencode 'q=plan limits' --data-urlencode 'k=5' --data-urlencode 'kind=code,doc'
lore search "plan limits"                       # the same, from the CLI
lore search "pricing plans" --kind doc --k 3
lore search "plan limits" --source sample --json
```

| Parameter | Meaning |
|---|---|
| `q` | The query. Required: a missing or blank `q` is a 400. At most 2000 characters, otherwise a 400. It cannot contain a NUL byte |
| `k` | How many hits, default 10. Any integer is accepted and clamped to 1 to 50. Something that is not an integer is a 400 |
| `kind` | Comma-separated document kinds: `code`, `doc`, `session`, `note`. Another name is a 400 that lists the kinds. Empty means all |
| `source` | Only the source with this name, whatever its kind (`repo`, `traces`, `documents`). Empty means all. A name that matches nothing gives no hits |

The answer is `{"hits": [...]}`, best first. A hit has `chunk_id`, `document_id`, `source`, `kind`, `title`, `uri`, `path`, `start_line`, `end_line` (1-based, inclusive), `symbols`, `snippet` (the first 40 lines of the chunk) and `score`. `path` is set for repo files only. `uri` is the source's `web_url_template` filled in for repo files (see below), or else the document's own URI, or null.

The `web_url_template` of a repo source can use `{path}`, `{commit}`, `{start}` and `{end}`, for example `https://github.com/acme/app/blob/{commit}/{path}#L{start}-L{end}`. `{path}` is percent-encoded segment by segment (the `/` between segments stays), so a space, `#`, `?` or `%` in a file name cannot change the link. Other `{...}` text is left as it is.

How it ranks: the query is run twice, once against the Postgres full-text index (any query word may match, ranked with `ts_rank_cd`) and once against the pgvector embeddings (cosine distance). Each run keeps its best 50 chunks. The two lists are merged with Reciprocal Rank Fusion: a chunk scores the sum of `1 / (60 + rank)` over the lists it is in, so the score is at most about 0.033 and is only good for comparing hits of one search. Ties go to the better full-text rank, then to the lower chunk id. A query with only stop words (`the and of`) has no full-text part and is answered from the vectors alone. Identifiers such as `MaxSeatsPerPlan` are also indexed as the words `max seats per plan`, so a search for "seats" finds them.

The CLI prints, for each hit, `path:start-end  (score 0.032)  symbols` (the title when there is no path), then the first 3 snippet lines indented by 4 spaces. `--json` prints the server's JSON on one line. No hits prints `no results` on stderr and exits 0.
