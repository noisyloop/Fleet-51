"""A stand-in for Ollama's HTTP API, for tests: models, downloads and chat replies.

Replies echo the model name and include reasoning unless thinking is turned off. Messages
containing "save a note" make the model write notes.md; "escape" makes it try to write
outside its folder; "break it" overwrites notes.md; "run code" runs a command that also tries to
write outside the folder.
"""
import json
import sys
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

MODELS = {"qwen3:1.7b": 1107409472, "qwen3:0.6b": 396705472, "gemma4:e4b": 9600000000}


class H(BaseHTTPRequestHandler):
    def log_message(self, *a):
        pass

    def body(self):
        n = int(self.headers.get("Content-Length") or 0)
        return json.loads(self.rfile.read(n) or b"{}")

    def send(self, obj, code=200):
        b = json.dumps(obj).encode()
        self.send_response(code)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(b)))
        self.end_headers()
        self.wfile.write(b)

    def do_GET(self):
        if self.path == "/api/version":
            self.send({"version": "0.0.0-fake"})
        elif self.path == "/api/tags":
            models = [{"name": n, "size": s, "details": {"parameter_size": "1B", "quantization_level": "Q4"}} for n, s in MODELS.items()]
            # A cloud model as Ollama lists it: tiny on disk, run on a remote host.
            models.append({"name": "deepseek-v4.1-flash:1017-cloud", "size": 384, "remote_model": "deepseek-v4.1-flash:1017", "remote_host": "https://ollama.com:443", "details": {}})
            self.send({"models": models})
        else:
            self.send({"error": "not found"}, 404)

    def do_POST(self):
        b = self.body()
        if self.path == "/api/show":
            caps = ["completion", "tools", "thinking"] + (["vision"] if b.get("model", "").startswith("gemma4") else [])
            self.send({"capabilities": caps})
        elif self.path == "/api/pull":
            self.send_response(200)
            self.end_headers()
            for st in ["pulling manifest", "success"]:
                self.wfile.write(json.dumps({"status": st}).encode() + b"\n")
            MODELS[b["model"]] = 1
        elif self.path == "/v1/chat/completions":
            model = b["model"]
            if model not in MODELS:
                return self.send({"error": {"message": f"model '{model}' not found"}}, 404)
            last = b["messages"][-1]
            if isinstance(last.get("content"), list):
                images = sum(1 for p in last["content"] if p.get("type") == "image_url" and p["image_url"]["url"].startswith("data:image/png;base64,"))
                text = " ".join(p.get("text", "") for p in last["content"] if p.get("type") == "text")
                self.send({"choices": [{"message": {"role": "assistant", "content": f"I can see {images} image(s). {text[-80:]}"}}], "usage": {"prompt_tokens": 1, "completion_tokens": 1}})
                return
            msg = {"role": "assistant", "content": f"Hello from {model}."}
            if last["role"] == "user" and last["content"].startswith("[Automation"):
                msg = {"role": "assistant", "content": f"Automation ran with {len(b['messages'])} messages."}
            elif last["role"] == "user" and "save a note" in last["content"]:
                msg = {"role": "assistant", "content": "", "tool_calls": [{"id": "c1", "type": "function", "function": {"name": "write_file", "arguments": json.dumps({"path": "notes.md", "content": "hi"})}}]}
            elif last["role"] == "user" and "escape" in last["content"]:
                msg = {"role": "assistant", "content": "", "tool_calls": [{"id": "c2", "type": "function", "function": {"name": "write_file", "arguments": json.dumps({"path": "../escaped.txt", "content": "pwned"})}}]}
            elif last["role"] == "user" and "break it" in last["content"]:
                msg = {"role": "assistant", "content": "", "tool_calls": [{"id": "c3", "type": "function", "function": {"name": "write_file", "arguments": json.dumps({"path": "notes.md", "content": "broken"})}}]}
            elif last["role"] == "user" and "run code" in last["content"]:
                cmd = "echo ran-in-sandbox > out.txt && cat out.txt && echo nope > ../sandbox-escape.txt"
                msg = {"role": "assistant", "content": "", "tool_calls": [{"id": "c4", "type": "function", "function": {"name": "run_command", "arguments": json.dumps({"command": cmd})}}]}
            elif last["role"] == "tool":
                msg = {"role": "assistant", "content": f"Tool said: {last['content']}"}
            if b.get("reasoning_effort") != "none":
                msg["reasoning"] = f"Thinking about it with {model}."
            time.sleep(0.2)
            self.send({"choices": [{"message": msg}], "usage": {"prompt_tokens": 10, "completion_tokens": 5}})
        else:
            self.send({"error": "not found"}, 404)

    def do_DELETE(self):
        MODELS.pop(self.body().get("model"), None)
        self.send({})


ThreadingHTTPServer(("127.0.0.1", int(sys.argv[1])), H).serve_forever()
