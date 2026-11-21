#!/usr/bin/env bash
# Asks the Service, through kubectl port-forward, for the version it serves.
#   ./smoke.sh v1
set -euo pipefail
want=${1:?version}
kubectl port-forward svc/app 18080:80 >/dev/null 2>&1 &
pf=$!
trap 'kill $pf 2>/dev/null' EXIT
for i in $(seq 30); do
    got=$(curl -fsS -m 2 localhost:18080/ 2>/dev/null || true)
    [[ $got == "version: $want" ]] && { echo "$got"; exit 0; }
    sleep 1
done
echo "expected version: $want, got: ${got:-nothing}" >&2
exit 1
