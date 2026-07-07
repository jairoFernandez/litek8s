#!/usr/bin/env bash
# End-to-end deploy of {{PROJECT_NAME}} onto the litek8s nodes provisioned by
# terraform/. Run from this project root AFTER `terraform -chdir=terraform apply`.
#
#   ./deploy.sh
#
# Steps: read node coordinates from terraform output → register every node with
# litek8s → apply deploy/*.yaml in order → smoke test through the Service.
set -euo pipefail

PROJECT_ROOT="$(cd "$(dirname "$0")" && pwd)"
TF_DIR="$PROJECT_ROOT/terraform"
MANIFEST_DIR="$PROJECT_ROOT/deploy"
SSH_USER="ubuntu"

# SSH key for node access. --ssh-key makes litek8s use `-i <key> -o
# IdentitiesOnly=yes`, avoiding "Too many authentication failures" when the
# local ssh-agent holds many keys. Any raw ssh call below uses the same opts.
SSH_KEY="${SSH_KEY:-$HOME/.ssh/id_ed25519}"
SSH_OPTS=(-o StrictHostKeyChecking=accept-new -o IdentitiesOnly=yes -i "$SSH_KEY")

# --- 1. Pull node coordinates from terraform state ---
# hetero_nodes is a map of { name => { public_ip, private_ip, ... } }.
NODES_JSON=$(terraform -chdir="$TF_DIR" output -json hetero_nodes)
NODE_NAMES=$(echo "$NODES_JSON" | jq -r 'keys[]')

echo "nodes:"
echo "$NODES_JSON" | jq -r 'to_entries[] | "  \(.key): pub=\(.value.public_ip) priv=\(.value.private_ip)"'

# --- 2. Start the control-plane daemon (if not already up) ---
if ! litek8s get nodes >/dev/null 2>&1; then
  echo "starting litek8s daemon..."
  litek8s serve &
  sleep 3
fi

# --- 3. Register nodes (--install cross-builds + ships the agent binary) ---
for name in $NODE_NAMES; do
  pub=$(echo "$NODES_JSON" | jq -r --arg n "$name" '.[$n].public_ip')
  echo "registering node $name ($pub)..."
  litek8s node add "$name" \
    --ssh "$SSH_USER@$pub" \
    --ssh-key "$SSH_KEY" \
    --install \
    --label role=worker
done
litek8s get nodes

# --- 4. (Optional) pre-create hostPath dirs for any volumes used by the
# manifests. litek8s bind-mounts absolute paths; Docker would otherwise create
# them root-owned. Uncomment and adjust to match volumes in deploy/app.yaml. ---
# for name in $NODE_NAMES; do
#   pub=$(echo "$NODES_JSON" | jq -r --arg n "$name" '.[$n].public_ip')
#   ssh "${SSH_OPTS[@]}" "$SSH_USER@$pub" \
#     "sudo mkdir -p /opt/{{PROJECT_NAME}}/data && sudo chmod -R 777 /opt/{{PROJECT_NAME}}"
# done

# --- 5. Apply the app, then the service (ordered) ---
litek8s apply -f "$MANIFEST_DIR/app.yaml"
litek8s apply -f "$MANIFEST_DIR/service.yaml"

litek8s get deployments
litek8s get services

# --- 6. Smoke test through the litek8s Service. The Service binds
# 127.0.0.1:8080 locally and forwards to the replica. hostPort 8080 is NOT open
# in the node security group, so this is the intended access path. ---
echo "smoke test: GET http://127.0.0.1:8080/ (via litek8s Service)"
ok=0
for i in $(seq 1 24); do
  if curl -fsS "http://127.0.0.1:8080/" >/dev/null 2>&1; then
    echo "  ok"
    ok=1
    break
  fi
  echo "  not ready (attempt $i), retrying in 5s..."
  sleep 5
done
[ "$ok" = 1 ] || echo "  WARNING: smoke test did not pass — inspect with: litek8s logs {{PROJECT_NAME}}"
