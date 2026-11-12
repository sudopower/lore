# Symbol retrieval validation

The fourth RRF list changes ranking, not the search response shape. It activates on explicit symbol names or call/definition/connection intent, rather than incidental English words. For example, both `what calls index_text?` and `how does checkout reach persist?` activate it. An ordinary question about a time budget does not.

The integrated comparison against main's #50 executable repeats the same public 20-question ranks and the same four supplemental ranks shown below, using real BGE embeddings and the gold commit. There are no public-set per-question regressions; the supplemental 27-to-34 regression remains visible rather than being excluded.

## Ranking

The original standalone search-only evaluation uses `fastembed:bge-small-en-v1.5`, the default embedding model, and no language model. Both servers index the same 117 files and 1,773 chunks from commit `105500e75cdf3d65886d5220ca070440735b50bb`. The baseline executable is built from that commit. Matched comparisons also use identical stored vectors and chunk ids in isolated databases. Build baseline and candidate in separate Cargo target directories, or verify copied executables, to avoid stale shared-target artifacts.

The public 20-question set from #32 has **no per-question rank changes**:

| Category | Baseline ranks | Symbol retrieval ranks |
| --- | --- | --- |
| enumeration (01–04) | 1, miss, 10, miss | 1, miss, 10, miss |
| schema (05–08) | 10, 2, 13, 35 | 10, 2, 13, 35 |
| where (09–12) | 2, 1, 1, 29 | 2, 1, 1, 29 |
| config (13–16) | 12, 14, 10, 2 | 12, 14, 10, 2 |
| how (17–20) | 1, 28, 6, 1 | 1, 28, 6, 1 |

Overall hit@5 is 8/20, hit@12 13/20, hit@50 18/20 and MRR 0.365 for both. This is no-regression evidence, not evidence that the general suite improves.

The supplemental [symbol questions](callgraph-queries.json) were specified from call sites and definitions in the original source before comparing their results. They cover two caller lookups and two definition/callee relationships. Gold ranges remain fixed at the original commit; `need: all` requires both ranges for the how questions.

| Question | Baseline rank | Symbol retrieval rank |
| --- | ---: | ---: |
| where `index_text` is called for sessions | 2 | 1 |
| where MCP uses `document_text` | 1 | 1 |
| how `session_id` chooses and bounds its value | 4 | 2 |
| how `index_text` embeds chunks asynchronously | 27 | 34 |

Where MRR improves from 0.750 to 1.000; how MRR from 0.144 to 0.265. Overall MRR improves from 0.447 to 0.632. Hit@5 and hit@12 stay 3/4; hit@50 stays 4/4. The last question **regresses from 27 to 34**, so this is a small mixed-result set, not a general quality guarantee. Common-name caps can omit useful callees.

With the #32 search-only harness, run each server against the same fixed corpus and gold sets:

```bash
bench/eval/eval LABEL URL KEY bench/eval/questions/lore.json
bench/eval/eval SYMBOL_LABEL URL KEY docs/callgraph-queries.json
bench/eval/eval --diff BASE_LABEL CANDIDATE_LABEL
```

No Claude comparison or paid grading is needed.

## Resource gates

`tests/callgraph.rs` contains a separately run latency gate:

```bash
DATABASE_URL=postgres://lore:lore@localhost:5434/lore_i37 \
  cargo test --test callgraph related_latency -- --ignored --nocapture
```

It starts a fresh process and database, stores 20,092 chunks, and measures 40 calls including the first call, misses, two-name queries, eight symbols with five definitions each, and a 25-definition expansion with 50 caller candidates and 64 selected callee names. It alternates unfiltered and source-filtered lookups. The heavy rows hold 256 metadata entries of up to 128 bytes. Definitions fill the output before expansion when the 30-result budget is already full. The full suite separately checks 100-definition and 200-caller truncation, and retrieval through a one-connection pool.

On the integrated implementation, the first call took **7.672 ms** and the worst of 40 took **20.475 ms**, below the issue's **50 ms** added-latency ceiling. These are added-branch wall times, not full search times. No latency limit was loosened.

