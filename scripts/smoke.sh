#!/usr/bin/env bash
# End-to-end check of Fleet-51 against a stand-in Ollama: first launch creates a bot, bots
# chat (with reasoning), use different models, write files only inside their folder, and
# survive an update handoff. Needs bash, curl and Python 3. Nothing outside a temp dir is touched.
# Runs on Linux, macOS and Windows (Git Bash).
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
WORK="$(mktemp -d "${TMPDIR:-/tmp}/fleet51-smoke.XXXXXX")"
PY=python3
WINDOWS=""
if command -v cygpath >/dev/null 2>&1; then
  # Windows: paths both bash and Windows programs understand (C:/Users/...).
  WINDOWS=1
  WORK="$(cygpath -m "$WORK")"
  ROOT="$(cygpath -m "$ROOT")"
  PY=python
fi
# Is this (Windows or Unix) process id still running?
alive() {
  if [ -n "$WINDOWS" ]; then tasklist //FI "PID eq $1" //NH 2>/dev/null | grep -q " $1 "; else kill -0 "$1" 2>/dev/null; fi
}
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
json() { "$PY" -c "import json,sys; d=json.load(sys.stdin); print($1)"; }
# Wait until the bot's messages contain an event matching a Python expression over `e`.
wait_for() {
  for _ in $(seq 100); do
    if api "/api/bots/$1/messages" | "$PY" -c "import json,sys; sys.exit(0 if any($2 for e in json.load(sys.stdin)) else 1)"; then return 0; fi
    sleep 0.2
  done
  api "/api/bots/$1/messages"; echo; fail "timed out waiting for: $2"
}

(cd "$ROOT" && cargo build -q)
"$PY" "$ROOT/scripts/fake_ollama.py" 11499 & FAKE_PID=$!
sleep 1

step "First launch: the service starts, finds Ollama and creates the first bot"
"$BIN" up --listen "127.0.0.1:$PORT"
for _ in $(seq 50); do [ "$(api /api/bots | json 'len(d)')" = "1" ] && break; sleep 0.2; done
"$BIN" status
"$BIN" status | grep -q "models: ready (system" || fail "model engine not ready"
FLEET=$(api /api/bots | json 'd[0]["id"]')
[ "$(api /api/models | json "[m['cloud'] for m in d['installed'] if m['name'].endswith('-cloud')][0]")" = "True" ] || fail "cloud model not marked"
[ "$(api /api/models | json "[m['cloud'] for m in d['installed'] if m['name']=='qwen3:1.7b'][0]")" = "False" ] || fail "local model marked as cloud"
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

step "Adding files: copied into the bot's folder, documents read in the sandbox, images seen"
"$PY" "$ROOT/scripts/make_test_files.py" "$WORK/in"
upload() { curl -sS -H "Authorization: Bearer $(cat "$FLEET51_HOME/token")" --data-binary "@$1" "http://127.0.0.1:$PORT/api/bots/$SCOUT/files?name=$2"; }
upload "$WORK/in/report.pdf" report.pdf | grep -q '"text_file":"report.pdf.txt"' || fail "PDF text not read"
grep -q "Fleet-51 test document" "$FLEET51_BOTS_DIR/Scout/report.pdf.txt" || fail "wrong PDF text"
upload "$WORK/in/letter.docx" letter.docx >/dev/null
grep -q "Dear Fleet & crew" "$FLEET51_BOTS_DIR/Scout/letter.docx.txt" || fail "Word text not read"
# A name that tries to leave the folder is cleaned; nothing lands outside.
upload "$WORK/in/alien.png" "..%2F..%2Fescaped-upload.png" | grep -q '"name":"escaped-upload.png"' || fail "upload name not cleaned"
[ ! -e "$FLEET51_BOTS_DIR/escaped-upload.png" ] && [ -f "$FLEET51_BOTS_DIR/Scout/escaped-upload.png" ] || fail "upload landed outside the folder"
# Adding the same name again never overwrites.
upload "$WORK/in/alien.png" alien.png >/dev/null; upload "$WORK/in/alien.png" alien.png | grep -q '"name":"alien (2).png"' || fail "upload overwrote a file"
api "/api/bots/$SCOUT/messages" '{"text":"what is this?","attachments":["../../etc/passwd"]}' | grep -q "not in" || fail "a file outside the folder was attached"
LAST=$(api "/api/bots/$SCOUT/messages" | json "d[-1]['seq']")
api "/api/bots/$SCOUT/model" '{"model":"gemma4:e4b"}' >/dev/null
api "/api/bots/$SCOUT/messages" '{"text":"what is in the picture and the report?","attachments":["alien.png","report.pdf"]}' >/dev/null
wait_for "$SCOUT" "e['type']=='prompt_queued' and e.get('attachments')==['alien.png','report.pdf']"
wait_for "$SCOUT" "e['type']=='text' and 'I can see 1 image' in e['text'] and 'report.pdf.txt' in e['text'] and e['seq']>$LAST"

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
# The bot gets the instructions as a standalone task (system prompt + task), not the whole chat.
api /api/automations | json 'd[0]["last_result"]' | grep -q "with 2 messages" || { api /api/automations; fail "automation was not run as a standalone task"; }
RUN=$(api /api/automations | json 'd[0]["history"][0]["id"]')
api "/api/automations/$AUTO/history/delete" "{\"run\":\"$RUN\"}" >/dev/null
[ "$(api /api/automations | json 'len(d[0]["history"])')" = "0" ] || fail "run not deleted from history"

step "Update handoff: a newer service takes over and keeps the bots"
OLD=$("$PY" -c "import json; print(json.load(open('$FLEET51_HOME/daemon.json'))['pid'])")
"$BIN" daemon --listen "127.0.0.1:$PORT" > "$WORK/daemon2.log" 2>&1 &
for _ in $(seq 50); do alive "$OLD" || break; sleep 0.2; done
alive "$OLD" && fail "old service still running"
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
