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

Local models (free): install Ollama from https://ollama.com, then use the Models
panel in the web UI to download one, e.g. qwen3:1.7b or tinydolphin.

Command line (optional):

  sudo ln -sf /Applications/Fleet-51.app/Contents/Resources/fleet51 /usr/local/bin/fleet51
  fleet51 run ollama --model qwen3:1.7b --repo ~/code/my-app -p "fix the failing test" --wait

Stop the daemon:  fleet51 down   (or Fleet-51 > Quit and Stop Agents)
Data and logs:    ~/.fleet51
