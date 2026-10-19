#!/usr/bin/env bash
# Docker engine benchmark: the same protocol against whatever DOCKER_HOST
# points at (litek8s, dockerd, OrbStack). Base images are pulled first and
# not timed. Prints one JSON object.
#
#   DOCKER_HOST=unix://$HOME/.litek8s/docker.sock bench/docker-bench.sh [runs]
#
#   run_ms        median `docker run --rm alpine true`
#   nginx_ms      `docker run -d -p` nginx:alpine until it answers on localhost
#   compose_ms    compat/docker/compose-basic `up -d` until the client job ran
#   build_cold_s  compat/docker/compose-build's app image, no cache
#   build_warm_s  the same, all cached
#   build_edit_s  after changing the copied source file (last layers rebuilt)
# Builds use the engine's default builder unless BUILDER=classic
# (DOCKER_BUILDKIT=0).
set -euo pipefail
HERE="$(cd "$(dirname "$0")/.." && pwd)"
RUNS="${1:-10}"
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT
export DOCKER_CLI_HINTS=false
[[ "${BUILDER:-default}" == classic ]] && export DOCKER_BUILDKIT=0 COMPOSE_BAKE=false

now_ms() { perl -MTime::HiRes=time -e 'printf "%d", time * 1000'; }
secs() { perl -e "printf '%.2f', $1 / 1000"; }
median() { sort -n | awk '{a[NR]=$1} END {print (NR % 2) ? a[(NR+1)/2] : int((a[NR/2] + a[NR/2+1]) / 2)}'; }

docker pull -q alpine:3.20 >/dev/null
docker pull -q nginx:alpine >/dev/null
docker pull -q postgres:16-alpine >/dev/null
docker pull -q python:3.12-alpine >/dev/null
docker pull -q redis:7-alpine >/dev/null

docker run --rm alpine:3.20 true # warm
run=$(for _ in $(seq 1 "$RUNS"); do s=$(now_ms); docker run --rm alpine:3.20 true; echo $(( $(now_ms) - s )); done | median)

nginx=$(for i in $(seq 1 3); do
  s=$(now_ms)
  docker run -d --name bench-nginx-$i -p 18790:80 nginx:alpine >/dev/null
  until curl -sf localhost:18790 >/dev/null 2>&1; do sleep 0.01; done
  echo $(( $(now_ms) - s ))
  docker rm -f bench-nginx-$i >/dev/null
done | median)

cp -r "$HERE/compat/docker/compose-basic" "$WORK/cb"
compose=$(for _ in 1 2 3; do
  s=$(now_ms)
  (cd "$WORK/cb" && docker compose up -d >/dev/null 2>&1)
  until (cd "$WORK/cb" && docker compose logs client 2>/dev/null | grep -q hello-from-env-file); do sleep 0.05; done
  echo $(( $(now_ms) - s ))
  (cd "$WORK/cb" && docker compose down -v >/dev/null 2>&1)
done | median)

cp -r "$HERE/compat/docker/compose-build/app" "$WORK/app"
s=$(now_ms); docker build -q --no-cache -t bench-app:1 "$WORK/app" >/dev/null; cold=$(( $(now_ms) - s ))
s=$(now_ms); docker build -q -t bench-app:1 "$WORK/app" >/dev/null; warm=$(( $(now_ms) - s ))
echo "# edit" >> "$WORK/app/server.py"
s=$(now_ms); docker build -q -t bench-app:1 "$WORK/app" >/dev/null; edit=$(( $(now_ms) - s ))
docker rmi bench-app:1 >/dev/null 2>&1 || true

server=$(docker version --format '{{.Server.Version}} {{.Server.Os}}/{{.Server.Arch}}')
printf '{"server": "%s", "builder": "%s", "run_ms": %s, "nginx_ms": %s, "compose_ms": %s, "build_cold_s": %s, "build_warm_s": %s, "build_edit_s": %s}\n' \
  "$server" "${BUILDER:-default}" "$run" "$nginx" "$compose" \
  "$(secs "$cold")" "$(secs "$warm")" "$(secs "$edit")"
