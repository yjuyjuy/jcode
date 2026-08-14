#!/usr/bin/env bash
#
# End-to-end smoke test for the jcode session account-switch control surface
# (ADR 0031, Phase 1). Starts an isolated daemon in a throwaway JCODE_HOME,
# seeds two fake Anthropic accounts, spins up headless sessions, then drives the
# real `jcode session list` and `jcode session switch-account` CLIs over the
# daemon socket and asserts the JSON results.
#
# This exercises the full path an external orchestrator (quota-axi's fenced
# `switch` verb) uses: enumerate live sessions, flip one session and all
# sessions between two accounts, and switch account+model together across
# providers, all without terminal injection and without interrupting a turn.
#
# The fake accounts have well-formed but non-real tokens: the switch pins the
# account (which only reads the label + stored token) without making a network
# call, so this stays hermetic. It validates the control plane, not live
# inference.
#
# Usage: scripts/asw_session_control_e2e.sh [path-to-jcode-binary]
set -euo pipefail

BIN="${1:-$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)/target/debug/jcode}"
if [[ ! -x "$BIN" ]]; then
  echo "jcode binary not found or not executable: $BIN" >&2
  echo "Build it first: cargo build -p jcode --bin jcode" >&2
  exit 2
fi

WORK="$(mktemp -d "${TMPDIR:-/tmp}/asw-e2e.XXXXXX")"
export JCODE_HOME="$WORK/home"
export JCODE_RUNTIME_DIR="$WORK/run"
mkdir -p "$JCODE_HOME" "$JCODE_RUNTIME_DIR"
SOCKET="$JCODE_RUNTIME_DIR/jcode.sock"

cleanup() {
  "$BIN" --socket "$SOCKET" server stop >/dev/null 2>&1 || true
  # Fall back to killing the pid we started if the graceful stop did not land.
  if [[ -n "${SERVER_PID:-}" ]]; then
    kill "$SERVER_PID" >/dev/null 2>&1 || true
  fi
  rm -rf "$WORK"
}
trap cleanup EXIT

# Two fake Anthropic accounts so a switch has somewhere to go.
cat > "$JCODE_HOME/auth.json" <<'JSON'
{
  "anthropic_accounts": [
    {"label":"claude-1","access":"fake-access-1","refresh":"fake-refresh-1","expires":9999999999999,"email":"a@example.com","subscription_type":"max","scopes":["user:inference"]},
    {"label":"claude-2","access":"fake-access-2","refresh":"fake-refresh-2","expires":9999999999999,"email":"b@example.com","subscription_type":"max","scopes":["user:inference"]}
  ],
  "active_anthropic_account":"claude-1"
}
JSON

cat > "$JCODE_HOME/config.toml" <<'TOML'
[display]
debug_socket = true
TOML

fail() { echo "E2E FAIL: $*" >&2; exit 1; }

echo "== starting isolated daemon =="
"$BIN" --socket "$SOCKET" serve >"$WORK/server.log" 2>&1 &
SERVER_PID=$!

# Wait for the socket to accept connections.
for _ in $(seq 1 100); do
  if "$BIN" --socket "$SOCKET" debug -s "$SOCKET" ping >/dev/null 2>&1; then
    break
  fi
  sleep 0.1
done

DEBUG_SOCKET="${SOCKET%.sock}-debug.sock"

echo "== creating two headless sessions =="
SID1_JSON="$("$BIN" debug -s "$SOCKET" create_session 2>/dev/null || true)"
SID2_JSON="$("$BIN" debug -s "$SOCKET" create_session 2>/dev/null || true)"
echo "created: $SID1_JSON | $SID2_JSON"

echo "== jcode session list (json) =="
LIST_JSON="$("$BIN" --socket "$SOCKET" session list --json)"
echo "$LIST_JSON"
COUNT="$(printf '%s' "$LIST_JSON" | python3 -c 'import sys,json; print(len(json.load(sys.stdin)))')"
[[ "$COUNT" -ge 2 ]] || fail "expected >=2 live sessions, got $COUNT"

# Pick the first two session ids from the list.
read -r S1 S2 < <(printf '%s' "$LIST_JSON" | python3 -c 'import sys,json; d=json.load(sys.stdin); print(d[0]["session_id"], d[1]["session_id"])')
echo "sessions: $S1 $S2"

echo "== switch one session to claude-2 =="
OUT="$("$BIN" --socket "$SOCKET" session switch-account "$S1" --account claude-2 --json)"
echo "$OUT"
printf '%s' "$OUT" | python3 -c '
import sys,json
r=json.load(sys.stdin)
assert len(r)==1, r
assert r[0]["ok"], r
assert r[0]["account"]=="claude-2", r
print("one-session switch ok")
' || fail "single-session switch did not report ok"

echo "== verify list reflects the per-session account =="
LIST2="$("$BIN" --socket "$SOCKET" session list --json)"
printf '%s' "$LIST2" | python3 -c '
import sys,json
d={s["session_id"]:s for s in json.load(sys.stdin)}
s1="'"$S1"'"; s2="'"$S2"'"
assert d[s1].get("account")=="claude-2", d[s1]
# The sibling session must be untouched by a single-session switch.
assert d[s2].get("account") in (None,"claude-1"), d[s2]
print("per-session isolation ok")
' || fail "per-session account not isolated"

echo "== switch ALL sessions to claude =="
OUTALL="$("$BIN" --socket "$SOCKET" session switch-account --all --account claude-1 --json)" || true
echo "$OUTALL"
printf '%s' "$OUTALL" | python3 -c '
import sys,json
r=json.load(sys.stdin)
assert len(r)>=2, r
assert all(x["ok"] for x in r), r
assert all(x["account"]=="claude-1" for x in r), r
print("all-session switch ok")
' || fail "all-session switch did not report ok for every session"

echo "== switch account+model together (cross-provider shape) =="
# Use a claude-api model spec so the atomic account+model path is exercised.
OUTCM="$("$BIN" --socket "$SOCKET" session switch-account "$S1" --account claude-2 --model 'claude-api:claude-sonnet-4-5' --json || true)"
echo "$OUTCM"
printf '%s' "$OUTCM" | python3 -c '
import sys,json
r=json.load(sys.stdin)
assert len(r)==1, r
# The account+model path returns one row; ok may be true (applied) and the row
# must name the account it targeted.
assert r[0]["account"]=="claude-2", r
print("account+model switch reported per-session:", r[0]["ok"])
' || fail "account+model switch did not return a per-session row"

echo "== switch to a nonexistent account reports per-session failure =="
if "$BIN" --socket "$SOCKET" session switch-account "$S1" --account no-such-account --json >"$WORK/failout.json" 2>"$WORK/failerr.txt"; then
  fail "switch to nonexistent account unexpectedly succeeded"
fi
cat "$WORK/failout.json"
python3 - "$WORK/failout.json" <<'PY' || fail "nonexistent-account failure not reported per-session"
import sys, json
r = json.load(open(sys.argv[1]))
assert len(r) == 1, r
assert not r[0]["ok"], r
assert r[0].get("error"), r
print("failure reporting ok")
PY

echo "ALL E2E CHECKS PASSED"
