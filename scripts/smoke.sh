#!/usr/bin/env bash
# End-to-end check of Fleet-51 against a stand-in Ollama: first launch creates a bot, bots
# chat (with reasoning), use different models, write files only inside their folder, and
# survive an update handoff. Needs bash, curl and python3. Nothing outside a temp dir is touched.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
WORK="$(mktemp -d "${TMPDIR:-/tmp}/fleet51-smoke.XXXXXX")"
export FLEET51_HOME="$WORK/home" FLEET51_BOTS_DIR="$WORK/bots" OLLAMA_HOST="127.0.0.1:11499" FLEET51_ONLINE=1
PORT=7499
BIN="$ROOT/target/debug/fleet51"
FAKE_PID=""
cleanup() {
  "$BIN" down >/dev/null 2>&1 || true
  [ -n "$FAKE_PID" ] && kill "$FAKE_PID" 2>/dev/null || true
  sleep 1
  rm -rf "$WORK"
}
trap cleanup EXIT
step() { printf '\n== %s\n' "$*"; }
fail() { echo "FAIL: $*"; exit 1; }
api() { curl -sS -H "Authorization: Bearer $(cat "$FLEET51_HOME/token")" -H 'Content-Type: application/json' "http://127.0.0.1:$PORT$1" ${2:+-d "$2"}; }
json() { python3 -c "import json,sys; d=json.load(sys.stdin); print($1)"; }
# Wait until the bot's messages contain an event matching a Python expression over `e`.
wait_for() {
  for _ in $(seq 100); do
    if api "/api/bots/$1/messages" | python3 -c "import json,sys; sys.exit(0 if any($2 for e in json.load(sys.stdin)) else 1)"; then return 0; fi
    sleep 0.2
  done
  api "/api/bots/$1/messages"; echo; fail "timed out waiting for: $2"
}

(cd "$ROOT" && cargo build -q)
python3 "$ROOT/scripts/fake_ollama.py" 11499 & FAKE_PID=$!
sleep 1

step "First launch: the service starts, finds Ollama and creates the first bot"
"$BIN" up --listen "127.0.0.1:$PORT"
for _ in $(seq 50); do [ "$(api /api/bots | json 'len(d)')" = "1" ] && break; sleep 0.2; done
"$BIN" status
"$BIN" status | grep -q "models: ready (system" || fail "model engine not ready"
FLEET=$(api /api/bots | json 'd[0]["id"]')
[ "$(api /api/bots | json 'd[0]["name"]')" = "Fleet" ] || fail "first bot is not called Fleet"

step "Chat: the reply and its reasoning arrive"
api "/api/bots/$FLEET/messages" '{"text":"hello"}' >/dev/null
wait_for "$FLEET" "e['type']=='text' and 'Hello from qwen3:1.7b' in e['text']"
wait_for "$FLEET" "e['type']=='reasoning'"

step "A second bot on a different model, chatting at the same time"
SCOUT=$(api /api/bots '{"name":"Scout","shape":"star","color":"green"}' | json 'd["id"]')
api "/api/bots/$SCOUT/model" '{"model":"qwen3:0.6b"}' >/dev/null
api "/api/bots/$SCOUT/messages" '{"text":"hi scout"}' >/dev/null
api "/api/bots/$FLEET/messages" '{"text":"hi again"}' >/dev/null
wait_for "$SCOUT" "e['type']=='text' and 'qwen3:0.6b' in e['text']"
[ "$(api "/api/bots/$FLEET/messages" | json "sum(e['type']=='text' for e in d)")" = "2" ] || wait_for "$FLEET" "e['type']=='text' and e['seq']>8"

step "Think off: no reasoning in the next reply"
api "/api/bots/$SCOUT/think" '{"think":false}' >/dev/null
BEFORE=$(api "/api/bots/$SCOUT/messages" | json "sum(e['type']=='reasoning' for e in d)")
api "/api/bots/$SCOUT/messages" '{"text":"quick one"}' >/dev/null
wait_for "$SCOUT" "e['type']=='text' and e['text'].startswith('Hello') and e['seq']>12"
AFTER=$(api "/api/bots/$SCOUT/messages" | json "sum(e['type']=='reasoning' for e in d)")
[ "$BEFORE" = "$AFTER" ] || fail "reasoning appeared with Think off"

step "Files: a bot writes inside its folder, and cannot write outside it"
api "/api/bots/$SCOUT/messages" '{"text":"save a note"}' >/dev/null
wait_for "$SCOUT" "e['type']=='text' and 'wrote notes.md' in e['text']"
[ -f "$FLEET51_BOTS_DIR/Scout/notes.md" ] || fail "notes.md missing"
api "/api/bots/$SCOUT/messages" '{"text":"escape please"}' >/dev/null
wait_for "$SCOUT" "e['type']=='text' and 'outside your folder' in e['text']"
[ ! -e "$FLEET51_BOTS_DIR/escaped.txt" ] || fail "bot wrote outside its folder"

step "Undo: a reply that changed files can be undone"
api "/api/bots/$SCOUT/messages" '{"text":"break it"}' >/dev/null
wait_for "$SCOUT" "e['type']=='text' and 'wrote notes.md' in e['text'] and e['seq']>$(api "/api/bots/$SCOUT/messages" | json "max(x['seq'] for x in d if x['type']=='checkpoint')")"
[ "$(cat "$FLEET51_BOTS_DIR/Scout/notes.md")" = "broken" ] || fail "notes.md not overwritten"
CK=$(api "/api/bots/$SCOUT/messages" | json "[e['id'] for e in d if e['type']=='checkpoint'][-1]")
api "/api/bots/$SCOUT/undo" "{\"checkpoint\":\"$CK\"}" | grep -q '"ok":true' || fail "undo refused"
[ "$(cat "$FLEET51_BOTS_DIR/Scout/notes.md")" = "hi" ] || fail "undo did not restore notes.md"
wait_for "$SCOUT" "e['type']=='undone'"

