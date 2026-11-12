# SQL migration chronology

Search can retrieve an original SQL definition without retrieving the migration that changes
it. SQL chronology adds the newest matching later migration directly after that older evidence.
For example, a six-value `CHECK` and a later alteration adding a seventh value remain separate
sources, newest first. Sources and fast answers warn that the older passage has later changes.

Allowed-value changes are the exception: a same-subject `ALTER TYPE ADD/RENAME VALUE` or
table-level `ALTER TABLE ADD [CONSTRAINT name] CHECK (...)` goes before its older definition.
An unrelated `ADD COLUMN body`, column default, comment, quoted keyword or string containing
`CHECK` does not trigger that promotion. An already earlier-ranked newer hit keeps its place.
Classification uses structural tokens from the full statement before chunk truncation,
and is stored per canonical subject, not once for a multi-object chunk. It uses no query-word
heuristic and adds no ranking prior.

## Identity and ordering

SQL subjects are stored case-folded and schema-qualified, separately from searchable symbols.
Canonical identities retain quoted case and identifier boundaries: unqualified `accounts`
means `public.accounts`, but `"Accounts"` is distinct, as are `"event.log"` and `event.log`.
Tables and types remain distinct. Multiple subjects in a chunk and multi-object `DROP`
retain every identity. Hash-indexed long identities still require full identity equality.

Lookup fixes tenant, actual source ID, document kind and canonical identity. It selects the
newest chunk by bytewise full stored path, then the last chunk in that file. Ingestion time
and numeric filename parsing do not affect order: `9.sql` sorts after `10.sql`.

Paths through 2,048 bytes use an ordered index. Longer stored paths use a second index to
find and sort matching overflow rows, then compare their newest candidate with the ordinary
index result. This preserves complete matching, but overflow work grows with the number and
size of matching paths. It is not a constant-time fallback.

## Ranking, duplicate sources and snapshots

Chronology runs after ranking priors and duplicate collapse. SQL subjects require both
identical full text and the same full migration path to collapse across sources. Identical
SQL at different migration positions remains separate evidence.

A collapsed passage retains private original chunk IDs from the authorized retrieval pool.
Every source alias receives its own canonical lookup. A change only in source B does not
become a change in source A: companions retain their actual source/file, while warnings
identify changes as `source:path`. Valid duplicate positions still carry `also_in`.
Duplicate companions cannot consume extra result slots.

Companions inherit their seed's ranked score. This is an evidence expansion, not another
similarity score. Both newest and older evidence count toward `k`; only one may fit
at `k=1`. Non-SQL priors and duplicate behavior are unchanged. SQL adds no public JSON fields.

Ranking, SQL lookup and hydration share the search's repeatable-read snapshot. Lookup rows
are inserted/replaced with chunks and cascade on deletion. Failed or pending replacements
cannot publish partial chronology, and embedding-cache publication remains post-commit.

## Limits and upgrade

This is not a live schema interpreter. It does not execute `SET search_path`, reconstruct
rename chains, infer framework/date ordering, or gather every intermediate alteration.
Value-change promotion deliberately recognizes the enum and table-level CHECK forms above,
not every SQL dialect's column-alteration syntax. Other changes still appear as companions.
Every SQL file with a stored path can contribute, without guessing migration filename rules.

The fallback does not remove existing ingestion limits: Git's strict object checks, host
checkout limits, document-ID index size, and the documents API's path limit still apply.
The 16-KiB path checks use valid store inputs with independent short document IDs and uncut
metadata paths. An invalid oversized Git path is separately checked for clean rejection
without index side effects; valid nested Git paths exercise real bundle ingestion.

SQL uses migration `0010_sql_subjects.sql` and chunker version 5, preserving ranking migration
0011. Run `lore repo push --full` after upgrading to populate existing SQL subjects.

## Validation and reproduction

`tests/sql_chronology.rs` covers search, actual sources/fast answer handlers and prompt
capture, path ordering/deletion, canonical identity isolation, multiple subjects, collision
checks, replacement rollback and visibility, cascades, long-path boundaries, source aliases,
repeated migration positions and result-count boundaries. SQL parsing has 20 unit cases,
including comments/strings, arbitrary names, mixed-subject chunks and pre-truncation flags.
The ignored real-embedding scaling measurement is explicit opt-in coverage, not a skipped
feature test.
Final `make lint` and `make test` pass on the structural ordering implementation:
709 tests passed, 6 ignored across 33 test binaries.

