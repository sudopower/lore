#!/usr/bin/env bash
# Index the fixed, independently labelled SQL fixture and run the #32 retrieval harness.
# Uses a local server only. No language-model calls.
set -euo pipefail
cd "$(dirname "$0")/../.."
ROOT=$(pwd)
export LORE_URL=${LORE_URL:-http://localhost:8080}
export LORE_ADMIN_TOKEN=${LORE_ADMIN_TOKEN:-dev-admin}
LORE_BIN=${LORE_BIN:-$ROOT/target/debug/lore}
LABEL=${1:?usage: bash bench/eval/sql.sh LABEL}
WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT
cp -R bench/eval/fixtures/sql-chronology/. "$WORK/"
git -C "$WORK" init -q
git -C "$WORK" add .
git -C "$WORK" commit -qm 'Fixture: Record SQL chronology evidence'
KEY=${EVAL_KEY:-}
if [ -z "$KEY" ]; then
    tenant=$("$LORE_BIN" admin tenant create --name "sql-eval-$(date +%s)-$RANDOM" |
        python3 -c 'import json,sys; print(json.load(sys.stdin)["id"])')
    KEY=$("$LORE_BIN" admin key create --tenant "$tenant" --scopes ingest,read |
        python3 -c 'import json,sys; print(json.load(sys.stdin)["key"])')
fi
LORE_KEY=$KEY "$LORE_BIN" repo push "$WORK" --name sql-chronology >&2
python3 bench/eval/retrieval.py "$LABEL" "$LORE_URL" "$KEY" bench/eval/questions/sql-chronology.json
