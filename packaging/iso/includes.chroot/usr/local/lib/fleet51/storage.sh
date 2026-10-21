#!/bin/sh
# Where Fleet-51 keeps chats, bot folders and models in the VM: a disk labelled FLEET51 (used
# before), else a completely blank disk, which is formatted and labelled for next time. A disk
# with anything on it is never touched. With no such disk, everything lives in memory and is
# gone when the VM stops.
set -eu

dev=$(blkid -L FLEET51 2>/dev/null || true)
if [ -z "$dev" ]; then
  for sys in /sys/block/sd* /sys/block/vd* /sys/block/nvme*n*; do
    [ -e "$sys" ] || continue
    name=${sys##*/}
    disk=/dev/$name
    [ "$(cat "$sys/removable")" = 0 ] && [ "$(cat "$sys/ro")" = 0 ] || continue
    # Partitions, a filesystem, or any data in its first megabyte: not ours to format.
    ls "$sys" | grep -q "^$name" && continue
    [ -z "$(blkid -p "$disk" 2>/dev/null)" ] || continue
    [ "$(head -c 1048576 "$disk" | tr -d '\000' | wc -c)" = 0 ] || continue
    mkfs.ext4 -q -L FLEET51 "$disk"
    dev=$disk
    break
  done
fi

mkdir -p /data
if [ -n "$dev" ]; then
  mount -o noatime "$dev" /data
  echo disk >/run/fleet51-storage
else
  mount -t tmpfs -o size=75%,mode=0755 tmpfs /data
  echo memory >/run/fleet51-storage
fi
mkdir -p /data/home
chown fleet51:fleet51 /data/home
chmod 700 /data/home
