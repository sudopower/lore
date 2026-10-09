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

`NS` picks the namespace (default `lore`; use `lore-dev-<name>` for experiments, for example `make kind-deploy NS=lore-dev-t7 VERSION=dev-t7`). `kind-deploy` generates an overlay in `deploy/k8s/overlays/generated/` (gitignored) that sets the image tag, and keeps two records there per namespace, written only after a rollout succeeds: `.current-<NS>` (the last tag that rolled out) and `.previous-<NS>` (the one it replaced). The records live in your checkout, so they are per worktree, not per cluster. Use a new `VERSION` for every build: kind loads images by tag and the Deployment does not change when the tag does not, so a rebuilt image under the same tag is not picked up. `NS` must be one word of lowercase letters, digits and `-`.

| Target | What it does |
|---|---|
| `make kind-secret` | Creates the namespace and the secrets `lore-db` and `lore-env` (random password and admin token) if they are missing. Existing secrets are never changed |
| `make kind-rollback` | Goes back to the last tag that rolled out when the live tag is a newer one that failed, otherwise to the previous tag; runs `kubectl rollout undo` when nothing is recorded |
| `make kind-logs` | Last 200 log lines of the lore pod |
| `make kind-delete NS=lore-dev-<name>` | Deletes a dev namespace. It refuses `lore`, any name that is not `lore-dev-*`, and any value with a space or other character outside `[a-z0-9-]` |

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
scripts/smoke.sh m2                  # last line: SMOKE M2 OK
scripts/mcp_smoke.sh                 # last line: MCP SMOKE OK
```

`scripts/smoke.sh <stage>` (`m1`, `m2`, or `all` for every stage that exists) needs `LORE_URL`, `LORE_ADMIN_TOKEN`, and `lore`, `jq` and `git` on the path. It creates fresh tenants on each run. It also works against `make run`. `scripts/mcp_smoke.sh` checks the MCP endpoint (see [Use lore from agents](#use-lore-from-agents)); it needs `curl` as well.

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
| `LORE_LLM` | `anthropic` | Model provider: `anthropic`, `openai` (an OpenAI-compatible server such as llama-server, text only) or `fake` (deterministic, for tests and `make run`). Anything else is a startup error |
| `ANTHROPIC_API_KEY` | unset | Anthropic API key. Required when `LORE_LLM=anthropic`: `lore serve` exits at startup with an error naming the variable when it is missing |
| `ANTHROPIC_BASE_URL` | `https://api.anthropic.com` | Base URL of the Anthropic API |
| `LORE_FAST_MODEL` | `claude-opus-5-5` | Model for fast answers |
| `LORE_FAST_EFFORT` | `low` | Effort sent with fast answers. An empty value (`LORE_FAST_EFFORT=`) sends no effort. Claude Haiku 4.5 rejects `effort`, so the escape hatch `LORE_FAST_MODEL=claude-haiku-4-5` needs `LORE_FAST_EFFORT=` (empty) as well |
| `LORE_DEEP_MODEL` | `claude-opus-5-5` | Model for deep answers |
| `LORE_DEEP_EFFORT` | `medium` | Effort sent with deep answers. An empty value (`LORE_DEEP_EFFORT=`) sends no effort, as required for Claude Haiku 4.5 |
| `LORE_OPENAI_BASE_URL` | unset | Base URL of the OpenAI-compatible server, for example `http://localhost:8081/v1` (lore calls `<base>/chat/completions`). Required when `LORE_LLM=openai` |
| `LORE_OPENAI_MODEL` | unset | Model name sent to the OpenAI-compatible server, used for both answer modes with no effort. Required when `LORE_LLM=openai` |
| `LORE_OPENAI_API_KEY` | unset | Bearer token for the OpenAI-compatible server. Optional |
| `LORE_MCP_ALLOWED_HOSTS` | unset | Comma-separated `Host` values the MCP endpoint (`/mcp`) accepts, for example `lore.example.com,lore.lore.svc:8080`. Unset means any host is accepted, which is what you want behind Caddy or a cluster Service; the bearer key is the protection. When set, a request with another `Host` header gets a 403 |
| `LORE_SESSION_QUIET_SECS` | `60` | Seconds a session must go without a new span before its spans are turned into one searchable document. A session that keeps receiving spans is indexed again after each quiet period. `0` indexes on the next worker run |
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

## Traces

