#!/usr/bin/env bash
# Run a real LLM (Qwen2.5-0.5B) distributed across the cluster: node-big-0
# coordinates, the 2 ARM peers hold the sharded weights, generation flows over
# the private LAN. You just pass a prompt.
#
#   ./examples/run-distributed-llm.sh "The capital of France is"
#   ./examples/run-distributed-llm.sh "Once upon a time" 10
#
# Edit COORDINATOR / PEERS if your node IPs change (terraform output hetero_nodes).
set -euo pipefail

KEY=~/.ssh/id_ed25519
COORDINATOR=ubuntu@54.165.57.182       # node-big-0 public IP (where the model lives)
PEERS=10.42.1.13,10.42.1.19            # ARM peers' private IPs
MODEL_DIR='~/qwen05'                    # model dir on the coordinator (quoted: expands there, not here)

PROMPT="${1:-The capital of France is}"
MAX_NEW="${2:-6}"

ssh_run() { ssh -o BatchMode=yes -i "$KEY" -o IdentitiesOnly=yes "$@"; }

echo "Checking peer agents are up over the LAN..."
for ip in ${PEERS//,/ }; do
  case $ip in
    10.42.1.13) pub=34.230.4.160 ;;
    10.42.1.19) pub=98.80.119.80 ;;
    *) pub="" ;;
  esac
  if [ -n "$pub" ]; then
    # wake the peer's persistent agent (idempotent), it listens on tcp/6444
    ssh_run "ubuntu@$pub" 'pgrep -f "agent --serve" >/dev/null || (setsid ~/.local/bin/litek8s agent --serve >/tmp/a.log 2>&1 </dev/null &); sleep 1' || true
  fi
done

echo "Generating (node-big-0 coordinates, ARM peers compute over the LAN)..."
echo
ssh_run "$COORDINATOR" \
  "~/.local/bin/litek8s llm distributed --model-dir $MODEL_DIR --peers $PEERS '$PROMPT' --max-new $MAX_NEW"
