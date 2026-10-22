# lore

Ask questions of your agents' traces, your coding sessions and your codebases, and get answers with citations.

- **fast** mode: one retrieval and one model call, an answer in seconds (for standups and meetings).
- **deep** mode: an agent loop with `search`, `grep`, `read_file` and `get_session` tools, under a time budget.

Inputs: OpenTelemetry GenAI traces over OTLP/HTTP, git repositories, and a plain documents API.
Surfaces: a `lore` CLI, an MCP server, and a small built-in web page.

Written in Rust, MIT licensed. Status: version 0.1. Indexing, search, both answer modes, the CLI, MCP and the web page work; the HTTP API may still change.

Indexing and search need no model and, once the embedding model is downloaded, no network. Answers call a model, by default the Anthropic API, so they need the network: see [Runs offline?](#runs-offline).

## At a glance: one question, three ways

The question: "what's included in our entry-level billing plan?", asked of a private 1,816-file service. The plan is defined in a database seed migration, which lists 13 facts: price, included usage, overage rate, six limits, retention, audit logs, regions and one policy. Test files elsewhere in the repo repeat some of those values.

| | lore fast | lore deep | Claude Code in a checkout |
|---|---|---|---|
| Time to answer | **4.9 s** | **13.3 s** | 28.0 s |
| How it got there | one search, one model call | 4 tool calls | 5 turns of grep and read |
| Found the seed migration | No. It answered from test files and said so | Yes | Yes |
| Facts right, of 13 | 9 | **13** | **13** |
| Facts wrong | 0 | 0 | 0, but it cited the wrong line (the next plan's row) |

- **fast** gives a quick, cited answer that is only as good as its top 12 search hits. Here it said its values came from test files rather than the plan's definition, which was true.
- **deep** searches until it finds the definition. It gave the same answer as Claude Code in under half the time.
- All three used `claude-opus-5-5` on the same commit, run once each on 2026-10-22. Claude Code cost $0.27 for this question; lore's cost is not reported (see [lore vs Claude Code](#lore-vs-claude-code), which also has three more questions on a second repo).

## How it works

lore works out where things are once, when they are indexed, instead of on every question. A question then costs one database query and, for an answer in prose, one model call.

```mermaid
flowchart TD
    repo["git repo<br/>lore repo push sends a git bundle"]
    otlp["agent traces<br/>OTLP/HTTP to /v1/traces"]
    docs["notes and documents<br/>POST /v1/documents"]

    repo --> mirror[("bare mirror and checkout<br/>per repo, on the server's disk")]
    otlp --> sessions["spans grouped by session,<br/>one markdown transcript<br/>per quiet session"]
    mirror --> chunk
    sessions --> chunk
    docs --> chunk

    chunk["chunking<br/>code: one chunk per function, class, impl... (tree-sitter)<br/>markdown: one chunk per heading section<br/>other text: windows of 60 lines<br/>identifiers split: MaxSeatsPerPlan = max seats per plan"]
    chunk --> embed["embedding<br/>bge-small-en-v1.5 via fastembed<br/>on the CPU, inside the lore process"]
    embed --> pg[("Postgres<br/>tsvector full-text index<br/>pgvector HNSW index")]

    pg --> hybrid["hybrid retrieval<br/>full-text top 50 + vector top 50<br/>fused with Reciprocal Rank Fusion"]
    hybrid --> search["search<br/>no model call"]
    hybrid --> fast["fast answer<br/>best 12 passages, one model call"]
    hybrid --> deep["deep answer<br/>the model calls tools in a loop:<br/>search, grep, read_file, get_session"]
    mirror -.->|"grep, read_file"| deep

    search --> surfaces["REST API, lore CLI,<br/>MCP at /mcp, web page at /"]
    fast --> surfaces
    deep --> surfaces
```

1. **Ingest.** `lore repo push` sends a git bundle of the repo's committed HEAD. After the first push it sends only the commits the server lacks, and only changed files are indexed again. The server keeps a bare mirror and a checkout of each repo. Trace spans arrive over OTLP/HTTP; once a session has had no new span for 60 seconds (`LORE_SESSION_QUIET_SECS`), its spans become one markdown transcript. Notes and documents arrive as text through `POST /v1/documents`.
2. **Chunking.** Rust, Go, TypeScript, TSX, JavaScript and Python files are parsed with tree-sitter into one chunk per top-level item (function, class, struct, impl, trait, interface and so on), together with the comments and attributes directly above it. An item over 150 lines is split: a class, impl, trait or interface into a header and one chunk per member, anything else into windows of 120 lines. Code outside any item goes into chunks of at most 60 lines. Markdown files, session transcripts and documents are split at `#`, `##` and `###` headings. Any other text file becomes windows of 60 lines that overlap by 10. A chunk keeps its line range and its symbol names, such as `Plan` and `Plan::limits`.
3. **Embedding.** Each chunk, with its path (or title) and symbol names in front, is embedded by bge-small-en-v1.5 (384 dimensions) through fastembed, on the CPU, inside the lore process. No embedding API is called. A chunk whose text did not change keeps its stored vector.
4. **Storage.** Each chunk is one Postgres row with a `tsvector` for full-text search and a pgvector embedding (HNSW index, cosine distance). The `tsvector` also holds every identifier split into words, so `MaxSeatsPerPlan` matches a search for "seats". Every row carries its tenant id.
5. **Retrieval.** A query runs against both indexes. Each keeps its best 50 chunks, and Reciprocal Rank Fusion merges the two lists (see [Search](#search)).
6. **Answers.** Fast mode puts the best 12 passages (at most 60,000 characters) in a prompt as numbered sources and makes one model call. Deep mode gives the model four tools, `search`, `grep`, `read_file` and `get_session`, and runs the calls it asks for on the server, for up to 12 rounds within 30 seconds by default. In both modes lore maps each citation back to its document and line range, and to a link when the source has one.

### What lore does and what the model does

| | lore | The model |
|---|---|---|
| Indexing | Receives bundles, traces and documents; chunks, embeds and stores them | Nothing |
| A search | Runs the full-text and vector searches and fuses the results | Nothing |
| A fast answer | Retrieves the passages, builds the prompt, maps `[n]` citations to files, lines and links | Reads the passages and writes the answer, in one call |
| A deep answer | Runs each tool call against the tenant's index and repo checkouts, enforces the time budget and round limit, maps citations to files, lines and links | Chooses the next tool call and its input, reads the results, writes the answer |
| Access | Tenants, API keys and scopes, per-tenant deep-mode limits; the REST API, CLI, MCP server and web page | Nothing |

### Why it is faster than an agent grepping a checkout

An agent pointed at a checkout finds the relevant code at question time: it greps, reads a file, greps again. Each step is another model round trip, in sequence, and each one sends the whole conversation so far. lore does that discovery once, at index time. A fast question is then tens of milliseconds of retrieval (18 to 36 ms in the measurement below) and one model call. Deep mode still loops, but its `search` starts from ranked chunks rather than raw files, and the tools run on the server next to the index, with the calls of one round run at the same time. [lore vs Claude Code](#lore-vs-claude-code) measures the difference on one repo.

## Runs offline?

Partly. Indexing and search never call a model. Answers do, and by default that model is the Anthropic API.

| Capability | Needs a model? | Needs the network? |
|---|---|---|
| Indexing: `lore repo push`, `POST /v1/traces`, `POST /v1/documents` | No | No, only the client's connection to your lore server |
| Search: `lore search`, `GET /v1/search`, the MCP tools `search`, `get_document` and `list_sources` | No | No |
| `fast` answers (`lore ask`, `POST /v1/ask`, the MCP `ask` tool, the web page) with the default `LORE_LLM=anthropic` | Yes, one call | Yes, to the Anthropic API |
| `fast` answers with `LORE_LLM=openai` and a local OpenAI-compatible server, such as llama-server | Yes, one call | No |
| `deep` answers | Yes, a loop of calls with tools | Yes. Deep mode needs a provider that can use tools, which is `anthropic` only: with `openai` a deep question is a 400 |

- The embedder runs on the CPU inside lore. The first start with `LORE_EMBEDDER=fastembed` (the default) downloads the model, about 130 MB, from Hugging Face into `$LORE_DATA_DIR/models` (`$HF_HOME` instead, when that is set), before the server listens. Later starts load it from there without the network. To set up a machine with no network, copy that directory from one that has it.
- With the default provider, `lore serve` does not start without `ANTHROPIC_API_KEY`, even if you only search. It does not call the API at startup, so search keeps working while the network is down. `LORE_LLM=fake` starts without a key, for a server that only searches; its answers are placeholders.
- For answers with no network, run lore with `LORE_LLM=openai`, `LORE_OPENAI_BASE_URL` set to the local server (for example `http://localhost:8081/v1`) and `LORE_OPENAI_MODEL`. That gives fast mode only.

## lore vs Claude Code

What does lore add over pointing Claude Code at a checkout of the repo and asking it? One measurement, taken on 2026-10-22.

The repo is a private 54-file Python service (an MCP server). lore indexed it, 54 files into 430 chunks, in 23 seconds.

A larger private repo, a service of 1,816 files (mostly Go and SQL), took 35 minutes to index on the same laptop (Apple M2 Pro, 10 cores, 16 GB): 1,801 files into 22,796 chunks. lore used up to 7 cores and 3.7 GB of memory while it embedded, and nearly all of the time is the embedding model on the CPU. `lore repo push` stops waiting after 15 minutes and reports a timeout, but the server finishes the push anyway; `lore repo list` shows the new commit once it is done. Later pushes embed only the files that changed.

lore and Postgres ran on an Apple Silicon laptop. Both sides used `claude-opus-5-5`. lore ran with its defaults: effort `low` in fast mode, `medium` in deep mode, 12 passages for a fast answer. Claude Code 2.1.304 ran as `claude -p "<question> Answer from this repository's code and cite file:line."` in a checkout of the repo, with only `Read`, `Grep` and `Glob` allowed (`--allowedTools`) and no MCP servers (`--strict-mcp-config`), so it could not use lore. "Retrieval only" is the median of 5 `GET /v1/search` calls (`k=12`) timed by curl: embedding the query, both index scans and the fusion, with no model call. The other times are wall-clock as each tool reports it, `latency_ms` from `/v1/ask` and `duration_ms` from Claude Code. The fast answers, the deep answers and the Claude Code runs started at the same time; the three deep questions ran one after another.

| Question | Retrieval only | lore fast | lore deep | Claude Code |
|---|---|---|---|---|
| Architecture overview ("how does auth work?") | 18 ms | 8.1 s | 16.1 s (4 tool calls) | 52.5 s (6 turns, $0.38) |
| List a feature's tools and what each does | 19 ms | 5.9 s | 15.8 s (3 tool calls) | 38.7 s (6 turns, $0.32) |
| Trace one code path end to end, with the test that covers it | 36 ms | 8.2 s | 19.5 s (5 tool calls) | 36.9 s (9 turns, $0.33) |

Claude Code sent 164k to 211k input tokens per question, most of them read from the prompt cache. Its cost is the `total_cost_usd` it reports, at list price.

lore's cost is not in the table. `/v1/ask` reports the `input_tokens` the API returns, which count only input that was not cached. lore turns prompt caching on for every request, so most of the input lands in cache tokens that lore does not report (the fast answers here show 4 input tokens). A fast answer is one call with at most 60,000 characters of passages.

Accuracy was checked on two of the questions. For the tool list, all three answers (lore fast, lore deep and Claude Code) named all 5 tools. For the code path, all three had all 5 facts checked: the HTTP method and path, the error message, the helper that marks the call as failed, the exact test name and the lifecycle constant.

What this does not show:

- Each cell is one run. Model latency varies from run to run.
- It is one small repo. On a large one Claude Code has more to search, and fast mode's passages cover less of the code. Fast mode sees only its top `k` passages (12 by default) and can miss what does not rank there; deep mode, which can grep and read whole files, is the fallback.
- lore answers from the last pushed commit and does not see uncommitted edits. Claude Code reads the working tree.
- Claude Code can go on to edit the code and run commands. lore only answers.
- Indexing is paid up front: 23 seconds for this repo and 35 minutes for the 1,816-file one, then only the changed files on each later push.

To run the comparison on your own repo, put one question per line in a file and run `scripts/bench_vs_claude_code.sh`:

```bash
export LORE_URL=http://localhost:8080 LORE_KEY=lk_...
scripts/bench_vs_claude_code.sh ~/code/app questions.txt bench-out
```

It pushes the repo with `lore repo push` (`BENCH_PUSH=0` skips that), times 5 searches per question, then for each question starts a fast answer, a deep answer and a Claude Code run together. It saves every response in the output directory (default `bench-<unix time>`) and prints a table like the one above, with Claude Code's input tokens added. It needs `curl`, `jq`, `claude` and `lore` on the path, and a key with the `read` scope (and `ingest` for the push) of a tenant that holds only this repo. `BENCH_MODEL` sets Claude Code's model (default `claude-opus-5-5`); run the lore server with the same `LORE_FAST_MODEL` and `LORE_DEEP_MODEL`. Every question costs one fast answer, one deep answer and one Claude Code run.

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
scripts/smoke.sh m5                  # last line: SMOKE M5 OK (deep mode, with LORE_LLM=fake)
scripts/mcp_smoke.sh                 # last line: MCP SMOKE OK
```

`scripts/smoke.sh <stage>` (`m1`, `m2`, `m4`, `m5`, or `all` for every stage that exists) needs `LORE_URL`, `LORE_ADMIN_TOKEN`, and `lore`, `jq`, `git` and `curl` on the path. It creates fresh tenants on each run. It also works against `make run`. `scripts/mcp_smoke.sh` checks the MCP endpoint (see [Use lore from agents](#use-lore-from-agents)); it needs `curl` as well.

## Self-hosting

To run lore on your own VPS with Caddy for HTTPS and a daily backup, see [docs/self-hosting.md](docs/self-hosting.md). `make prod-local` rehearses the same stack on your machine.

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
| `LORE_DEEP_BUDGET_SECS` | `30` | Time budget of one deep-mode question, in seconds. Once more than 80% of it has passed, the model is told to answer with what it has. Must be a positive integer |
| `LORE_DEEP_MAX_ROUNDS` | `12` | Most tool rounds of one deep-mode question. After that the model is told to answer with what it has. Must be a positive integer |
| `LORE_DEEP_CONCURRENCY` | `2` | How many deep-mode questions one tenant may run at once. A further question waits up to 5 seconds for a free slot, then gets a 429. Must be a positive integer |
| `LORE_DEEP_DAILY_LIMIT` | `100` | How many deep-mode questions one tenant may ask per UTC day. The next one gets a 429 ("daily deep-mode limit reached"). The count is kept in memory, so a restart resets it. Must be a positive integer |
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

`source` names a group of documents (1 to 100 characters of `A-Z a-z 0-9 . _ -`, but not `.` or `..`; it is created on first use). `documents` has 1 to 100 entries. Each has an `external_id` (1 to 512 bytes, unique within the source, with no `.` or `..` between slashes, so `a.b` and `a/.b` are fine but `..` and `a/../b` are not), a `title` of at most 1 KiB, a `text` of at most 1 MiB, and optionally `kind` (`note`, the default, or `doc`), `uri` (shown with search hits, at most 2,048 bytes) and `metadata` (a JSON object of at most 64 KiB once serialized). A text that makes more than 1000 chunks is refused too. Anything invalid is a 400 and nothing of that request is written. The body may be up to 128 MiB; more is a 413.

Putting an `external_id` again replaces the document. When its kind, title, text, uri and metadata are all the same as stored, it is skipped and counted in `unchanged`, so repeating a sync is cheap. If one id appears twice in a request, the documents are handled in order and the last one wins. The text is split into sections at markdown headings, and long sections into overlapping windows of lines. All of the text is searchable: a window is kept within 8,000 characters, and a single line longer than that is split into several chunks (at a space when there is one close to the end) instead of being cut.

```bash
lore doc put --source notes --id n1 --title "Review" --text "The quarterly zorblax review moved to Thursdays."
lore doc put --source wiki --id guide --title "Guide" --file guide.md --kind doc --uri https://example.com/guide
lore doc get <DOCUMENT_ID>                      # prints the text
lore doc rm --source notes --id n1
```

`GET /v1/documents/{id}` (a `read` key) returns `{"id", "kind", "title", "uri", "metadata", "text"}` for any document of the tenant, using the `document_id` of a search hit. For a document put through the API the text is exactly what was sent (line endings, blank lines and the final newline included), cut at 100,000 characters; for repo files and sessions it is rebuilt from the indexed chunks. `DELETE /v1/documents/{source}/{external_id}` (an `ingest` key) answers 204, or 404 when the tenant has no such document; an `external_id` may contain `/`, and other special characters must be percent-encoded. `lore doc put` accepts either `--file` or `--text`, and prints the server's JSON.

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
| `mode` | `fast` (the default) or `deep`. `deep` runs an agent loop (see below) and is a 400 ("deep mode needs a tool-capable model provider (LORE_LLM=anthropic)") with `LORE_LLM=openai` |
| `k` | Fast mode only: how many sources to retrieve, default 12, clamped to 1 to 50 |
| `kind` | Fast mode only: a list of document kinds: `code`, `doc`, `session`, `note`. Another name is a 400 |
| `source` | Fast mode only: only the source with this name |

The answer is `{"answer", "mode", "model", "citations", "sources_considered", "tool_calls", "usage", "latency_ms"}`. `citations` lists the sources the answer cites with `[n]`, once each, in the order they first appear. A citation has `label` (the source number, as text), `document_id`, `kind`, `title`, `path`, `start_line`, `end_line` and `url` (the same link `/v1/search` gives as `uri`). `sources_considered` is the number of passages that went into the prompt (at most 60,000 characters of them, each cut to its first 120 lines), `tool_calls` is 0 in fast mode, and `usage` has `input_tokens` and `output_tokens`.

When nothing indexed matches, the answer is "I found nothing indexed that matches this question.", `model` is `"none"` and the model is not called. If the model declines to answer, the response is a 422 `{"error": "the model declined to answer"}`.

Deep mode (`"mode": "deep"`) lets the model drive the `search`, `grep`, `read_file` and `get_session` tools for up to `LORE_DEEP_MAX_ROUNDS` rounds within `LORE_DEEP_BUDGET_SECS` seconds. Tool calls of one round run at the same time, each for at most 10 seconds; a slower one gives the model an error result. Past 80% of the budget, or at the round cap, the model gets one last call with the tools turned off and is told to answer. Cost is capped per tenant by `LORE_DEEP_CONCURRENCY` and `LORE_DEEP_DAILY_LIMIT` (both give a 429). A deep answer cites code as `[path:start-end]`, with the path as the tools print it minus the source name, and sessions as `[session:<id>]`. Each distinct citation appears once in `citations` (at most 50), in order of first appearance. The `label` is the text between the brackets. A citation whose path or session lives in the tenant's index carries `document_id`, `kind`, `title` and `url`; one that does not match keeps its `label`, `path` and lines, with `document_id` null. In deep mode `tool_calls` counts the tool calls, `sources_considered` is 0, `usage` is summed over all model calls and `model` is the model of the last call. If the budget runs out with no answer at all, the response is a 500.

The CLI prints the answer, a blank line, a `Sources:` list with `[label] path:start-end` (the title when there is no path) and the link on the next line, then a dim line `<model> · <latency> ms · <input> in / <output> out`. With `LORE_LLM=fake` the answer is `Fake answer from N sources.` followed by `[1] [2]` and so on: it exercises the plumbing without a model.

`GET /v1/info` needs no key. It reports what the server runs, for clients and health dashboards:

```bash
curl localhost:8080/v1/info    # {"version":"0.1.0","llm":"fake","embedder":"hash","modes":["fast","deep"]}
```

`modes` lists `deep` only when the model provider can use tools (`anthropic` and `fake`, not `openai`).

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

Any other MCP client that speaks streamable HTTP works the same way: register `$LORE_URL/mcp` with the same `Authorization: Bearer <key>` header.

| Tool | Input | Returns |
| --- | --- | --- |
| `search` | `query`, `k` (1 to 50, default 8), `kind` (list of `code`, `doc`, `session`, `note`) | One text item: a JSON array of hits with `document_id`, `path`, `start_line`, `end_line`, `kind`, `title`, `source`, `score` and `snippet` (the first 40 lines) |
| `ask` | `question`, `mode` (`fast`, the default; `deep` runs the agent loop) | Two text items: the answer followed by a `Sources:` list with one line per citation, then the full `/v1/ask` response as JSON |
| `get_document` | `document_id` (the id of a search hit or a citation) | The document's text under a `# <title>` header with its kind and path, cut to 100,000 characters. An unknown id, or another tenant's, is the tool error `document not found` |
| `list_sources` | none | One text item: a JSON array of `{name, kind, indexed_commit, documents}` |

`scripts/mcp_smoke.sh` runs the whole exchange against a server: `LORE_URL=http://localhost:8080 LORE_ADMIN_TOKEN=dev-admin scripts/mcp_smoke.sh` prints the sorted tool names, the `src/billing/plans.rs` hit and `MCP SMOKE OK`. By default `/mcp` accepts any `Host` header; set `LORE_MCP_ALLOWED_HOSTS` to restrict it.