The first integration run failed at **92.904 ms**. `EXPLAIN (ANALYZE, BUFFERS)` on its retained 20,092-chunk fixture showed source-filtered custom plans scanning every chunk in the source before intersecting matched ids: even a one-definition `persist` lookup took 71.985 ms. Hydrating each matched id through a correlated `LATERAL` query with `OFFSET 0` keeps both custom and generic plans on primary-key lookups; that scoped query then took 0.051 ms. Stored metadata, source/kind filters, stable ordering and lookup caps remain unchanged. A regression with 242 excluded source/kind rows checks filtering precedes the caps.

Related retrieval projects only metadata it uses, after the same scoped sort and row cap. Definitions keep relevant bare and qualified names; callers keep all variants of the matched calls. The global first 64 distinct callee names are selected on the server in the original definition/call order. Only useful callee results trigger a final source-call lookup, which includes every co-defined name, not just those 64. Public symbol lookups still return their complete metadata. Differential tests retain the original algorithm as an oracle and independently check qualifiers, ambiguity, scope, caps and co-defined names beyond the global budget.

The first projection implementation failed the branch gate at **56.275 ms** because it reversed every long, unqualified metadata entry twice. A bare-name fast path removes those allocations while preserving the original rightmost-delimiter rule for qualified names, including overlapping colons and Unicode. That optimization did not change vector retrieval, embeddings, accuracy limits or lookup caps.

The new GIN indexes use `fastupdate=off`. A default pending list made a negative 64-name lookup scan recent bulk inserts and miss the 50 ms gate. Direct index updates shift that work to ingestion instead; lookup limits and accuracy were not relaxed. This gate measures the added branch, not embedding/model startup, network latency or a production-sized corpus.

### Exact source-filtered vector ranking

The graph-only integrated implementation still failed the full HTTP gate at **163.784 ms** added latency, including a **50.826 ms** increase for an absent symbol with no graph matches. Source-filtered vector retrieval now preselects matching document ids once and uses an exact cosine-distance sort, with chunk-id tie breaking and the same 50-candidate cap. Source-null SQL remains byte-for-byte identical to the prior implementation, including its approximate HNSW path and kind-only behavior. All branches still use the same repeatable-read connection.

This deliberately changes approximate filtered results when HNSW misses nearer chunks. An independent exhaustive scalar cosine reference checked the actual shipping query on 42,166 chunks, including 1,000 other-tenant repos, two real BGE vectors, both custom and generic plans, broad/selective sources, kind filters, other/unknown tenants, absent scopes and large identical-vector ties. All 32 ordered-id/enumeration comparisons match; cosine distances differ from double-precision `math.fsum` by at most **0.000000167**. In the second-vector broad/code cases the original approximate path returned background copies at distance 0.340762 while exact ranking correctly finds the query vector's own chunk at distance zero.

The control matrix also records tradeoffs, not a universal speedup: a custom broad-source plan took 5.022 ms with HNSW versus 19.150 ms exact; a selective three-row custom lookup took 0.085 versus 0.567 ms. A generic selective lookup dropped from 22.346 to 0.933 ms by avoiding 22,166 chunk/source probes, while a 20-row other-tenant generic query rose from 0.224 to 1.761 ms because the exact document/tenant bitmap intersection visits more index entries. These are diagnostic warm-plan times, not cold HTTP acceptance results.

The integrated first-HTTP comparison after this change passes the light fixture at **16.999 ms** maximum added latency but still **fails** the heavy fixture at **55.689 ms** against the strict **50 ms** limit. All 16 pairs in both server orders, 32 fresh processes, remain recorded. The failing pair is reverse-order `checkout`; the other heavy increases are at most 45.547 ms. This is improvement over the earlier 163.784 ms failure, not a passing acceptance result. Follow-up phase diagnostics found first-auth connection-acquisition contention in the unchanged startup/pool code on either executable, not a proven feature-specific regression.

The production pool now eagerly establishes two connections, still capped at ten, before workers start. A controlled diagnostic showed authentication waiting 49.529 ms to open a second backend even though the initial poll returned the first backend after about 6 ms. A ready spare avoids that race without delaying the worker or moving the readiness message. It costs one extra idle Postgres backend, including for the migration CLI's shared constructor. The startup regression checks two live backends independently through `pg_stat_activity`, holds the first poll's connection across real authentication, and verifies that no third backend opens; arbitrary one-connection caller pools remain unchanged. Full first-HTTP comparisons also record startup and startup-plus-first-request times, rather than hiding the extra connection's startup cost outside the request timer.

