#!/usr/bin/env bash
# Times lore (search, fast ask, deep ask) against Claude Code on the same questions about one repo.
#
#   scripts/bench_vs_claude_code.sh <repo> <questions-file> [out-dir]
#
# <repo> is a local git checkout. <questions-file> has one question per line; blank lines and
# lines starting with # are skipped. Results go to [out-dir] (default bench-<unix time>):
#   push.json, push_secs.txt   the `lore repo push --json` report and how long it took
#   search-qN.txt              the time of each GET /v1/search, in seconds
#   lore-qN-fast.json          the POST /v1/ask response in fast mode
#   lore-qN-deep.json          the same in deep mode
#   cc-qN.json, cc-qN.err      Claude Code's --output-format json, and its stderr
#   summary.md                 the table this script prints at the end
#
# Needs LORE_URL and LORE_KEY in the environment, and curl, jq and claude on PATH. The key needs
# the read scope, and ingest as well unless BENCH_PUSH=0. Use a tenant that holds only this repo,
# so lore's answers draw on nothing else.
#
# Optional environment:
#   BENCH_MODEL         model for Claude Code (default claude-opus-5-5). For a fair comparison,
#                       run the lore server with LORE_FAST_MODEL and LORE_DEEP_MODEL set to it too
#   BENCH_PUSH          1 (default) runs `lore repo push <repo>` first, so lore answers from the
#                       repo's HEAD, and needs lore on PATH; 0 skips the push
#   BENCH_SEARCH_RUNS   how many times each search is timed (default 5); the median is reported
#
# For each question, the fast answer, the deep answer and the Claude Code run start at the same
# time: they wait on the model API, not on this machine. Questions run one after another.
#
# This costs money: every question is one fast and one deep lore answer and one Claude Code run.
set -euo pipefail
shopt -s inherit_errexit 2>/dev/null || true

fail() {
  echo "BENCH FAIL: $*" >&2
  exit 1
}

[ $# -ge 2 ] && [ $# -le 3 ] || fail "usage: $0 <repo> <questions-file> [out-dir]"
[ -d "$1" ] || fail "no such directory: $1"
REPO=$(cd "$1" && pwd)
QUESTIONS=$2
[ -f "$QUESTIONS" ] || fail "no such file: $QUESTIONS"
OUT=${3:-bench-$(date +%s)}
: "${LORE_URL:?set LORE_URL to the lore server}"
: "${LORE_KEY:?set LORE_KEY to a tenant key}"
MODEL=${BENCH_MODEL:-claude-opus-5-5}
PUSH=${BENCH_PUSH:-1}
RUNS=${BENCH_SEARCH_RUNS:-5}
case $RUNS in '' | *[!0-9]* | 0) fail "BENCH_SEARCH_RUNS must be a positive integer" ;; esac

need=(curl jq claude)
[ "$PUSH" = 1 ] && need+=(lore)
for c in "${need[@]}"; do
  command -v "$c" >/dev/null || fail "$c is not on PATH"
done

questions=()
skip='^[[:space:]]*(#|$)'
while IFS= read -r line || [ -n "$line" ]; do
  [[ $line =~ $skip ]] && continue
  questions+=("$line")