step "Run code: off unless switched on; then only inside the sandbox"
api "/api/bots/$FLEET/messages" '{"text":"run code"}' >/dev/null
wait_for "$FLEET" "e['type']=='text' and 'switched off' in e['text']"
[ ! -e "$FLEET51_BOTS_DIR/Fleet/out.txt" ] || fail "a bot ran code with running code off"
if api "/api/bots/$SCOUT/run-code" '{"run_code":true}' | grep -q '"run_code":true'; then
api "/api/bots/$SCOUT/messages" '{"text":"run code"}' >/dev/null
wait_for "$SCOUT" "e['type']=='text' and e['text'].startswith('Tool said') and e['seq']>$(api "/api/bots/$SCOUT/messages" | json "d[-1]['seq']")"
RAN=$(api "/api/bots/$SCOUT/messages" | json "[e['text'] for e in d if e['type']=='text'][-1]")
echo "$RAN"
[ ! -e "$FLEET51_BOTS_DIR/sandbox-escape.txt" ] || fail "code wrote outside the bot folder"
if echo "$RAN" | grep -q "ran-in-sandbox"; then
  [ -f "$FLEET51_BOTS_DIR/Scout/out.txt" ] || fail "sandboxed code could not write in its folder"
else
  fail "the sandbox did not run the code"
fi
elif [ "$(uname)" = Darwin ]; then
  fail "no sandbox found on macOS"
else
  echo "(no sandbox on this machine: running code cannot be switched on)"
fi

step "Clear chat: messages go, the bot starts fresh"
api "/api/bots/$FLEET/clear" '{}' >/dev/null
[ "$(api "/api/bots/$FLEET/messages" | json 'len(d)')" -le 2 ] || fail "chat not cleared"
api "/api/bots/$FLEET/messages" '{"text":"fresh start"}' >/dev/null
wait_for "$FLEET" "e['type']=='text'"

step "Automations: a bot runs saved instructions on a schedule, and on demand"
AUTO=$(api /api/automations "{\"bot\":\"$SCOUT\",\"name\":\"Morning report\",\"instructions\":\"daily summary please\",\"schedule\":{\"repeat\":\"daily\",\"times\":[\"07:30\"]}}" | json 'd["id"]')
[ "$(api /api/automations | json 'd[0]["next_run"] is not None')" = "True" ] || fail "no next run"
api /api/automations "{\"bot\":\"$SCOUT\",\"name\":\"Past\",\"instructions\":\"x\",\"schedule\":{\"repeat\":\"once\",\"times\":[\"09:00\"],\"date\":\"2020-01-01\"}}" | grep -q "already passed" || fail "a time in the past was accepted"
api "/api/automations/$AUTO/run" '{}' >/dev/null
wait_for "$SCOUT" "e['type']=='prompt_queued' and e.get('automation')=='Morning report'"
for _ in $(seq 100); do [ "$(api /api/automations | json 'd[0]["last_status"]')" = "done" ] && break; sleep 0.2; done
[ "$(api /api/automations | json 'd[0]["last_status"]')" = "done" ] || { api /api/automations; fail "automation run did not finish"; }
[ "$(api /api/automations | json 'len(d[0]["history"])')" = "1" ] || fail "run not in history"
RUN=$(api /api/automations | json 'd[0]["history"][0]["id"]')
api "/api/automations/$AUTO/history/delete" "{\"run\":\"$RUN\"}" >/dev/null
[ "$(api /api/automations | json 'len(d[0]["history"])')" = "0" ] || fail "run not deleted from history"

step "Update handoff: a newer service takes over and keeps the bots"
OLD=$(python3 -c "import json; print(json.load(open('$FLEET51_HOME/daemon.json'))['pid'])")
"$BIN" daemon --listen "127.0.0.1:$PORT" > "$WORK/daemon2.log" 2>&1 &
for _ in $(seq 50); do kill -0 "$OLD" 2>/dev/null || break; sleep 0.2; done
kill -0 "$OLD" 2>/dev/null && fail "old service still running"
for _ in $(seq 50); do api /api/health >/dev/null 2>&1 && break; sleep 0.2; done
[ "$(api /api/bots | json 'len(d)')" = "2" ] || fail "bots lost in handoff"
[ "$(api /api/automations | json 'len(d)')" = "1" ] || fail "automations lost in handoff"
LAST=$(api "/api/bots/$SCOUT/messages" | json "d[-1]['seq']")
api "/api/bots/$SCOUT/messages" '{"text":"still there?"}' >/dev/null
wait_for "$SCOUT" "e['type']=='text' and e['text'].startswith('Hello') and e['seq']>$LAST"

step "Delete a bot"
api "/api/bots/$FLEET/delete" '{}' >/dev/null
[ "$(api /api/bots | json 'len(d)')" = "1" ] || fail "bot not deleted"
api "/api/bots/$SCOUT/delete" '{}' >/dev/null
[ "$(api /api/automations | json 'len(d)')" = "0" ] || fail "a deleted bot's automations remain"

echo
echo "smoke test passed"
