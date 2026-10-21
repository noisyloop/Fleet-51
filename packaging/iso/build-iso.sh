#!/usr/bin/env bash
# Build Fleet-51 as a bootable ISO (x86-64): a small Debian live system that starts Fleet-51 by
# itself, for VirtualBox and other VMs. Runs as root on Debian (CI runs it in a debian:bookworm
# container) and needs live-build, zstd and curl.
#   packaging/iso/build-iso.sh <version> <static fleet51 binary> <output.iso>
set -euo pipefail

VERSION="$1"
BIN="$(realpath "$2")"
OUT="$(realpath -m "$3")"
HERE="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$HERE/../.." && pwd)"
OLLAMA_VERSION=$(sed -n 's/^pub const OLLAMA_VERSION: &str = "\(.*\)";/\1/p' "$ROOT/src/runtime.rs")
# From Ollama's sha256sum.txt for that release.
OLLAMA_SHA256=c164ab8b1b28e2a247c29719b8e404c423c5b83ae64c3ab4e59d50c7ff7fc5e0
WORK="$(mktemp -d)"
cd "$WORK"

lb config \
  --distribution bookworm \
  --architectures amd64 \
  --archive-areas main \
  --binary-images iso-hybrid \
  --bootloaders "syslinux grub-efi" \
  --debian-installer none \
  --apt-recommends false \
  --memtest none \
  --iso-application "Fleet-51" \
  --iso-volume "Fleet-51 $VERSION" \
  --bootappend-live "boot=live components live-config.noautologin live-config.nouser live-config.hostname=fleet51 console=tty0 console=ttyS0,115200 quiet"

cat >config/package-lists/fleet51.list.chroot <<'EOF'
bubblewrap
ca-certificates
systemd-resolved
e2fsprogs
util-linux
EOF

cp -a "$HERE/includes.chroot/." config/includes.chroot/
install -Dm755 "$BIN" config/includes.chroot/usr/local/bin/fleet51

# Ollama for Linux, checked against its published SHA-256. NVIDIA and AMD libraries are left out:
# VMs have no graphics card, so models run on the processor.
curl -fsSL -o ollama.tar.zst \
  "https://github.com/ollama/ollama/releases/download/$OLLAMA_VERSION/ollama-linux-amd64.tar.zst"
echo "$OLLAMA_SHA256  ollama.tar.zst" | sha256sum -c -
tar --zstd -xf ollama.tar.zst -C config/includes.chroot/usr/local \
  --exclude='lib/ollama/cuda_*' --exclude='lib/ollama/rocm*' --exclude='lib/ollama/mlx*'
rm ollama.tar.zst
du -sh config/includes.chroot/usr/local

cat >config/hooks/normal/9000-fleet51.hook.chroot <<'EOF'
#!/bin/sh
set -e
useradd --system --home-dir /data/home --no-create-home --shell /usr/sbin/nologin fleet51
chmod 755 /usr/local/lib/fleet51/*.sh
systemctl enable systemd-networkd systemd-resolved fleet51-storage fleet51 fleet51-console
EOF

# Boot by itself after 3 seconds instead of waiting at the menu.
cp -a /usr/share/live/build/bootloaders config/
cfg=config/bootloaders/isolinux/isolinux.cfg
if grep -q '^timeout' "$cfg"; then sed -i 's/^timeout .*/timeout 30/' "$cfg"; else echo 'timeout 30' >>"$cfg"; fi
for f in config/bootloaders/grub-pc/config.cfg config/bootloaders/grub-pc/grub.cfg; do
  [ ! -f "$f" ] || sed -i 's/set timeout=.*/set timeout=3/' "$f"
done

lb build
mkdir -p "$(dirname "$OUT")"
mv ./*.iso "$OUT"
ls -la "$OUT"
