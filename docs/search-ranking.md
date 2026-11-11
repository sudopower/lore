# Search ranking across repositories

Search combines full-text, vector and enumeration ranks. File-level hints reduce the
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
This is a ranking hint, not a claim that every README is wrong.

Before applying the requested result count, search collapses **exact full chunk text**
found in different sources among the candidate hits. It keeps the highest-ranked location
and reports other source names in `also_in` in the search JSON, or `also in: …` in CLI,
MCP, sources answers and model passages. Similar snippets do not collapse. Repeats
confined to one source retain separate locations. Only the current tenant and requested source
and kind filters can contribute duplicate names. No second unfiltered lookup is used.

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
