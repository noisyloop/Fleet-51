Fleet-51 for macOS
==================

Your own fleet of alien AI bots, running privately on this computer.

1. Drag Fleet-51 into Applications.

2. First launch: this build is not signed by Apple, so macOS will block a
   double-click. Either right-click Fleet-51 > Open > Open, or run once in Terminal:

     xattr -dr com.apple.quarantine /Applications/Fleet-51.app

3. Open Fleet-51. The first launch sets everything up: it uses Ollama if you have
   it, otherwise downloads Ollama (~175 MB, checked against a SHA-256 built into
   Fleet-51), downloads a first model (qwen3:1.7b, ~1.4 GB), and opens your first
   bot.

Create more bots with +. Pick each bot's model at the top of its chat, and get
more free models from the Models button. File > New Window (Shift-Command-N) or a
bot's "Open in new window" gives you several chats side by side.

Closing the window leaves bots running. Fleet-51 > Quit and Stop Bots
(Option-Command-Q) stops everything.

Bots' files:  ~/Fleet-51/<bot name>
App data:     ~/.fleet51