The independently hand-labelled twelve-question fixture in
`bench/eval/questions/sql-chronology.json` requires original plus newest evidence using
`need: all`. Gold ranges/reference answers come from the fixed SQL, not retrieval output.
Run `bench/eval/sql.sh` and compare with the #32 retrieval harness. That harness measures
overlap ranks, so integration tests separately enforce current-value changes before their
definition and unrelated column changes after it.

### Matched retrieval results

The final comparison uses main `4c981d4` and SQL candidate `d7b8fba`, one cached local
`fastembed:bge-small-en-v1.5` snapshot, default debug profile and unchanged gold labels.
Both variants index 1,773 public chunks at `105500e`, the same 24 SQL fixture chunks,
and the same multi-repo fixture. Exact content-addressed vectors were copied unchanged
to a fresh owned validation database; both variants use normal migrations.

| Set | Hit@5 | Hit@12 | Hit@50 | MRR | Median latency |
|---|---:|---:|---:|---:|---:|
| Public20 | 8 -> 9 | 14 -> 14 | 18 -> 18 | .376 -> .384 | 32.1 -> 37.2 ms |
| SQL12, both gold ranges | 12 -> 12 | 12 -> 12 | 12 -> 12 | .431 -> .479 | 16.5 -> 19.5 ms |
| Mixed-tenant multi15 | 15 -> 15 | 15 -> 15 | 15 -> 15 | .839 -> .839 | 35.5 -> 41.5 ms |

Only public question `lore-05` changes rank, from 10 to 4. The document-kind CREATE stays
first; its unrelated body-column alteration follows it. Every multi-repo question rank
is unchanged. The SQL qualified-dot case worsens from 3 to 4; the other changed SQL cases
improve, and all original/newest pairs stay top-five. Enum current-value evidence changes
from original/latest positions 1/2 to 2/1; CHECK evidence stays 2/1.

The multi-repo comparison uses a tenant also containing public and SQL fixtures, so its
.839 is not comparable to an isolated multi-repo tenant's 1.000. These are quality gains,
not speedups: public first/worst latency is 55.4 -> 103.5 ms, SQL worst is 28.3 -> 29.5 ms,
and mixed multi-repo worst is 47.7 -> 72.0 ms.

Raw question ranks, gold positions, top hits and timings are committed as
`bench/eval/results/i36-ordering-{baseline,sql}-{public,sql,multi}.json`.
`bench/eval/results/i36-ordering-provenance.json` records source/executable hashes,
model identity, exact cache reuse and actual fast-answer historical-warning proof.

### Local scaling observations

Each row is a fresh test process using cached `fastembed:bge-small-en-v1.5`, snapshot
`ea104dacec62c0de699686887e3f920caeb4f3e3`, default debug profile, two Cargo
build jobs and no incremental compilation. Each ingests one original plus 1,000 matching
later migrations through the real store, then makes eight authenticated HTTP-handler
searches at `k=5`. Every request checks newest and retained original evidence.
Model loading and ingestion are recorded separately from query latency.

| Stored paths | First / worst query | Steady median | Ingest | Peak process RSS |
|---|---:|---:|---:|---:|
| Ordinary short paths | 40.1 / 40.1 ms | 13.3 ms | 52.4 s | 549.2 MiB |
| 2,049-byte matching overflow | 63.0 / 63.0 ms | 34.2 ms | 58.6 s | 490.0 MiB |
| 16,384-byte matching overflow | 133.7 / 133.7 ms | 57.6 ms | 81.1 s | 507.1 MiB |

Peak RSS includes both local model instances but excludes PostgreSQL. These measure the
actual HTTP handler in-process, not network round-trip overhead. The larger overflow rows
cost more, as expected; this is a local scaling observation, not a production latency bound.
Full timings, model-load cost and executable provenance are in
`bench/eval/results/i36-scaling.json`. See the eval README for the opt-in command.
These measurements are from the earlier unconditional-newest-first variant. The final
structural ordering refinement preserves lookup/index keys and predicates; these are
historical index-scaling observations, not timings of the final executable. The current
opt-in test checks original-then-newest order for its ordinary `ADD COLUMN` fixture.