`POST /v1/traces` takes OpenTelemetry traces over OTLP/HTTP, so an agent or a collector can export its sessions to lore. It needs an `ingest` key. Send `Content-Type: application/json` or `application/x-protobuf`, optionally with `Content-Encoding: gzip`. Anything else is a 415. A bad body is a 400. A body over 16 MiB, one that unzips to more than 64 MiB, or one whose spans would store more than 64 MiB of row data (the resource attributes count once per span) is a 413. A span whose trace id is not 16 bytes, whose span id is not 8 bytes, or whose parent span id is not empty or 8 bytes is not stored; the answer is still a 200 and carries `partialSuccess` with `rejectedSpans` and an `errorMessage`. A session id over 512 bytes is shortened to its first part plus a hash, so two long ids stay distinct. With nothing rejected the answer is an empty `ExportTraceServiceResponse` in the format of the request.

Spans are stored per tenant, keyed by trace id and span id, so sending a span again updates it. A span's session is its `session.id` attribute, else `gen_ai.conversation.id`, else the resource's `session.id`, else its trace id. Each session is queued for indexing. Strings longer than 64 KiB are cut and end with `…[truncated]`.

```bash
lore otlp send tests/fixtures/otlp/claude_code_session.json          # JSON, needs --url and --key
lore otlp send tests/fixtures/otlp/otel_genai_session.json --protobuf --gzip
```

`lore otlp send <FILE.json>` posts an OTLP JSON file to `/v1/traces` with the global `--url` and `--key`. `--protobuf` converts it to protobuf first, and `--gzip` compresses the body. It prints `HTTP <status>` and exits non-zero when the server does not answer 2xx.

## Documents

`POST /v1/documents` stores notes and documents that are not in a repo or a trace, and indexes them so search and ask find them. It needs an `ingest` key.

```bash
curl -X POST "$LORE_URL/v1/documents" \
  -H "Authorization: Bearer $LORE_KEY" -H "Content-Type: application/json" \
  -d '{"source": "notes", "documents": [
        {"external_id": "n1", "title": "Review", "text": "The quarterly zorblax review moved to Thursdays."}
      ]}'
# {"upserted":1,"unchanged":0,"chunks":1}
```

`source` names a group of documents (1 to 100 characters of `A-Z a-z 0-9 . _ -`; it is created on first use). `documents` has 1 to 100 entries. Each has an `external_id` (1 to 512 bytes, unique within the source), a `title`, a `text` of at most 1 MiB, and optionally `kind` (`note`, the default, or `doc`), `uri` (shown with search hits) and `metadata` (a JSON object). Anything invalid is a 400 and nothing of that request is written. The body may be up to 128 MiB; more is a 413.

Putting an `external_id` again replaces the document. When its kind, title, text, uri and metadata are all the same as stored, it is skipped and counted in `unchanged`, so repeating a sync is cheap. If one id appears twice in a request, the documents are handled in order and the last one wins. The text is split into sections at markdown headings.

```bash
lore doc put --source notes --id n1 --title "Review" --text "The quarterly zorblax review moved to Thursdays."
lore doc put --source wiki --id guide --title "Guide" --file guide.md --kind doc --uri https://example.com/guide
lore doc get <DOCUMENT_ID>                      # prints the text
lore doc rm --source notes --id n1
```

`GET /v1/documents/{id}` (a `read` key) returns `{"id", "kind", "title", "uri", "metadata", "text"}` for any document of the tenant, using the `document_id` of a search hit; the text is cut at 100,000 characters. `DELETE /v1/documents/{source}/{external_id}` (an `ingest` key) answers 204, or 404 when the tenant has no such document; an `external_id` may contain `/`, and other special characters must be percent-encoded. `lore doc put` accepts either `--file` or `--text`, and prints the server's JSON.

## Ask

`POST /v1/ask` answers a question from the key's tenant, with numbered citations. It needs a `read` key. Fast mode runs one hybrid search (the same as `/v1/search`), puts the best passages in a prompt as numbered sources, and makes one model call.

```bash
curl localhost:8080/v1/ask -H "Authorization: Bearer $LORE_KEY" -H 'content-type: application/json' \
  -d '{"question": "how many plans are there and what are their limits?"}'
lore ask "how many plans are there and what are their limits?"     # the same, from the CLI
lore ask "who owns billing?" --kind doc,note --k 8 --source sample
lore ask "how many plans are there?" --json                        # the server's JSON on one line
```

| Field | Meaning |
|---|---|
| `question` | Required. A missing or blank question is a 400. At most 2000 characters, otherwise a 400 |
| `mode` | `fast` (the default) or `deep`. `deep` is a 400 ("deep mode is not available yet") until it exists |
| `k` | How many sources to retrieve, default 12, clamped to 1 to 50 |
| `kind` | A list of document kinds: `code`, `doc`, `session`, `note`. Another name is a 400 |
| `source` | Only the source with this name |

