#!/bin/sh
# PID 1's child (tini): sway, which starts wayvnc and Chromium; websockify
# serves noVNC on :6080 and forwards to wayvnc on localhost. Only the pod's
# localhost and :6080 are reachable; reach them with kubectl port-forward.
set -eu
mkdir -p "$XDG_RUNTIME_DIR" /data/profile
chmod 700 "$XDG_RUNTIME_DIR"
sed "s/@SIZE@/$DESKTOP_SIZE/" /etc/agent-desktop/sway.conf > /tmp/sway.conf

websockify --web /usr/share/novnc 6080 127.0.0.1:5900 >/tmp/websockify.log 2>&1 &
exec sway --config /tmp/sway.conf
