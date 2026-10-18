#!/usr/bin/env bash
# End-to-end demo of Fleet-51 with offline mock agents. No API keys, nothing outside a temp dir.
#   scripts/demo.sh            run the demo
#   DEMO_DOCKER=1 scripts/demo.sh   also run a session in a Docker sandbox (needs a running dockerd)
#   KEEP=1 scripts/demo.sh     leave the daemon running afterwards so you can open the web UI
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
PORT="${PORT:-7471}"
WORK="$(mktemp -d "${TMPDIR:-/tmp}/fleet51-demo.XXXXXX")"
export FLEET51_HOME="$WORK/home"
export PATH="$ROOT/target/debug:$PATH"
# The demo uses mock agents only; don't download or start Ollama.
export FLEET51_NO_RUNTIME=1

step() { printf '\n\033[1m== %s\033[0m\n' "$*"; }
sid() { fleet51 ls | awk -v t="$1" 'index($0, t) { print $1; exit }'; }
daemon_pid() { sed -n 's/.*"pid": *\([0-9]*\).*/\1/p' "$FLEET51_HOME/daemon.json"; }
cleanup() {
  if [[ "${KEEP:-}" != 1 ]]; then
    fleet51 down >/dev/null 2>&1 || true
    sleep 1
    rm -rf "$WORK"
  fi
}
trap cleanup EXIT

step "Build"
(cd "$ROOT" && cargo build -q)

step "Demo repos in $WORK"
cd "$WORK"
git init -q app
git -C app -c user.email=demo@fleet51 -c user.name=demo commit -q --allow-empty -m init
mkdir -p app/packages/web && echo web > app/packages/web/index.txt
git -C app add -A && git -C app -c user.email=demo@fleet51 -c user.name=demo commit -q -m "add web package"
git init -q api && git -C api -c user.email=demo@fleet51 -c user.name=demo commit -q --allow-empty -m init
mkdir notes && echo "plain folder, no git" > notes/readme.txt

step "Start the daemon (account a1 will report a rate limit)"
FLEET51_MOCK_RATE_LIMIT=a1 fleet51 daemon --listen "127.0.0.1:$PORT" >"$WORK/daemon.log" 2>&1 &
for _ in $(seq 50); do fleet51 status >/dev/null 2>&1 && break; sleep 0.1; done
fleet51 status
fleet51 agents

step "1. Structured protocols: Claude stream-json, Codex exec-json, ACP"
fleet51 run mock-claude --repo app -p "add a greeting" --wait
fleet51 run mock-codex --repo app -p "write release notes" --wait
fleet51 run mock-acp --repo app -p "document the api" --wait

step "1b. Terminal-only agent with a slow start: the prompt waits for the readiness marker"
fleet51 run mock-pty --repo app -p "typed only once the agent is ready" --wait

step "2. Review: diff, comment on a line, send it back as feedback, merge"
S=$(sid "add a greeting")
fleet51 diff "$S"
fleet51 annotate "$S" mock/add-a-greeting.md:3 "make it friendlier"
fleet51 feedback "$S"
sleep 2
fleet51 log "$S" | tail -5
fleet51 merge "$S"
git -C app log --oneline -3

step "3. Fan out one prompt to three agents and compare the attempts"
G=$(fleet51 fanout --agents mock-claude,mock-codex,mock-acp --repo app -p "tidy the docs" | awk '/^group/ { print $2 }')
sleep 4
fleet51 compare "$G"

step "4. Multi-repo workspace, starting from a monorepo subfolder"
fleet51 ws create fullstack app/packages/web api
W=$(fleet51 ws ls | grep -o 'w-[0-9a-f]*' | tail -1)
fleet51 run mock-claude --ws "$W" -p "connect web to api" --wait >/dev/null
fleet51 diff "$(sid "connect web to api")" | grep -E '^━━|^  mock'

step "5. Plain folder (no git): still diffable through a shadow snapshot"
fleet51 run mock-claude --repo notes -p "summarise the notes" --wait >/dev/null
fleet51 diff "$(sid "summarise the notes")" | grep -E '^━━|^  mock'

step "6. Accounts: a1 hits a rate limit, the session fails over to a2 and re-runs the turn once"
fleet51 accounts add mock a1 >/dev/null
fleet51 accounts add mock a2 >/dev/null
fleet51 run mock-claude --repo app --account auto -p "long task" --wait >/dev/null
sleep 2
fleet51 log "$(sid "long task")" | grep -E 'rate limited|switched|turn done|re-running'
fleet51 accounts ls

step "7. Agent crashes mid-turn; an explicit resume re-runs only the unfinished turn"
fleet51 run mock-claude --repo app -p "crash-once now" --wait | tail -2 || true
C=$(sid "crash-once now")
fleet51 resume "$C" >/dev/null
sleep 3
fleet51 log "$C" | tail -4

step "8. Upgrade handoff: a new daemon generation takes over and resumes every session"
OLD=$(daemon_pid)
FLEET51_MOCK_RATE_LIMIT=a1 fleet51 daemon --listen "127.0.0.1:$PORT" >"$WORK/daemon2.log" 2>&1 &
sleep 4
head -3 "$WORK/daemon2.log"
if kill -0 "$OLD" 2>/dev/null; then echo "old daemon still running (unexpected)"; else echo "old daemon $OLD exited"; fi

step "9. Daemon killed with SIGKILL mid-turn: the next start reaps, resumes and re-runs the turn"
P=$(sid "typed only once")
fleet51 send "$P" "second prompt" >/dev/null
sleep 0.3
kill -9 "$(daemon_pid)"
sleep 1
FLEET51_MOCK_RATE_LIMIT=a1 fleet51 daemon --listen "127.0.0.1:$PORT" >"$WORK/daemon3.log" 2>&1 &
sleep 6
fleet51 log "$P" | tail -9

if [[ "${DEMO_DOCKER:-}" == 1 ]]; then
  step "10. Docker sandbox"
  fleet51 run mock-claude --repo app --sandbox docker -p "work in a container" --wait
fi

step "Usage (from the event logs; non-git folders included)"
fleet51 usage

step "Your global agent config was not touched"
echo "Fleet-51 wrote only under $FLEET51_HOME (per-session overlays live in sessions/*/overlay)"
if [[ "${KEEP:-}" == 1 ]]; then
  echo
  echo "Daemon left running. Web UI: $(fleet51 url)"
  echo "Stop it with: FLEET51_HOME=$FLEET51_HOME fleet51 down"
fi