The answer is `{"answer", "mode", "model", "citations", "sources_considered", "tool_calls", "usage", "latency_ms"}`. `citations` lists the sources the answer cites with `[n]`, once each, in the order they first appear. A citation has `label` (the source number, as text), `document_id`, `kind`, `title`, `path`, `start_line`, `end_line` and `url` (the same link `/v1/search` gives as `uri`). `sources_considered` is the number of passages that went into the prompt (at most 60,000 characters of them, each cut to its first 120 lines), `tool_calls` is 0 in fast mode, and `usage` has `input_tokens` and `output_tokens`.

When nothing indexed matches, the answer is "I found nothing indexed that matches this question.", `model` is `"none"` and the model is not called. If the model declines to answer, the response is a 422 `{"error": "the model declined to answer"}`.

The CLI prints the answer, a blank line, a `Sources:` list with `[label] path:start-end` (the title when there is no path) and the link on the next line, then a dim line `<model> · <latency> ms · <input> in / <output> out`. With `LORE_LLM=fake` the answer is `Fake answer from N sources.` followed by `[1] [2]` and so on: it exercises the plumbing without a model.

`GET /v1/info` needs no key. It reports what the server runs, for clients and health dashboards:

```bash
curl localhost:8080/v1/info    # {"version":"0.1.0","llm":"fake","embedder":"hash","modes":["fast"]}
```

## Web page

`GET /` serves a small built-in page for asking questions, for example in a standup. It is one self-contained HTML file embedded in the binary (no build step, no CDN) and needs no key to load. Open `LORE_URL/` (`http://localhost:8080/` with `make run`), paste a `read` key and type a question. Enter sends it, Shift+Enter adds a new line.

The key stays in the browser's local storage (under `lore.key`) and is sent only to this server, as `Authorization: Bearer <key>`. If the browser blocks local storage, the page still works and you paste the key again on each visit. The answer shows with numbered citations that link to the sources; a source is a link only when its URL starts with `http://` or `https://`. The `deep` option is enabled only when `GET /v1/info` lists `deep` in `modes`.

The page logic is checked by `tests/web.rs`, which also runs `tests/web_pure.js` under node when node is installed.

## Use lore from agents

lore serves MCP (the Model Context Protocol) over streamable HTTP at `/mcp`, so an agent can search and ask questions about what you indexed. The endpoint is stateless and answers with plain JSON. It takes the same `lk_` tenant keys as the REST API, sent as `Authorization: Bearer <key>`, and the key needs the `read` scope. A missing or bad key is a 401 and a key without `read` is a 403, both with the usual `{"error": "..."}` body. Every tool sees only the key's own tenant.

Claude Code:

```bash
claude mcp add --transport http lore "$LORE_URL/mcp" --header "Authorization: Bearer $LORE_KEY"
```

Tares agents: register lore in the Tares MCP registry as a streamable HTTP server, with the URL `$LORE_URL/mcp` and the same `Authorization: Bearer <key>` header. The tools then appear to the agent as `lore__search`, `lore__ask`, `lore__get_document` and `lore__list_sources`.

Any other MCP client that speaks streamable HTTP works the same way.

| Tool | Input | Returns |
| --- | --- | --- |
| `search` | `query`, `k` (1 to 50, default 8), `kind` (list of `code`, `doc`, `session`, `note`) | One text item: a JSON array of hits with `document_id`, `path`, `start_line`, `end_line`, `kind`, `title`, `source`, `score` and `snippet` (the first 40 lines) |
| `ask` | `question`, `mode` (`fast`, the default; `deep` is a tool error until deep mode exists) | Two text items: the answer followed by a `Sources:` list with one line per citation, then the full `/v1/ask` response as JSON |
| `get_document` | `document_id` (the id of a search hit or a citation) | The document's text under a `# <title>` header with its kind and path, cut to 100,000 characters. An unknown id, or another tenant's, is the tool error `document not found` |
| `list_sources` | none | One text item: a JSON array of `{name, kind, indexed_commit, documents}` |

`scripts/mcp_smoke.sh` runs the whole exchange against a server: `LORE_URL=http://localhost:8080 LORE_ADMIN_TOKEN=dev-admin scripts/mcp_smoke.sh` prints the sorted tool names, the `src/billing/plans.rs` hit and `MCP SMOKE OK`. By default `/mcp` accepts any `Host` header; set `LORE_MCP_ALLOWED_HOSTS` to restrict it.
