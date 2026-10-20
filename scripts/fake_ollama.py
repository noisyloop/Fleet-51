"""A stand-in for Ollama's HTTP API, for tests: models, downloads and chat replies.

Replies echo the model name and include reasoning unless thinking is turned off. Messages
containing "save a note" make the model write notes.md; "escape" makes it try to write
outside its folder; "break it" overwrites notes.md; "run code" runs a command that also tries to
write outside the folder; "context size" reports the context size asked for; "cut off" ends the
reply as if the model ran out of room.
"""
import json
import os
import sys
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

DEMO = os.environ.get("FAKE_DEMO") == "1"
DEMO_REPLIES = [
    ("haiku", "Lights over the dunes,\nthe fleet hums in quiet code,\nno one sees us land.", "A haiku is 5-7-5. Area 51 is in the Nevada desert, so: dunes, lights, a quiet fleet."),
    ("plan", "Here's your week:\n\n- **Mon**: finish the report draft\n- **Wed**: review notes with the team\n- **Fri**: send the final version", "The notes mention a report due Friday, so work backwards from that."),
]

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
            self.send({"capabilities": caps, "model_info": {"qwen3.context_length": 40960}})
        elif self.path == "/api/pull":
            self.send_response(200)
            self.end_headers()
            for st in ["pulling manifest", "success"]:
                self.wfile.write(json.dumps({"status": st}).encode() + b"\n")
            MODELS[b["model"]] = 1
        elif self.path == "/api/chat":
            model = b["model"]
            if model not in MODELS:
                return self.send({"error": {"message": f"model '{model}' not found"}}, 404)
            last = b["messages"][-1]
            # For recording the README demo: a couple of short, plausible replies.
            if DEMO and isinstance(last.get("content"), str) and last["role"] == "user":
                for key, reply, why in DEMO_REPLIES:
                    if key in last["content"].lower():
                        time.sleep(1.2)
                        return self.send({"message": {"role": "assistant", "content": reply, "thinking": why}, "done": True, "done_reason": "stop", "prompt_eval_count": 1, "eval_count": 1})
            if last.get("images"):
                images = sum(1 for i in last["images"] if i.startswith("iVBOR"))
                self.send({"message": {"role": "assistant", "content": f"I can see {images} image(s). {last['content'][-80:]}"}, "done": True, "done_reason": "stop", "prompt_eval_count": 1, "eval_count": 1})
                return
            msg = {"role": "assistant", "content": f"Hello from {model}."}
            if last["role"] == "user" and last["content"].startswith("[Automation"):
                msg = {"role": "assistant", "content": f"Automation ran with {len(b['messages'])} messages."}
            elif last["role"] == "user" and "save a note" in last["content"]:
                msg = {"role": "assistant", "content": "", "tool_calls": [{"id": "c1", "type": "function", "function": {"name": "write_file", "arguments": {"path": "notes.md", "content": "hi"}}}]}
            elif last["role"] == "user" and "escape" in last["content"]:
                msg = {"role": "assistant", "content": "", "tool_calls": [{"id": "c2", "type": "function", "function": {"name": "write_file", "arguments": {"path": "../escaped.txt", "content": "pwned"}}}]}
            elif last["role"] == "user" and "break it" in last["content"]:
                msg = {"role": "assistant", "content": "", "tool_calls": [{"id": "c3", "type": "function", "function": {"name": "write_file", "arguments": {"path": "notes.md", "content": "broken"}}}]}
            elif last["role"] == "user" and "run code" in last["content"]:
                cmd = "echo ran-in-sandbox > out.txt && cat out.txt && echo nope > ../sandbox-escape.txt"
                if os.name == "nt":  # Windows runs commands in cmd, without a sandbox
                    cmd = "echo ran-unprotected> out.txt && type out.txt"
                msg = {"role": "assistant", "content": "", "tool_calls": [{"id": "c4", "type": "function", "function": {"name": "run_command", "arguments": {"command": cmd}}}]}
            elif last["role"] == "user" and "context size" in last["content"]:
                msg = {"role": "assistant", "content": f"My context is {b.get('options', {}).get('num_ctx')} tokens."}
            elif last["role"] == "tool":
                msg = {"role": "assistant", "content": f"Tool said: {last['content']}"}
            if b.get("think") is not False:
                msg["thinking"] = f"Thinking about it with {model}."
            time.sleep(0.2)
            self.send({"message": msg, "done": True, "done_reason": "length" if "cut off" in str(last.get("content")) else "stop", "prompt_eval_count": 10, "eval_count": 5})
        else:
            self.send({"error": "not found"}, 404)

    def do_DELETE(self):
        MODELS.pop(self.body().get("model"), None)
        self.send({})


ThreadingHTTPServer(("127.0.0.1", int(sys.argv[1])), H).serve_forever()
