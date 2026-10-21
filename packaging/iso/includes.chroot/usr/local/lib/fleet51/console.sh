#!/bin/sh
# The VM's screen: how to open Fleet-51 from the computer running the VM, with the access code.
# Also written to the serial port, where the automated boot test reads it.
set -eu

token=/data/home/.fleet51/token
for _ in $(seq 120); do [ -s "$token" ] && break; sleep 1; done
code=$(tr -d '[:space:]' <"$token" 2>/dev/null | sed 's/.\{4\}/& /g; s/ $//')
if [ "$(cat /run/fleet51-storage 2>/dev/null)" = disk ]; then
  keep="Chats and models are kept on the attached disk (FLEET51)."
else
  keep="No empty disk attached: chats and models are kept in memory only and are
  gone when the VM stops. To keep them, attach an empty virtual disk (8 GB or more)."
fi

cat >/etc/issue <<EOF

  Fleet-51 is running.

  On the computer running this VM, open   http://localhost:7499
  and enter this access code:

      $code

  (VirtualBox: Settings > Network > Adapter 1: NAT > Port Forwarding,
   add a rule: Host IP 127.0.0.1, host port 7499, guest port 7499.)

  $keep

EOF
cat /etc/issue >/dev/ttyS0 2>/dev/null || true
echo "fleet51-token: $(tr -d '[:space:]' <"$token")" >/dev/ttyS0 2>/dev/null || true
systemctl restart getty@tty1.service || true
