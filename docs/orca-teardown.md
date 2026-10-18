# Orca teardown, and how Fleet-51 beats it

Source: `stablyai/orca` at HEAD on 2026-10-18 (v1.4.214, MIT). Evidence comes from the code
itself plus its public issue tracker. Issue numbers below refer to `stablyai/orca`.

## 1. What Orca is

An Electron desktop app that runs CLI coding agents (Claude Code, Codex, OpenCode, 40+ others)
side by side, each in its own git worktree, with:

- terminal splits (xterm.js + WebGL) backed by a persistent terminal daemon
- a Monaco editor, file explorer, diff viewer with line annotations sent back to agents
- an embedded Chromium "design mode" (click an element, send HTML/CSS/screenshot to the agent)
- GitHub / GitLab / Linear / Jira task sources, PR review
- SSH and WSL remote worktrees, ephemeral VMs
- an `orca` CLI so agents can drive Orca (create worktrees, click, fill, snapshot)
- iOS/Android companion app via a cloud relay
- account switching and Claude/Codex usage tracking, PostHog telemetry

### Architecture as built

| Layer | What it is |
|---|---|
| `src/main` | Electron main process, ~963k lines. Owns PTYs, git, hooks, SSH, accounts, automations. |
| `src/renderer` | React 19 + Zustand + shadcn UI, ~983k lines, 675 top-level component entries. |
| `src/shared` | ~229k lines of types and protocol shared across processes. |
| Terminal daemon | Separate process (`orcad`) that keeps PTYs alive across app restarts and updates. |
| Agent status | Orca **writes hook entries into each agent's global config** (`~/.claude/settings.json`, `~/.codex`, `~/.qwen`, …) so agents call back into a local hook server. Falls back to scraping the terminal screen ("tui-idle"). |
| Prompt delivery | Mostly **types the prompt into the agent's TUI** via the PTY, then tries to verify it landed. |
| `mobile/`, `cloud/` | Expo app plus a relay service for pairing. |
| `native/` | Per-OS helpers (computer use, Windows registry/security, notification status). |

## 2. Scale and health

| Metric | Value |
|---|---|
| Non-test TypeScript | **~2.2 million lines** |
| Test files | ~12,150 |
| Files under `src/` | ~29,900 |
| Open issues | **4,221** (1,478 labelled bug) |
| `pnpm lint` | chains ~25 custom "ratchet" and verify scripts |
| `docs/audits/` | 46 memory "retention" audits (leaks fixed one at a time) |
| Supported agents | 45 bespoke harness integrations |

This is the central weakness. The product ships daily and covers a lot of ground, but the code
base is far past the size any team can reason about. Each agent, OS and transport adds special
cases, and the issue tracker shows the result: the same kinds of bug come back across features.

## 3. What is wrong with it, by theme

### 3.1 Agent control is screen-scraping and keystroke injection

Orca mainly drives agents by typing into their TUIs and guessing state from the screen and
from hooks it installs. That is the root of a whole bug family:

- #22580 initial prompt truncated or never submitted for Crush and OpenCode (typed before the TUI was ready)
- #9976 slow Claude startup gives a false "idle" reading and loses the dispatched prompt
- #10939 a skill command is sent again when an agent resumes
- #14228 a cleanly exited Claude session is auto-resumed with `claude --resume` after restart
- #18399 orchestration routinely struggles with Grok agents
- `docs/bug-reproductions/delayed-agent-launch.md`: `sleep 30 && claude` broke readiness detection for all 45 harnesses

### 3.2 It edits your global agent config

Hooks are installed by rewriting user-level files such as `~/.claude/settings.json`
(`src/main/claude/hook-settings.ts`). That is invasive. It changes how agents behave outside
Orca, conflicts with users' own hooks, and leaves state behind after uninstall.

### 3.3 Process and daemon lifecycle

- #9138 app updates leave older daemon generations running forever; hidden sessions use up memory
- #16797 on Windows the terminal daemon exits silently while the window is in the background
- #11363 Windows UI freezes with about 5 terminals open
- #7951 terminal UI breaks on switching; #18464 blank terminal pane on a remote client
- #7725 Windows upgrade staging directories grow to several GB
- #23383 the Windows CLI launcher gets quarantined by enterprise EDR

### 3.4 The workspace model is one repo and one worktree

Top requests by 👍:

- #1082 Jujutsu workspaces (the #1 request)
- #1099, #21118, #7568, #8886 multi-repo workspaces and cross-repo grouping
- #2654 workspaces that are not tied to a new worktree
- #6357 (most-commented bug) a worktree in a monorepo can't see files or git changes

### 3.5 Half an IDE

It ships Monaco without language intelligence, and users keep asking for the rest of an IDE:

- #961, #3035, #14872 LSP, go to references, diagnostics
- #9948 git blame; #9699 Vim/Emacs keybindings; #6308 editor theme; #7754 "just embed VS Code"

### 3.6 Remote and mobile are bolted on

