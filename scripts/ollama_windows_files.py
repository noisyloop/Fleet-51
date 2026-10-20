"""Pin the files Fleet-51 takes from Ollama's Windows zip, each with its own SHA-256.

Fleet-51 doesn't download the whole zip (about 1.4 GB, nearly all NVIDIA CUDA libraries): it
reads just the files it needs out of the zip on GitHub and checks each against this list. The
list is made here from the full official zip, after checking that zip against the SHA-256
Ollama publishes in sha256sum.txt, so every pinned hash traces back to Ollama's own.

Usage: python scripts/ollama_windows_files.py v0.40.6 <sha256 of ollama-windows-amd64.zip>
Writes src/ollama-windows-files.txt. Run it again whenever OLLAMA_VERSION changes.
"""
import hashlib
import os
import sys
import tempfile
import urllib.request
import zipfile

# Left out: NVIDIA's CUDA libraries. Vulkan (in lib/ollama/vulkan) runs models on NVIDIA, AMD
# and Intel graphics instead.
SKIP = ("lib/ollama/cuda_",)

version, want = sys.argv[1], sys.argv[2]
url = f"https://github.com/ollama/ollama/releases/download/{version}/ollama-windows-amd64.zip"
out = os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "src", "ollama-windows-files.txt")

with tempfile.TemporaryDirectory() as tmp:
    path = os.path.join(tmp, "ollama.zip")
    h = hashlib.sha256()
    with urllib.request.urlopen(url) as r, open(path, "wb") as f:
        while chunk := r.read(1 << 20):
            h.update(chunk)
            f.write(chunk)
    if h.hexdigest() != want:
        sys.exit(f"the zip's SHA-256 is {h.hexdigest()}, not {want}")
    lines = []
    with zipfile.ZipFile(path) as z:
        for info in z.infolist():
            if info.is_dir() or info.filename.startswith(SKIP):
                continue
            lines.append(f"{hashlib.sha256(z.read(info)).hexdigest()}  {info.filename}")
    with open(out, "w", newline="\n") as f:
        f.write(f"# Ollama {version}, ollama-windows-amd64.zip (sha256 {want}), without CUDA.\n")
        f.write("# Made by scripts/ollama_windows_files.py; Fleet-51 downloads and checks these.\n")
        f.write("\n".join(lines) + "\n")
    print(f"{len(lines)} files written to {os.path.normpath(out)}")
