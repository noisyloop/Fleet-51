# Security review

A review of Fleet-51 as a local Ollama wrapper that lets bots read and write files in their own
folder and, when switched on, run code. Fleet-51 is a prototype: good for a demo, and this review
is about what it would take to rely on it. Scope: the daemon's HTTP surface, the file tools, the
code sandbox on each platform, and untrusted input reaching a bot.

Reviewed at version 0.5.7.

## Summary

| Area | State |
|---|---|
| HTTP API (localhost, token, CSP, host guard) | Solid |
| File tools confined to the bot's folder (`cap-std`, symlink-safe) | Solid |
| Undo / checkpoints confined to the bot's folder | Solid |
| **Linux code sandbox (bubblewrap)** | **Hardened in this change** |
| macOS code sandbox (`sandbox-exec`) | Allow-by-default; residual escape risk — see below |
| Windows "Run code" | No sandbox by design; off until the user accepts a warning |
| Prompt injection via added files / automations | Inherent agent risk — see below |

## What's done well

- **Localhost only, with a token.** Every `/api` route needs the daemon's bearer token, kept in a
  `0600` file. A host-header guard rejects any `Host` other than `localhost`/`127.0.0.1`, so a web
  page can't reach the service by DNS rebinding. Responses carry a strict CSP, `X-Frame-Options:
  DENY`, and `nosniff`.
- **File tools can't leave the folder.** Paths are resolved through the deepest existing component
  and re-checked against the folder, and all reads/writes go through a `cap-std` confined handle,
  so a symlink swapped in at the last moment can't redirect a write outside the folder. Undo uses
  the same confined handle and refuses to restore anything that isn't a bot folder.
- **Code is off by default** and only offered where a working sandbox exists (or on Windows after
  an explicit warning). The sandbox has no network, a clean environment, and a 60-second limit,
  and the process group is killed on timeout so background processes don't linger.

## Findings and fixes

### Fixed in this change — Linux sandbox hardening (`src/sandbox.rs`)

The Linux sandbox binds the whole disk read-only (`--ro-bind / /`) so tools like `python` can run.
That is reasonable, but it previously left three gaps, each confirmed on this machine with plain
commands (no exploit needed):

1. **The whole disk was readable.** Only the current user's home folder was hidden. Other users'
   home folders (`/home/*`, `/root`), and mounted or removable drives (`/mnt`, `/media`, `/srv`,
   `/run/media`) were all readable by a bot's code.
   *Fix:* those trees are now hidden behind empty tmpfs mounts; system folders (`/usr`, `/etc`, …)
   stay readable because programs need them. Removable drives under `/run/media` are already hidden
   because `/run` is tmpfs.

2. **Nested user namespaces could be created** inside the sandbox, widening the kernel surface
   reachable from sandboxed code.
   *Fix:* `--unshare-user --disable-userns` (the sandbox is refused on a kernel too old to support
   this, rather than silently running without it).

3. **No process limit** — a fork bomb or a disk-filling loop could stress the machine for the full
   60 seconds.
   *Fix:* the sandboxed shell now sets `ulimit -u 1024` and `ulimit -f` (single-file size);
   generous for ordinary scripts, bounded against a runaway.

The sandbox self-test now also asserts no files are readable under `/home` and that nested user
namespaces are refused.

### Residual: macOS sandbox is allow-by-default (not fixed here)

`macos_profile` starts from `(allow default)` and denies a specific list (network, writes outside
the folder, reads of the home folder, Apple Events, a set of mach services, `open`/`osascript`,
signals). Anything not on the deny list is allowed. This is the right instinct but the wrong
default for confinement:

- Only the home folder is denied for reads; `/Volumes`, `/Users/Shared`, and per-user temp/cache
  under `/private/var/folders` remain readable.
- The `process-exec` denies target exact paths, so a *copy* of `open`/`osascript` is not covered.
- Reaching `launchd` to start work outside the sandbox is not clearly prevented by this profile.

These were **not** changed in this review: a safe rewrite to deny-by-default needs a Mac to test
against (it is easy to break legitimate code). Treat the macOS "Run code" sandbox as
defense-in-depth, not a boundary you'd hand an untrusted model, until that rewrite lands. Tracked
under "Coming later" alongside the Windows sandbox.

### Residual: Windows "Run code" has no sandbox (by design)

On Windows, switched-on code runs as the user's account with full rights and network access; only
the time limit applies. This is clearly disclosed in the UI and README and is off until the user
accepts the warning. Documenting here so it isn't mistaken for a confinement boundary. The same
applies to PDF/Word text extraction on Windows, which runs the parser unsandboxed.

### Low: access token in the URL

The daemon prints and the app opens `…/?token=<token>`, and the token is also accepted as a query
parameter. On localhost behind the host guard this is low risk, but tokens in URLs can leak into
browser history and logs. Prefer the `Authorization: Bearer` header (already supported) and avoid
the query-parameter fallback where the native app can inject the header instead.

### Inherent: prompt injection via added files and automations

Text in a file a user adds (a PDF, a Word doc, a web page saved as text) is given to the model and
can contain instructions. With "Run code" on, a crafted document could get the bot to run commands
— confined to the sandbox, but still acting on its own. Automations make this unattended. This is
inherent to tool-using agents and can't be fully "fixed"; the mitigations that matter are the ones
above (strong sandbox, code off by default, no network in the sandbox) plus keeping the human in
the loop for anything consequential.

## Linux support

Fleet-51 builds and runs fully on Linux today: the daemon finds or starts Ollama, serves the chat
to any browser (`fleet51 up` then open `fleet51 url`), and — with bubblewrap installed — runs bot
code in the hardened sandbox above. There is no native desktop window on Linux yet (use the
browser); a packaged build is still "Coming later".
