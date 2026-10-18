#!/bin/bash
# Fleet-51.app entry point: start the daemon if needed, then open the web UI in the browser.
set -u
BIN="$(cd "$(dirname "$0")/../Resources" && pwd)/fleet51"
# Apps launched from Finder get a minimal PATH; add the usual places agents and Ollama live.
export PATH="/opt/homebrew/bin:/usr/local/bin:$HOME/.local/bin:$HOME/.cargo/bin:$PATH"

if ! "$BIN" up >/dev/null 2>&1; then
  osascript -e 'display alert "Fleet-51 could not start" message "See ~/.fleet51/daemon.log for details."' >/dev/null
  exit 1
fi
open "$("$BIN" url)"
