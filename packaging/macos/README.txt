Fleet-51 for macOS
==================

1. Drag Fleet-51 into Applications.

2. First launch: this build is not signed by Apple, so macOS will block a
   double-click. Either right-click Fleet-51 > Open > Open, or run once in Terminal:

     xattr -dr com.apple.quarantine /Applications/Fleet-51.app

3. Open Fleet-51. It opens in its own window and starts the background daemon
   that runs your agents. Closing the window leaves agents running; use
   Fleet-51 > Quit and Stop Agents (Option-Command-Q) to stop everything.
   View > Open in Browser shows the same UI in a browser tab, e.g. to use it
   from another device.

First launch sets everything up: it downloads the model engine (Ollama, ~175 MB,
checked against a SHA-256 built into Fleet-51) and a first model (qwen3:1.7b,
~1.4 GB), then opens your first bot. If you already run Ollama, that is used.
Get more free models from the model menu in a chat, or the Models button.

Command line (optional):

  sudo ln -sf /Applications/Fleet-51.app/Contents/Resources/fleet51 /usr/local/bin/fleet51
  fleet51 run ollama --model qwen3:1.7b --repo ~/code/my-app -p "fix the failing test" --wait

Stop the daemon:  fleet51 down   (or Fleet-51 > Quit and Stop Agents)
Data and logs:    ~/.fleet51
