# Search ranking across repositories

Search combines full-text, vector, enumeration and call-graph ranks. File-level hints reduce the
contribution of test and generated chunks to 70% each (49% if both apply). They are never
removed. Queries explicitly asking for tests, fixtures, mocks or snapshots do not receive
the test penalty; queries mentioning generated code, protobuf or codegen do not receive
the generated penalty. This applies to source-filtered and unscoped searches alike.

The index marks these files:

- Tests: directories `tests`, `test`, `testdata`, `fixtures`, `mocks`, `__snapshots__`,
  `e2e`; filenames `*_test.*`, `test_*.*`, `*.spec.*`.
- Generated: directories `out`, `gen`; filenames `*_generated.*`, `*.pb.*`; or a line
  among the first ten containing both `Code generated` and `DO NOT EDIT`.

Matching is case-insensitive and checks directory components, not substrings. A generated
header marks every chunk in that file. The migration backfills existing paths and retained
headers. Run `lore repo push --full` to detect headers old chunkers did not retain; stored
embeddings are reused when their input is unchanged.

For behavioral questions mentioning limits, enforcement, rejection, what happens or what
something does, a README receives a small 10% penalty. Other documentation has no penalty.
Explicit requests for a README or documentation skip this prior. This is a ranking hint,
not a claim that every README is wrong.

Before applying the requested result count, search collapses **exact full chunk text**
found in different sources among the candidate hits. It keeps the highest-ranked location
and reports other source names in `also_in` in the search JSON, or `also in: …` in CLI,
MCP, sources answers and model passages. Similar snippets do not collapse. Repeats
confined to one source retain separate locations. Only the current tenant and requested source
and kind filters can contribute duplicate names. No second unfiltered lookup is used.

SQL chunks with stored object subjects also require an identical full migration path to
collapse. Identical text at different migration positions remains separate chronological
evidence. For a collapsed SQL passage, latest-change lookup still checks each original
source separately. Companions retain their actual source/file, and historical warnings use
`source:path`, so a change in one alias is not attributed to another source.

## Reproducing the multi-repo regression eval

Use a local server with the real `fastembed` embedder and the fake LLM, not the hash embedder.
No Anthropic requests or model grading are needed. Create a tenant key with `ingest,read`
scopes, then run from the repository root:

```sh
python3 bench/eval/multirepo.py baseline http://localhost:8080 "$EVAL_KEY" \
  --bin /absolute/path/to/lore --work /absolute/scratch/multi33
```

The setup creates six miniature Git repositories from the fixed
`bench/eval/testdata/multirepo.json` corpus, pushes them into the tenant and runs the
#32 harness on fifteen independently labeled questions. Every request omits `source`,
but scoring still requires the gold source, path and overlapping lines. Nine questions
cover fixtures vs Helm configuration, repeated generated schemas vs plugin code, and
contradictory README claims vs enforcing code. Six guard explicit test, snapshot,
generated-output and documentation requests. The setup records the corpus SHA-256 and
each source commit in the work directory's `manifest.json`.

Run the changed server against the same database and repeat with label `ranking`.
Reuse the same corpus work directory, then compare:

```sh
bench/eval/eval --diff baseline ranking
```

Use `bench/eval/local.sh` separately for the unchanged twenty-question lore set. That
source is pinned to its original commit, so a comparison changes ranking, not the
indexed code or gold labels. The private historical 33-question set referenced in #33
is not shipped here. The acceptance gate uses the public twenty-question set as an
explicitly approved substitute: matched hit@12 and MRR must not decrease. This is not
a reproduction of the historical 18/33 hit@12 or 0.355 MRR reference.

## Measured results

Matched local runs compare main `ad4654b` with ranking code at `a2eafaa`, using
`fastembed:bge-small-en-v1.5`, model snapshot
`ea104dacec62c0de699686887e3f920caeb4f3e3`, and the fake LLM. All binaries used the
same debug profile, two Cargo build jobs and no incremental compilation. Each server
started in a fresh process. They reused the same stored vectors and indexed files,
not fresh independently sampled embeddings. The public source has 1,773 chunks;
the representative multi-repo corpus has 22 chunks across six sources.

| Set / variant | hit@5 | hit@12 | hit@50 | MRR |
|---|---:|---:|---:|---:|
| Public20, baseline | 8/20 | 13/20 | 18/20 | 0.365 |
| Public20, ranking | 8/20 | 14/20 | 18/20 | 0.376 |
| Multi15, baseline | 15/15 | 15/15 | 15/15 | 0.789 |
| Multi15, ranking without README prior | 15/15 | 15/15 | 15/15 | 0.900 |
| Multi15, complete ranking | 15/15 | 15/15 | 15/15 | 1.000 |

No public20 or multi15 gold rank worsened. Two Helm questions moved from rank 3
to 1. The toolset, payload and retry questions moved from 3/3/2 to 1/1/1.
Without the README prior those three behavioral answers remain at rank 2, so the
small prior is retained. Explicit test, snapshot, generated and docs requests stay
at rank 1. The plugin-span answer was already first in this corpus; this run does
not claim an improvement for its gold rank. Its repeated generated copies disappear
from the top five, and exact copies remain retrievable with other source names.

Client latency for the actual `k=50` eval requests:

| Set | Median baseline / ranking | Worst steady baseline / ranking | First call baseline / ranking |
|---|---:|---:|---:|
| Public20 | 33.9 / 42.5 ms | 44.0 / 58.3 ms | 386.5 / 50.0 ms |
| Multi15 | 16.7 / 14.0 ms | 20.0 / 17.3 ms | 20.3 / 14.4 ms |

The worst matched per-question increase is 18.2 ms on public20 and 0.5 ms on
multi15. Public median latency increases by 8.6 ms because more candidate passages
are hydrated before the limit, so this is not an across-the-board speed improvement.
These are one-pass local observations, not a large-production-index performance
guarantee. First calls are recorded separately from the steady median; the later
multi15 run in each process is not a cold-model measurement.

Raw per-question results, category summaries, first/worst latency and binary/runtime
provenance are in [`bench/eval/results/`](../bench/eval/results/):
`i33-baseline-{public,multi}.json`, `i33-no-readme-{public,multi}.json`,
`i33-ranking-{public,multi}.json`, and `i33-manifest.json`. The baseline multi
result also records the fixture SHA-256 and each source commit. Compare them with:

```sh
bench/eval/eval --diff i33-baseline-public i33-ranking-public
bench/eval/eval --diff i33-baseline-multi i33-ranking-multi
bench/eval/eval --diff i33-no-readme-multi i33-ranking-multi
```
