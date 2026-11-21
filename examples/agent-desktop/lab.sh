#!/usr/bin/env bash
# A lightweight desktop for an agent, on a litek8s dev cluster: sway headless
# + Chromium (CDP for the agent) + wayvnc/noVNC (for a person to take over).
#
#   ./lab.sh up            cluster `desk`, image, StatefulSet, then `open`
#   ./lab.sh open          port-forwards: the desktop in your browser, CDP
#   ./lab.sh agent URL     the agent opens a tab (CDP), you watch it in the desktop
#   ./lab.sh shot [FILE]   a screenshot of the whole desktop (grim), default desk.png
#   ./lab.sh mem           memory per process (PSS, shared pages counted once)
#   ./lab.sh down          stop the cluster (objects, image and profile kept)
#   ./lab.sh delete        delete the cluster
#
# LK picks the litek8s: this checkout's dev/lk by default (its own
# LITEK8S_HOME, never your clusters); LK=litek8s for the installed release.
# CLUSTER (default desk), NOVNC_PORT (6080), CDP_PORT (9222).
set -euo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$HERE/../.." && pwd)"
LK="${LK:-$ROOT/dev/lk}"
CLUSTER="${CLUSTER:-desk}"
NOVNC_PORT="${NOVNC_PORT:-6080}"
CDP_PORT="${CDP_PORT:-9222}"
POD=agent-desktop-0

status_field() {
  $LK dev status --name "$CLUSTER" -o json | python3 -c "import json,sys; print(json.load(sys.stdin).get('$1') or '')"
}

kube_env() {
  [[ $LK == */dev/lk ]] && eval "$($LK env | grep KUBECONFIG)"
  CONTEXT="$(status_field context)"
  export DOCKER_HOST="unix://$(status_field docker_socket)"
}
k() { kubectl --context "$CONTEXT" "$@"; }

cmd_up() {
  $LK dev up --name "$CLUSTER" --no-switch
  kube_env
  DOCKER_BUILDKIT=0 docker build -t agent-desktop:lab "$HERE/image"
  k apply -f "$HERE/desktop.yaml"
  # A rebuilt image under the same tag needs a new pod.
  k delete pod "$POD" --ignore-not-found --wait=false
  k rollout status statefulset/agent-desktop --timeout=3m
  cmd_open
}

cmd_open() {
  kube_env
  echo "desktop: http://localhost:$NOVNC_PORT/vnc.html?autoconnect=1&resize=scale"
  echo "CDP:     http://localhost:$CDP_PORT/json/version"
  echo "(Ctrl-C to stop the port-forwards)"
  k port-forward "pod/$POD" "$NOVNC_PORT:6080" "$CDP_PORT:9222"
}

cmd_agent() {
  local url="${1:?URL}"
  curl -fsS -X PUT "http://localhost:$CDP_PORT/json/new?$url" >/dev/null \
    || { echo "no CDP on localhost:$CDP_PORT: run ./lab.sh open first" >&2; exit 1; }
  echo "agent opened $url"
}

cmd_shot() {
  kube_env
  local out="${1:-desk.png}"
  k exec "$POD" -- sh -c 'SWAYSOCK=$(ls $XDG_RUNTIME_DIR/sway-ipc.*.sock) WAYLAND_DISPLAY=wayland-1 grim -' > "$out"
  echo "$out"
}

cmd_mem() {
  kube_env
  k exec "$POD" -- sh -c '
    for p in /proc/[0-9]*; do
      pss=$(awk "/^Pss:/{print \$2}" $p/smaps_rollup 2>/dev/null) || continue
      [ -n "$pss" ] && echo "$(cat $p/comm) $pss"
    done | awk "{a[\$1]+=\$2; t+=\$2} END {for (k in a) printf \"%-16s %5.0f MiB\n\", k, a[k]/1024; printf \"%-16s %5.0f MiB\n\", \"TOTAL\", t/1024}" | sort -k2 -n -r'
}

case "${1:-}" in
  up) cmd_up ;;
  open) cmd_open ;;
  agent) shift; cmd_agent "$@" ;;
  shot) shift; cmd_shot "$@" ;;
  mem) cmd_mem ;;
  down) $LK dev down --name "$CLUSTER" ;;
  delete) $LK dev delete --name "$CLUSTER" ;;
  *) sed -n '2,16p' "$0" | sed 's/^# \{0,1\}//' >&2; exit 2 ;;
esac
