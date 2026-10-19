<p align="center"><img src="ui/icon.svg" width="112" alt="Fleet-51 alien logo"></p>

<h1 align="center">Fleet-51</h1>

<p align="center"><b>Your own fleet of alien AI bots, running privately on your computer.</b><br>
Create a bot, give it a name and a face, pick a free model, and chat. Run as many as you like at once.</p>

<p align="center"><a href="https://github.com/noisyloop/Fleet-51/releases/latest/download/Fleet-51-macos.dmg"><b>Download for Mac</b></a> · Apple Silicon and Intel · free and open source</p>

![Chatting with a Fleet-51 bot, its reasoning shown above the answer](docs/screenshots/bots.png)

| Create a bot | On a phone |
|---|---|
| ![Create New Bot: name, alien shape and colour](docs/screenshots/new-bot.png) | ![Bot list on a phone](docs/screenshots/phone-bots.png) |

## What it does

- **Create a bot in seconds:** a name, one of 13 alien shapes and a colour. That's it.
- **Many bots at once:** each bot keeps working while you chat with another. Open a bot in its own
  window (⋯ › Open in new window) to have several conversations side by side.
- **Pick a model per bot:** choose from the menu at the top of the chat, and download more free models
  from **Models**. Bots on different models run at the same time.
- **Automations:** give a bot a job on a schedule: once, hourly, daily, weekdays, weekly, monthly,
  annually, or your own days and times. Name it, write the instructions, and get a notification when
  it's done. The run and its reply land in the bot's chat.
- **Works offline:** after the first setup, bots and automations run with no internet at all. An
  **Offline** badge shows when you're disconnected; only downloading new models needs a connection.
- **See the thinking:** models that reason show it in a collapsible **Reasoning** section. Turn
  **Think** off for faster replies.
- **Private:** everything runs on your computer through [Ollama](https://ollama.com). No account, no
  cloud, no telemetry.
- **Safe by design:** each bot has its own folder (`~/Fleet-51/<bot name>`, private to you) and can
  only read and write files there.
- **Runs its own code, in a sandbox:** a bot can run what it writes to check it works. The code runs
  in the operating system's sandbox (built into macOS; bubblewrap on Linux) with no internet, the
  rest of your home folder hidden, only its own folder writable, and a 60-second limit. Where no
  sandbox is available, bots simply can't run code.
- **Undo:** before a bot changes its files in a reply, Fleet-51 keeps a copy outside the bot's reach.
  **Undo** under that reply puts the folder back exactly as it was.
- **Tidy up:** right-click a bot to clear its chat or delete it. Automation history can be deleted run
  by run or all at once.

## Install on a Mac

1. Download **[Fleet-51-macos.dmg](https://github.com/noisyloop/Fleet-51/releases/latest/download/Fleet-51-macos.dmg)**
   (or pick a version from [Releases](https://github.com/noisyloop/Fleet-51/releases)).
2. Open it and drag **Fleet-51** to Applications.
3. The app isn't signed by Apple yet, so the first time right-click it › **Open** › **Open**, or run:
   ```bash
   xattr -dr com.apple.quarantine /Applications/Fleet-51.app
   ```
4. Open Fleet-51. The first launch sets everything up by itself:
   - it uses Ollama if you already have it; otherwise it downloads Ollama (about 175 MB) and checks it
     against a SHA-256 built into Fleet-51;
   - it downloads a first model, qwen3:1.7b (about 1.4 GB);
   - it opens your first bot, **Fleet**.

Closing the window leaves your bots running in the background. **Fleet-51 › Quit and Stop Bots**
(⌥⌘Q) stops everything. Updating is just installing a newer `.dmg`; the new version takes over from
the old one and keeps your bots and chats.

Windows and Linux packages are planned. On Linux you can build from source (below) with Ollama
installed.

## Build from source

```bash
cargo build --release
target/release/fleet51 up       # start the background service (finds or starts Ollama)
target/release/fleet51 url      # link to open Fleet-51 in a browser
target/release/fleet51 models pull qwen3:4b
scripts/smoke.sh                # end-to-end test against a stand-in Ollama
```

## How it works

| Part | What it does |
|---|---|
| `src/daemon.rs` | Background service: owns the bots, model downloads and the model engine; a newer build takes over from an older one |
| `src/session.rs`, `src/events.rs` | Each bot is an append-only event log plus a supervisor that runs its agent; replies, reasoning and status are read from the log |
| `src/agent.rs` | The bot itself: talks to the Ollama model, with file tools confined to the bot's folder |
| `src/sandbox.rs`, `src/checkpoints.rs` | Running bot code in the OS sandbox; undo copies of bot folders |
| `src/automations.rs` | Schedules, runs and notifications for automations (`~/.fleet51/automations.json`) |
| `src/runtime.rs`, `src/models.rs` | Finds, downloads (checksum-verified) and starts Ollama; lists and downloads models |
| `ui/` | The chat app (no build step), shown in the Mac app's window or any browser |
| `packaging/macos/` | The native Mac app and `.dmg` packaging, built by GitHub Actions |

Fleet-51 began as a teardown of [Orca](https://github.com/stablyai/orca); the analysis is in
[`docs/orca-teardown.md`](docs/orca-teardown.md).

## Automations

**Automations › New** (or a bot's **⋯ › New automation…**) opens the editor: a name, the bot that does
it, a trigger, the instructions, and whether to notify you when it's done. Times are your computer's
local time. Fleet-51 has to be running for an automation to run (closing the window is fine); a run
missed while the computer was asleep or off happens when Fleet-51 is next running, if that's within
12 hours.

## Coming later

Email notifications, Windows and Linux packages, and signed builds.
