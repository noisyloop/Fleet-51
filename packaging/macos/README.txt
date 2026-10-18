Fleet-51 for macOS
==================

1. Drag Fleet-51 into Applications.

2. First launch: this build is not signed by Apple, so macOS will block a
   double-click. Either right-click Fleet-51 > Open > Open, or run once in Terminal:

     xattr -dr com.apple.quarantine /Applications/Fleet-51.app

3. Open Fleet-51. It starts the Fleet-51 daemon in the background and opens the
   web UI in your browser. Opening it again just reopens the web UI.

Local models (free): install Ollama from https://ollama.com, then use the Models
panel in the web UI to download one, e.g. qwen3:1.7b or tinydolphin.

Command line (optional):

  sudo ln -sf /Applications/Fleet-51.app/Contents/Resources/fleet51 /usr/local/bin/fleet51
  fleet51 run ollama --model qwen3:1.7b --repo ~/code/my-app -p "fix the failing test" --wait

Stop the daemon:  fleet51 down   (or quit it from Activity Monitor: "fleet51")
Data and logs:    ~/.fleet51