- #4280 first-class headless or server mode (top-15 request)
- #11714 mobile pairing over the relay fails (`relay_control_not_active`)
- #5049 Android sessions look alive but ignore input (`mobile/issue-5049-…md`)
- #15067 Claude sessions don't sync to mobile

### 3.7 Agents and accounts are hard-coded

- #2284 users can't add their own agent presets, only override built-in commands
- #14323, #12200 people keep asking for one more agent to be added
- #11557, #20512, #12098 multiple subscriptions, failover on rate limits, account switching bugs

### 3.8 No isolation beyond worktrees

- #13665 Docker sandboxes for agents
- #13081 intermediate permission modes (Claude "auto", Codex "approve for me")

### 3.9 Smaller points

- Telemetry is **on by default** (PostHog, opt-out).
- #6074 no multiple windows; #8377 no side-by-side multi-worktree view; #3099 chat-style UI requested.
- Usage stats ignore non-git folders (#20477).

## 4. Fleet-51: the product thesis

> **A small, headless agent fleet manager. One daemon owns every session as an event log;
> any client (terminal, browser, phone, desktop) is a view onto it.**

Design principles, each answering a section above:

1. **Structured protocols first, PTY last.** Talk to agents through real protocols: ACP (Agent
   Client Protocol), Claude Code `--output-format stream-json` / Agent SDK, Codex `app-server` /
   `exec --json`, OpenCode server API. A PTY is the fallback for agents with no protocol, and then
   readiness is an explicit, per-agent declared handshake (title/OSC marker or prompt regex
   captured from a transcript), never a timer. *(Fixes 3.1.)*
2. **Never touch global config.** Hooks and settings are injected per session: `--settings`
   overlays, `CLAUDE_CONFIG_DIR`/`CODEX_HOME` pointing at a session-scoped directory that layers
   on the user's own config, env vars. Uninstalling Fleet-51 leaves no trace. *(Fixes 3.2.)*
3. **Event-sourced sessions.** Every session is an append-only log (spawned, prompt sent, prompt
   acknowledged, tool call, permission requested, turn done, exited with code). Status, resume
   and replay are pure functions of the log. "Was this exited cleanly?" has exactly one answer,
   and a prompt is never re-sent unless the log says it was never acknowledged. *(Fixes 3.1, #14228, #10939.)*
4. **Headless core, thin clients.** `fleet51` is one static binary (its `daemon` subcommand is the server) with one supervisor. It runs
   on a laptop, a dev box or a VM identically. Clients: `fleet` CLI/TUI, a web UI served by the
   daemon (an installable PWA covers mobile), an optional desktop shell. Remote is the same code
   path as local. *(Fixes 3.3, 3.6.)*
5. **Crash-only lifecycle.** One daemon per user, a lock file with generation numbers, and a
   handoff on upgrade: the new daemon adopts live PTYs/sockets and the old one exits. No orphan
   generations. Terminals render only when visible, with output buffered server-side. *(Fixes 3.3.)*
6. **Workspace = a set of checkouts.** A workspace holds one or more checkouts, and each checkout
   is a git worktree, a jj workspace, a plain folder, or a path inside a container/VM. Multi-repo
   and monorepo subpaths are first class. *(Fixes 3.4.)*
7. **Review, don't re-implement the IDE.** Fleet-51 owns what IDEs lack: comparing N agent attempts,
   a unified cross-repo diff, line annotations that go back to the agent as structured feedback,
   and merging the winner. For editing, it opens the user's own editor (VS Code, Cursor, Zed,
   JetBrains, `$EDITOR`) at the right checkout and line, locally or over SSH. *(Fixes 3.5.)*
8. **Agents are data.** Each agent is a declarative manifest (`agents/*.toml`): binary, protocol,
   flags for prompt/model/permission mode/resume, readiness marker, config-overlay recipe, usage
   source. Built-ins ship as manifests; users add their own without a release. *(Fixes 3.7.)*
9. **Isolation tiers.** worktree → container (Docker/Podman, network policy, mounted checkout) →
   remote host/VM. Permission mode is a per-session setting mapped through each manifest. *(Fixes 3.8.)*
10. **Accounts and budgets.** Multiple accounts per provider, each with an isolated config dir.
    Usage is read per session from the event log. Optional failover to another account on a rate
    limit. *(Fixes 3.7.)*
11. **Privacy default.** No telemetry unless the user opts in.
12. **Size discipline.** A hard budget (for example 60k lines for the core at 1.0), with per-agent
    logic confined to manifests and small adapters. Orca shows what happens without one.

## 5. Proposed architecture

```
 ┌──────────── clients ─────────────┐
 │ fleet51 CLI │ Web UI/PWA │ Desktop shell (optional) │
 └───────┬───────────────┬──────────┘
         │  JSON-RPC + event stream over WebSocket / unix socket
 ┌───────▼─────────────────────────────────────────────┐
 │ fleet51 daemon                                       │
 │  ├─ Session supervisor (event log, SQLite + files)   │
 │  ├─ Agent adapters: ACP │ Claude stream-json │ Codex │
 │  │                 app-server │ PTY+handshake        │
 │  ├─ Workspace manager: git │ jj │ folder │ container │
 │  ├─ Review engine: diffs, annotations, compare/merge │
 │  ├─ Accounts & usage                                 │
 │  └─ Auth: local socket, tokens for remote clients    │
 └──────────────────────────────────────────────────────┘
```

Suggested stack: a **Rust** daemon (tokio, `portable-pty`, SQLite, axum) because it gives a
single static binary per platform, low idle memory, and no Electron. The web UI is TypeScript
(React or Solid) embedded in the binary. A desktop shell, if wanted, is Tauri around the same
web UI. A TypeScript/Bun daemon would be faster to start but gives up the binary and memory
advantages that Orca's lifecycle bugs point to.

## 6. Roadmap

| Phase | Scope | Proves |
|---|---|---|
| **0. Spike** | `fleet51` daemon + CLI. Launch Claude Code via stream-json and Codex via `exec --json` in git worktrees. Event log, `fleet51 ls/run/log`. | Protocol-first control works with no prompt-injection races. |
| **1. MVP** | Web UI: session list with live status, terminal/chat view, diff + annotate → send feedback, fan-out one prompt to N agents and compare. Agent manifests. PTY fallback with handshake. | The core loop beats Orca on reliability. |
| **2. Remote** | Same daemon on a remote host, token auth, PWA for phone, notifications. | Headless/server mode (#4280) and mobile without a relay. |
| **3. Breadth** | jj and multi-repo workspaces, container isolation, accounts and failover, GitHub/GitLab/Linear task sources, open-in-editor. | The top-requested items Orca hasn't shipped. |

## 7. Where Orca is still ahead (don't underestimate)

- Breadth: 45 agents, SSH/WSL, computer use, design mode, mobile apps, localization.
- Polish and velocity: it ships daily and has a large user base.
- Deep platform hardening on Windows and macOS (EDR, TCC, ConPTY job objects).

Fleet-51 should not chase feature parity. It should win on reliability of the core loop
(launch → prompt → status → review → merge), on running headless anywhere, and on staying small.

## 8. Implementation status (rough demos, all in this repo)

Every principle above has a working first version. They are exercised end-to-end by
`scripts/demo.sh` with offline mock agents that speak each real protocol.

| Principle | What exists now | Where |
|---|---|---|
| 1. Structured protocols first | Adapters for Claude stream-json (with `--replay-user-messages` acks), Codex `exec --json`, and ACP. Terminal fallback holds prompts until a declared readiness regex matches. | `src/adapters/` |
| 2. Never touch global config | Hooks and settings are written to `~/.fleet51/sessions/<id>/overlay/` and passed with `--settings`. Agent hooks call `fleet51 hook`. | `agents/claude.toml`, `src/session.rs` |
| 3. Event-sourced sessions | Append-only `events.jsonl` per session; status, pending prompts and the interrupted turn are folds over it. A clean exit is never auto-resumed. Unacknowledged prompts are re-sent, and an interrupted turn is re-run exactly once. | `src/events.rs` |
| 4. Headless core, thin clients | One binary: daemon, CLI, and an embedded web UI (installable PWA, phone layout, notifications). Token auth; `--listen 0.0.0.0:7451` for remote use. One multiplexed event stream per client. | `src/server.rs`, `ui/` |
| 5. Crash-only lifecycle | Generation-numbered `daemon.json`. A new daemon asks the old one to suspend and exit, then resumes its sessions. After a crash, leftover agent processes are matched by `FLEET51_SESSION` and reaped. | `src/daemon.rs` |
| 6. Workspace = set of checkouts | git worktrees, jj workspaces, plain folders (diffed through a shadow git dir), several repos at once, and monorepo subfolders. | `src/workspace.rs` |
| 7. Review, not an IDE | Cross-repo diff, line annotations turned into one feedback prompt, fan-out comparison, merge of the winner, `fleet51 open` in your own editor. | `src/review.rs` |
| 8. Agents are data | TOML manifests; drop one in `~/.fleet51/agents/` to add or override an agent. | `agents/`, `src/manifest.rs` |
| 9. Isolation tiers | `none`, `docker` (host network), and `docker-offline`, with checkouts mounted at the same paths. | `src/sandbox.rs` |
| 10. Accounts and budgets | Isolated config dir per account, `--account auto`, cooldown and failover on rate limits, usage by agent and account. | `src/accounts.rs` |
| 11. Privacy default | No telemetry code at all. | |
| 12. Size discipline | About 3.7k lines of Rust plus 0.6k lines of UI (Orca: about 2.2 million lines). | |

Known gaps in the demo:

- Only the mock agents are tested end to end. The real Claude, Codex, Gemini and OpenCode
  manifests follow their documented flags but have not been run against those CLIs here.
- A daemon handoff restarts agents through their resume flags instead of adopting the live
  processes. A per-session shim that outlives the daemon would remove even that restart.
- Container tiers need the agent binary inside the image (`sandbox_image` in the manifest).
- The PID liveness and reaping checks use `/proc`, so they are Linux-only for now.
- Merge is implemented for git checkouts only.
