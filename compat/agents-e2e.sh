#!/usr/bin/env bash
# End to end: can an LLM agent diagnose broken pods on a litek8s dev cluster
# through `litek8s mcp`? Starts a throwaway dev cluster, deploys two broken
# Deployments (a crash loop whose reason is in its previous logs, a readiness
# probe on the wrong port), asks Claude Code headless (`claude -p`, only the
# litek8s MCP tools allowed, the skill installed in a scratch project) why,
# checks the answer names both causes, and deletes the cluster.
#
#   compat/agents-e2e.sh                      # target/debug/litek8s
#   LITEK8S=~/.cargo/bin/litek8s compat/agents-e2e.sh
#
# Needs: kubectl, python3, a logged-in `claude` CLI (`claude auth status`).
# Leaves the user's other clusters and the current kube context alone.
set -euo pipefail

ROOT=$(cd "$(dirname "$0")/.." && pwd)
BIN=${LITEK8S:-$ROOT/target/debug/litek8s}
NAME=${NAME:-agents-e2e}
CTX=litek8s-$NAME
WORK=$(mktemp -d)
cleanup() {
  "$BIN" dev delete --name "$NAME" >/dev/null 2>&1 || true
  rm -rf "$WORK"
}
trap cleanup EXIT

if ! claude auth status 2>/dev/null | grep -q '"loggedIn": true'; then
  echo "agents-e2e: the claude CLI is not logged in (claude auth login)" >&2
  exit 2
fi

"$BIN" dev up --name "$NAME" --no-switch
kubectl --context "$CTX" apply -f - <<'EOF'
apiVersion: apps/v1
kind: Deployment
metadata: {name: shop, labels: {app: shop}}
spec:
  replicas: 1
  selector: {matchLabels: {app: shop}}
  template:
    metadata: {labels: {app: shop}}
    spec:
      containers:
      - name: shop
        image: busybox:1.36
        command: ["sh", "-c", "echo starting shop v2; echo 'connecting to database...'; echo 'FATAL: DATABASE_URL is not set' >&2; exit 1"]
---
apiVersion: apps/v1
kind: Deployment
metadata: {name: web, labels: {app: web}}
spec:
  replicas: 1
  selector: {matchLabels: {app: web}}
  template:
    metadata: {labels: {app: web}}
    spec:
      containers:
      - name: web
        image: nginx:1.27-alpine
        readinessProbe: {httpGet: {path: /, port: 8080}, periodSeconds: 3}
EOF

# Wait until the crash loop has restarted at least twice.
for _ in $(seq 1 60); do
  restarts=$(kubectl --context "$CTX" get pods -l app=shop -o jsonpath='{.items[0].status.containerStatuses[0].restartCount}' 2>/dev/null || true)
  [[ "${restarts:-0}" -ge 2 ]] && break
  sleep 2
done
kubectl --context "$CTX" get pods

cat > "$WORK/mcp.json" <<EOF
{"mcpServers": {"litek8s": {"command": "$BIN", "args": ["mcp"]}}}
EOF
(cd "$WORK" && "$BIN" skill --install .claude/skills >/dev/null)

PROMPT="My litek8s dev cluster $NAME (kube context $CTX) has two deployments, shop and web, that never become ready. Find out why each one is broken and tell me the fix. Do not change anything in the cluster."
(cd "$WORK" && claude -p "$PROMPT" --mcp-config mcp.json --strict-mcp-config --allowedTools 'mcp__litek8s__*' \
  --output-format stream-json --verbose >"$WORK/run.jsonl")

python3 - "$WORK/run.jsonl" <<'EOF'
import json, sys
tools, result = [], ""
for line in open(sys.argv[1]):
    v = json.loads(line)
    if v.get("type") == "assistant":
        tools += [c["name"] for c in v["message"]["content"] if c["type"] == "tool_use"]
    if v.get("type") == "result":
        result = v.get("result") or ""
print("tools used:", ", ".join(tools))
print(result)
ok = "DATABASE_URL" in result and "8080" in result and any(t.startswith("mcp__litek8s__") for t in tools)
print("agents-e2e:", "PASS" if ok else "FAIL")
sys.exit(0 if ok else 1)
EOF