### Final integrated first-HTTP comparison

After the ready-spare fix, the required gate passes against the current #50 baseline on genuine, byte-paired #50/#51 schemas. The light fixture's maximum added HTTP latency is **20.067 ms**; the heavy fixture's is **42.274 ms**, both strictly below **50 ms**. The same 16 paired searches, 32 fresh server processes, first request without query warm-up, `source=lore`, `k=50`, both server orders and every observation are retained.

| Query shape | Largest candidate minus current #50 HTTP time (ms) |
| --- | ---: |
| Real `index_text` lookup | -62.926 |
| Eight real symbols | 20.067 |
| Heavy `checkout` | -16.921 |
| Heavy `persist` | -57.764 |
| Absent symbol | -20.454 |
| Two background names | 42.274 |
| Five heavy symbols | -35.832 |
| Eight heavy symbols | -39.302 |

Startup is not free and is reported separately, from process launch through readiness. Light-fixture startup deltas range from **-35.738 to +2,125.615 ms**; the first `index_text` candidate starts in 2,901.392 ms versus 775.777 ms baseline, and startup-plus-first-request increases by **2,062.478 ms**. This outlier is included, not discarded. Heavy-fixture startup deltas range from **-6.089 to +179.632 ms**, with maximum combined increase **137.170 ms**. These local startup/model/filesystem measurements vary substantially and do not isolate the extra backend's connection cost. The request gate passes, but this is **not** a claim of improved startup or universally faster end-to-end searches. All paired startup and combined observations were recorded in addition to, not instead of, the unchanged request timer.

### Original standalone HTTP and ingestion measurements

The following HTTP and ingestion measurements compare the original standalone implementation against `105500e`, not the integrated main baseline. Full cold HTTP searches exercise the default local embedding model, `k=50` and `source=lore`. Each baseline/candidate pair starts fresh servers on cloned data with identical stored vectors and ids; their first search is timed after the server finishes loading the model, without query warm-up. Each query runs in both server orders. The real 1,773-chunk corpus covers `index_text` and eight real symbol names. A 21,863-chunk copy adds 20,000 distractors, 40 five-per-name definitions with maximum-sized metadata arrays, and 50 caller candidates as in the branch gate.

| Query shape | Largest candidate minus baseline (ms) |
| --- | ---: |
| Real `index_text` lookup | 10.392 |
| Eight real symbols | 25.962 |
| Heavy corpus `checkout` lookup | 25.185 |
| Heavy corpus `persist` lookup | -7.343 |
| Absent symbol | -54.154 |
| Two background names | 12.679 |
| Five heavy symbols, caller/callee expansion | 18.152 |
| Eight heavy symbols, definition budget full | 2.457 |

All 16 paired first searches (32 fresh server processes) remain below **50 ms** added latency; the largest increase is **25.962 ms**, not an average. Absolute candidate search times reach 161.189 ms, since embedding and existing retrieval are included. These local measurements are not a production latency guarantee or a controlled speedup claim.

The full-entrypoint check caught two costs absent from the original unfiltered branch-only check: extra cold pool connections, and source-first plans that scanned all chunks for absent callee names. Search now embeds before acquiring a single connection for lexical, vector, related and hit loading. Symbol queries probe GIN one name at a time, materialize matching ids only, then apply source filters and stable ordering. Limits, matches and ambiguity rules are unchanged.

With the final migration, real local embeddings, and the fixed 117-file corpus, a fresh push wrote 1,773 chunks in **135.642 s**, including **123.435 s** embedding all 1,773 chunks. A forced metadata re-index wrote the same 1,773 chunks in **7.601 s**, reusing every vector (zero embedding calls). The re-index test invalidated stored content hashes in its isolated database so it exercised actual deletes/inserts and GIN updates, not the unchanged-file fast path. These are representative write-cost measurements, not a controlled ingestion speedup claim against the baseline.
