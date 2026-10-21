#!/usr/bin/env bash
# One ci-bench "in a container" measurement: the way a GitLab.com job runs
# (a privileged container), an example from examples/github-actions/ on
# Docker in Docker (docker:dind) or on litek8s (examples/ci/litek8s-ci.sh in
# ubuntu:24.04).
#
#   .github/ci-bench/in-container.sh <dind|litek8s> <compose|helm>
#
# The harness's own tools (bash, bc, yq, kubectl, helm) are put in place
# before the clock starts; what the engine needs (dockerd starting, kind's
# node image, litek8s's apt packages and download) is timed. Writes
# result.json in $RUNNER_TEMP: time to ready, the example's steps, and the
# container's peak memory (its cgroup's memory.peak: everything in it).
set -euo pipefail
engine=${1:?engine} example=${2:?example}
out="${RUNNER_TEMP:-/tmp}"
name="bench-$engine-$example"
# The runner's tools (static Go binaries), wherever the image put them.
tools=(-v "$(command -v yq):/usr/local/bin/yq:ro" -v "$(command -v kubectl):/usr/local/bin/kubectl:ro" -v "$(command -v helm):/usr/local/bin/helm:ro")

if [[ $engine == dind ]]; then
  docker run -d --name "$name" --privileged --entrypoint sleep \
    -v "$PWD:/src" -w /src "${tools[@]}" \
    docker:27-dind infinity >/dev/null
  docker exec "$name" apk add -q bash bc curl >/dev/null
  # The runner's kubectl is linked against glibc; Alpine needs the static one.
  docker exec "$name" sh -c 'curl -fsSLo /usr/local/bin/kubectl.static https://dl.k8s.io/release/v1.33.0/bin/linux/amd64/kubectl && chmod +x /usr/local/bin/kubectl.static'
else
  docker run -d --name "$name" --privileged --entrypoint sleep \
    -v "$PWD:/src" -w /src "${tools[@]}" \
    -v "$(command -v docker):/usr/local/bin/docker:ro" \
    -v /usr/libexec/docker/cli-plugins:/usr/local/lib/docker/cli-plugins:ro \
    ubuntu:24.04 infinity >/dev/null
  docker exec "$name" bash -c 'apt-get update -qq && apt-get install -y -qq bc curl ca-certificates >/dev/null'
fi
cid=$(docker inspect -f '{{.Id}}' "$name")
cg=/sys/fs/cgroup/system.slice/docker-$cid.scope

# The engine, timed from here.
docker exec -i "$name" bash -s "$engine" "$example" > "$out/inner.log" 2>&1 <<'INNER' || { cat "$out/inner.log"; exit 1; }
set -euo pipefail
engine=$1 example=$2
now() { date +%s.%N; }
start=$(now)
if [[ $engine == dind ]]; then
  mkdir -p /opt/bin && ln -sf /usr/local/bin/kubectl.static /opt/bin/kubectl && export PATH=/opt/bin:$PATH
  # What the docker:dind image's entrypoint does as a GitLab service.
  (dockerd-entrypoint.sh dockerd >/var/log/dockerd.log 2>&1 &)
  for _ in $(seq 120); do docker info >/dev/null 2>&1 && break; sleep 0.5; done
  docker info >/dev/null
  if [[ $example == helm ]]; then
    curl -fsSLo /usr/local/bin/kind https://kind.sigs.k8s.io/dl/v0.33.0/kind-linux-amd64 && chmod +x /usr/local/bin/kind
    kind create cluster --wait 180s >/dev/null
    kubectl taint nodes --all node-role.kubernetes.io/control-plane:NoSchedule- >/dev/null 2>&1 || true
  fi
else
  eval "$(sh examples/ci/litek8s-ci.sh 2>/var/log/litek8s-ci.log)"
  export KUBECONFIG DOCKER_HOST DOCKER_BUILDKIT COMPOSE_BAKE
  [[ $example == helm ]] && kubectl wait --for=condition=Ready nodes --all --timeout=180s >/dev/null
fi
ready=$(echo "$(now) - $start" | bc)
echo "READY_SECONDS=$(printf '%.1f' "$ready")"
examples/github-actions/run.sh "$example"
INNER

cat "$out/inner.log"
ready=$(grep -o 'READY_SECONDS=.*' "$out/inner.log" | cut -d= -f2)
steps=$(grep -o 'STEP_SECONDS=.*' "$out/inner.log" | cut -d= -f2)
peak=$(( $(cat "$cg/memory.peak") / 1048576 ))
docker rm -f "$name" >/dev/null
jq -n --arg example "$example" --arg engine "$engine" --arg ready "$ready" --arg steps "$steps" --arg peak "$peak" \
  '{example: $example, engine: ($engine + " (container)"), ready_seconds: $ready, steps_seconds: $steps, idle_mib: "", peak_mib: $peak}' |
  tee "$out/result.json"
