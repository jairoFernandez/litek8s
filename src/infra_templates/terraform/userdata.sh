#!/usr/bin/env bash
set -euxo pipefail

# Install Docker and grant the SSH user socket access so the
# litek8s agent (~/.local/bin/litek8s agent) can talk to the daemon.
export DEBIAN_FRONTEND=noninteractive

# Retry apt to ride out transient Ubuntu mirror sync errors on fresh boots.
for attempt in 1 2 3 4 5; do
  if apt-get update -y; then break; fi
  echo "apt-get update failed (attempt ${attempt}), retrying in 15s..."
  sleep 15
done

for attempt in 1 2 3 4 5; do
  if apt-get install -y docker.io; then break; fi
  echo "docker.io install failed (attempt ${attempt}), retrying in 15s..."
  apt-get update -y || true
  sleep 15
done

systemctl enable --now docker
usermod -aG docker ubuntu
