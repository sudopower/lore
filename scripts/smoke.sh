#!/usr/bin/env bash
# Smoke test for a running lore server.
#
#   scripts/smoke.sh <stage>      stage is m1, m2, or all (every defined stage_* function, in order)
#
# Needs LORE_URL and LORE_ADMIN_TOKEN in the environment, and lore, jq and git on PATH.
# Each run creates fresh tenants, so it can be repeated against the same server.
set -euo pipefail
shopt -s inherit_errexit 2>/dev/null || true

cd "$(dirname "$0")/.."

fail() {
  echo "SMOKE FAIL: $*" >&2
  exit 1
}

# assert_jq <json> <jq-filter> <msg>: fails unless `jq -e <filter>` is true for the json.
assert_jq() {
  local json=$1 filter=$2 msg=$3
  if ! printf '%s' "$json" | jq -e "$filter" >/dev/null 2>&1; then
    fail "$msg (jq filter: $filter; got: ${json:0:400})"
  fi
}

# new_tenant <suffix>: creates a tenant and prints an API key (scopes ingest,read) for it.
new_tenant() {
  local suffix=$1 name tenant key
  name="smoke-$(date +%s)-$RANDOM-$suffix"
  tenant=$(lore admin tenant create --name "$name" | jq -er '.id') \
    || fail "could not create tenant $name"
  key=$(lore admin key create --tenant "$tenant" --scopes ingest,read | jq -er '.key') \
    || fail "could not create a key for tenant $tenant"
  printf '%s\n' "$key"
}

# fixture_dir: prints a new git repo made from tests/fixtures/sample-repo, with one commit.
fixture_dir() {
  local dir
  dir=$(mktemp -d "$WORK/fixture.XXXXXX") || fail "could not create a temp dir"
  cp -R tests/fixtures/sample-repo/. "$dir"/ || fail "could not copy the fixture"
  git_smoke "$dir" init -q -b main
  git_smoke "$dir" add -A
  git_smoke "$dir" commit -q -m "sample repo"
  printf '%s\n' "$dir"
}

# git_smoke <dir> <git args...>: runs git with a throwaway identity, never touching global config.
git_smoke() {
  local dir=$1
  shift
  git -C "$dir" -c user.name=smoke -c user.email=smoke@example.invalid \
    -c commit.gpgsign=false "$@" >/dev/null \
    || fail "git $* failed in $dir"
}

# M1: search a codebase, isolated per tenant.
stage_m1() {
  local KA KB D R S

  KA=$(new_tenant a) || exit 1
  KB=$(new_tenant b) || exit 1
  D=$(fixture_dir) || exit 1

  R=$(LORE_KEY=$KA lore repo push "$D" --name sample --json) || fail "first push failed"
  assert_jq "$R" '.files_indexed >= 5' "first push should index at least 5 files"

  S=$(LORE_KEY=$KA lore search "plan limits" --json) || fail "search failed"
  assert_jq "$S" '.hits[0].path == "src/billing/plans.rs"' "first hit for 'plan limits' should be src/billing/plans.rs"

  printf '// smoke edit\n' >>"$D/src/billing/plans.rs"
  git_smoke "$D" commit -q -am "smoke edit"
  R=$(LORE_KEY=$KA lore repo push "$D" --name sample --json) || fail "second push failed"
  assert_jq "$R" '.files_indexed == 1' "second push should index exactly 1 file"

  S=$(LORE_KEY=$KB lore search "plan limits" --json) || fail "search as the second tenant failed"
  assert_jq "$S" '(.hits | type) == "array" and (.hits | length) == 0' "the second tenant should get 0 hits"

  echo "SMOKE M1 OK"
}

# M2: a cited fast-mode answer. Run against the fake model, so the answer is deterministic.
stage_m2() {
  local K D A

  K=$(new_tenant ask) || exit 1
  D=$(fixture_dir) || exit 1

  LORE_KEY=$K lore repo push "$D" --name sample --json >/dev/null || fail "push failed"

  A=$(LORE_KEY=$K lore ask --mode fast "how many plans are there and what are their limits?" --json) \
    || fail "ask failed"
  assert_jq "$A" '.mode == "fast"' "the answer should be in fast mode"
  assert_jq "$A" '.model == "fake"' "the answer should come from the fake model"
  assert_jq "$A" '[.citations[].path] | index("src/billing/plans.rs") != null' "the answer should cite src/billing/plans.rs"
  assert_jq "$A" '.latency_ms < 5000' "the answer should take under 5 seconds"

  echo "SMOKE M2 OK"
}

main() {
  local stage=${1:-} fn missing=""
  case "$stage" in
    m[0-9] | all) ;;
    *) fail "usage: scripts/smoke.sh <stage> (m1, m2, or all)" ;;
  esac

  [ -n "${LORE_URL:-}" ] || missing="$missing LORE_URL"
  [ -n "${LORE_ADMIN_TOKEN:-}" ] || missing="$missing LORE_ADMIN_TOKEN"
  for cmd in lore jq git; do
    command -v "$cmd" >/dev/null 2>&1 || missing="$missing $cmd(on PATH)"
  done
  [ -z "$missing" ] || fail "missing:$missing"
  export LORE_URL LORE_ADMIN_TOKEN

  WORK=$(mktemp -d) || fail "could not create a temp dir"
  trap 'rm -rf "$WORK"' EXIT

  if [ "$stage" = all ]; then
    for fn in $(declare -F | awk '{print $3}' | grep '^stage_' | sort); do
      "$fn"
    done
  else
    fn="stage_$stage"
    declare -F "$fn" >/dev/null || fail "stage $stage is not defined yet"
    "$fn"
  fi
}

main "$@"
