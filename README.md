# Fleet-51

Run a fleet of coding agents (Claude Code, Codex, any ACP agent, or any terminal agent) from
one small headless daemon. Watch them side by side in a browser or on your phone, review their
diffs, send line comments back as feedback, and merge the winner.

Fleet-51 started as a teardown of [Orca](https://github.com/stablyai/orca). That analysis, and
what Fleet-51 does differently, is in [`docs/orca-teardown.md`](docs/orca-teardown.md). This is
an early, rough demo.

![Six agents working side by side](docs/screenshots/side-by-side.png)

| Compare attempts | Review and comment | Phone |
|---|---|---|
| ![Comparing fan-out attempts](docs/screenshots/compare.png) | ![Reviewing a diff with line comments](docs/screenshots/review.png) | ![Phone layout](docs/screenshots/phone.png) |

## Why it is different

- **Talks to agents through their protocols.** It uses Claude stream-json, Codex `exec --json`
  and ACP rather than typing into a TUI. Each prompt gets an explicit acknowledgement. Terminal-only
  agents get their prompt only after they show a declared readiness marker, never after a timer.
- **Never edits your agent config.** Hooks and settings are injected per session (for example
  `claude --settings <overlay>`). Everything Fleet-51 writes lives in `~/.fleet51`.
- **Every session is an event log.** Status, resume and replay are derived from it. A cleanly
  finished session is never resumed by surprise, and an interrupted turn is re-run exactly once.
- **Headless first.** One binary is the daemon, the CLI and the web UI. Run it on a dev box and
  open it from a laptop or phone.
- **Survives upgrades and crashes.** A new daemon generation takes over from the old one and
  resumes its sessions. After a crash it reaps leftover agent processes.
- **Workspaces are sets of checkouts.** These can be git worktrees, jj workspaces, plain folders
  (still diffable), several repos at once, or a monorepo subfolder.
- **Review instead of re-implementing an IDE.** You get a cross-repo diff, line comments turned
  into feedback, fan-out comparison, merge, and `fleet51 open` in your own editor.
- **Agents are data.** Add any agent with a TOML manifest in `~/.fleet51/agents/`.
- **Free local models.** A built-in coding agent runs on Ollama or any OpenAI-compatible server,
  so a fleet can be the same task on several local models.
- **Isolation tiers** (`none`, `docker`, `docker-offline`), **multiple accounts** with
  failover on rate limits, and **no telemetry**.

## Try it (no API keys needed)

```bash
cargo build
scripts/demo.sh            # full end-to-end run with offline mock agents
KEEP=1 scripts/demo.sh     # same, then leaves the daemon up and prints the web UI link
```

## Run a fleet of local models with Ollama

Ollama serves models; it is not a coding agent. Fleet-51 includes a small coding agent
(`fleet51 agent`) that uses the model through Ollama's API. It can list, read, search, write and
edit files and run commands, all confined to the session's workspace.

```bash
ollama pull qwen3:1.7b && ollama pull qwen3:0.6b
fleet51 up
fleet51 models                                   # what Ollama has locally
fleet51 run ollama --model qwen3:1.7b --repo . -p "fix the failing test" --wait
fleet51 fanout --agents ollama --models qwen3:0.6b,qwen3:1.7b -n 2 --repo . -p "add input validation"
fleet51 compare g-1234abcd                       # which model did best
```

Leave out `--model` to use the first model Ollama has. The `openai-compatible` agent does the same
against LM Studio, vLLM, llama.cpp server or OpenRouter (set `OPENAI_BASE_URL`, plus
`OPENAI_API_KEY` if needed). The model must support tool calling. Small local models are much
weaker than hosted ones, so compare several and review before merging.

## Use it

```bash
export PATH="$PWD/target/debug:$PATH"
fleet51 up                                   # start the daemon (127.0.0.1:7451)
fleet51 url                                  # web UI link with token; open it anywhere
fleet51 agents                               # what is installed

fleet51 run claude --repo ~/code/app -p "fix the flaky test" --wait
fleet51 fanout --agents claude,codex -n 2 --repo ~/code/app -p "add dark mode"
fleet51 compare g-1234abcd                   # files, lines, cost, last message per attempt
fleet51 diff s-1234abcd
fleet51 annotate s-1234abcd src/theme.ts:42 "use the existing token instead"
fleet51 feedback s-1234abcd                  # all unsent comments become one prompt
fleet51 merge s-1234abcd                     # commit on the agent's branch, merge into yours

fleet51 ws create fullstack ~/code/web ~/code/api   # multi-repo workspace
fleet51 run claude --ws w-1234abcd -p "wire the web app to the new endpoint"
fleet51 run claude --repo ~/code/app --sandbox docker-offline -p "..."

fleet51 accounts add anthropic work          # then: CLAUDE_CONFIG_DIR=<printed dir> claude
fleet51 run claude --repo . --account auto -p "..."   # fails over on rate limits
fleet51 usage
fleet51 down                                 # suspends sessions; `fleet51 up` resumes them
```

For remote use, run `fleet51 daemon --listen 0.0.0.0:7451` on the host. Then point the CLI at it
with `FLEET51_URL` and `FLEET51_TOKEN`, or open the `fleet51 url` link over a private network
such as Tailscale or WireGuard.

## Adding an agent

```toml
# ~/.fleet51/agents/my-agent.toml
name = "my-agent"
protocol = "acp"            # claude-stream-json | codex-exec-json | acp | pty
command = ["my-agent", "--acp"]
rate_limit_pattern = "(?i)rate limit"

# For terminal-only agents:
# protocol = "pty"
# [pty]
# ready_pattern = "> $"     # prompts wait for this; captured from a real transcript
# idle_pattern = "> $"
```

See `agents/` for the built-in manifests, including permission modes, resume flags,
per-session config overlays, account directories and sandbox images.

## Layout

| Path | What |
|---|---|
| `src/events.rs` | Session event log and the state fold |
| `src/session.rs` | Per-session supervisor: spawn, resume, account failover, config overlay |
| `src/adapters/` | Claude stream-json, Codex exec-json, ACP, PTY |
| `src/daemon.rs` | Generation handoff, crash recovery, session management |
| `src/server.rs` | HTTP API, event streams, embedded UI |
| `src/workspace.rs`, `src/review.rs` | Checkouts, diffs, feedback, merge |
| `src/accounts.rs`, `src/sandbox.rs` | Accounts and isolation tiers |
| `src/mock.rs` | Offline mock agent for every protocol |
| `ui/` | Web UI / PWA (no build step) |

## Status

This is a rough demo. The mock agents run end to end. The Ollama agent has run against real
qwen3 models on CPU, but no real task has been fixed and tested yet: qwen3:1.7b made genuine tool
calls and edits, but its fix was wrong and it was stopped after 10 minutes. The built-in manifests for
real agents follow their documented flags but are untested here. Known gaps are listed at the
end of [`docs/orca-teardown.md`](docs/orca-teardown.md).