done <"$QUESTIONS"
[ ${#questions[@]} -gt 0 ] || fail "$QUESTIONS has no questions"

mkdir -p "$OUT"
OUT=$(cd "$OUT" && pwd)

curl -fsS "$LORE_URL/v1/info" >"$OUT/info.json" || fail "no lore server answers at $LORE_URL"
DEEP=1
jq -e '.modes | index("deep")' "$OUT/info.json" >/dev/null ||
  { DEEP=0; echo "the server does not offer deep mode (LORE_LLM=anthropic does); skipping it" >&2; }

if [ "$PUSH" = 1 ]; then
  echo "pushing $REPO" >&2
  start=$(date +%s)
  lore repo push "$REPO" --json >"$OUT/push.json" ||
    fail "lore repo push failed (the key needs the ingest scope; BENCH_PUSH=0 skips the push)"
  echo $(($(date +%s) - start)) >"$OUT/push_secs.txt"
fi

# ask <n> <mode> <question>: one POST /v1/ask, the response saved as lore-q<n>-<mode>.json.
ask() {
  curl -sS "$LORE_URL/v1/ask" -H "Authorization: Bearer $LORE_KEY" \
    -H 'content-type: application/json' \
    -d "$(jq -n --arg q "$3" --arg m "$2" '{question: $q, mode: $m}')" >"$OUT/lore-q$1-$2.json"
}

# cc <n> <question>: one Claude Code run in the repo, read-only tools and no MCP servers.
cc() {
  (cd "$REPO" && claude -p "$2 Answer from this repository's code and cite file:line." \
    --model "$MODEL" --strict-mcp-config --allowedTools "Read,Grep,Glob" \
    --output-format json </dev/null >"$OUT/cc-q$1.json" 2>"$OUT/cc-q$1.err")
}

for i in "${!questions[@]}"; do
  n=$((i + 1))
  q=${questions[$i]}
  echo "question $n of ${#questions[@]}" >&2
  # Retrieval only, no model call. A failure here stops the run before it spends anything.
  for _ in $(seq "$RUNS"); do
    curl -fsS -o /dev/null -w '%{time_total}\n' -G "$LORE_URL/v1/search" \
      -H "Authorization: Bearer $LORE_KEY" --data-urlencode "q=$q" --data-urlencode k=12
  done >"$OUT/search-q$n.txt"
  ask "$n" fast "$q" &
  [ "$DEEP" = 1 ] && ask "$n" deep "$q" &
  cc "$n" "$q" &
  wait
done

# search_cell <file>: the median of the timed searches, in milliseconds.
search_cell() {
  sort -n "$1" | sed -n "$(((RUNS + 1) / 2))p" | awk '{ printf "%d ms", $1 * 1000 + 0.5 }'
}

# lore_cell <file>: "8.1 s", "16.1 s (4 tool calls)" for deep mode, or the error.
lore_cell() {
  [ -s "$1" ] || { echo "n/a"; return; }
  jq -r 'if (.latency_ms | type) == "number" then
           "\(.latency_ms / 100 + 0.5 | floor / 10) s"
           + (if .mode == "deep" then " (\(.tool_calls) tool calls)" else "" end)
         else "error: \(.error // "no answer")" end' "$1" 2>/dev/null || echo "error"
}

# cc_cell <file>: "52.5 s (6 turns, $0.38, 199k input tokens)", or "error".
cc_cell() {
  jq -r 'if (.duration_ms | type) == "number" and (.is_error | not) then
           "\(.duration_ms / 100 + 0.5 | floor / 10) s (\(.num_turns) turns, "
           + "$\(.total_cost_usd * 100 + 0.5 | floor / 100), "
           + "\((.usage.input_tokens + .usage.cache_creation_input_tokens
                + .usage.cache_read_input_tokens) / 1000 + 0.5 | floor)k input tokens)"
         else "error" end' "$1" 2>/dev/null || echo "error"
}

{
  if [ -s "$OUT/push.json" ]; then
    jq -r --arg secs "$(cat "$OUT/push_secs.txt")" \
      '"Indexed \(.files_indexed) files (\(.files_unchanged) unchanged) into \(.chunks) chunks in \($secs) s.\n"' \
      "$OUT/push.json"
  fi
  echo "| # | Question | Search (median of $RUNS) | lore fast | lore deep | Claude Code |"
  echo "|---|---|---|---|---|---|"
  for i in "${!questions[@]}"; do
    n=$((i + 1))
    short=$(printf '%s' "${questions[$i]}" | cut -c1-60 | tr '|' '/')
    printf '| %s | %s | %s | %s | %s | %s |\n' "$n" "$short" \
      "$(search_cell "$OUT/search-q$n.txt")" "$(lore_cell "$OUT/lore-q$n-fast.json")" \
      "$(lore_cell "$OUT/lore-q$n-deep.json")" "$(cc_cell "$OUT/cc-q$n.json")"
  done
} | tee "$OUT/summary.md"
echo "results in $OUT" >&2
