#!/usr/bin/env bash
# The Docker compatibility corpus: the docker CLI, compose, the classic
# builder, Testcontainers and the Dev Containers CLI against a Docker API.
#
#   DOCKER_HOST=unix://$HOME/.litek8s/docker.sock compat/docker/run.sh [case...]
#
# Runs against whatever DOCKER_HOST points at (run it against Docker or
# OrbStack too: every case must pass there). Needs docker with the compose
# plugin; the testcontainers case needs python3 (it makes a venv), the
# devcontainer case npx. Prints one line per case and a summary; exits 1 if
# any case failed. Cases clean up after themselves (containers, networks,
# volumes and images they create; pulled base images stay).
set -uo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
WORK="${COMPAT_DOCKER_WORK:-$(mktemp -d)}"
LOG="$WORK/log"
mkdir -p "$LOG"
export DOCKER_CLI_HINTS=false
PASS=0; FAIL=0; FAILED=()

check() { # name, command...
  local name="$1"; shift
  local start=$(date +%s.%N)
  if "$@" >"$LOG/$name.log" 2>&1; then
    PASS=$((PASS + 1)); printf 'PASS  %-28s %6.1fs\n' "$name" "$(echo "$(date +%s.%N) - $start" | bc)"
  else
    FAIL=$((FAIL + 1)); FAILED+=("$name"); printf 'FAIL  %-28s %6.1fs  (%s)\n' "$name" "$(echo "$(date +%s.%N) - $start" | bc)" "$LOG/$name.log"
    tail -5 "$LOG/$name.log" | sed 's/^/      /'
  fi
}

eq() { # expected actual
  if [[ "$1" != "$2" ]]; then echo "expected [$1], got [$2]"; return 1; fi
}

# --- docker CLI -----------------------------------------------------------

case_run_attach() {
  eq "hello" "$(docker run --rm alpine:3.20 echo hello)" &&
  docker run --rm alpine:3.20 sh -c 'echo out; echo err >&2; exit 3'; eq 3 $? &&
  eq "piped" "$(echo piped | docker run -i --rm alpine:3.20 cat)" &&
  eq "a b" "$(docker run --rm -e A=a --env B=b alpine:3.20 sh -c 'echo $A $B')" &&
  eq "/work" "$(docker run --rm -w /work alpine:3.20 pwd)" &&
  eq "nobody" "$(docker run --rm -u nobody alpine:3.20 whoami)" &&
  eq "" "$(docker run --rm --entrypoint '' alpine:3.20 true)"
}

case_tty() {
  # A pseudo-terminal for the container (script(1) gives the CLI one).
  command -v script >/dev/null || return 0
  local out
  if [[ "$(uname)" == Darwin ]]; then
    out=$(script -q /dev/null docker run --rm -t alpine:3.20 sh -c 'tty; stty size' 2>&1 | tr -d '\r')
  else
    out=$(script -qec "docker run --rm -t alpine:3.20 sh -c 'tty; stty size'" /dev/null 2>&1 | tr -d '\r')
  fi
  echo "$out" | grep -q '/dev/pts/0'
}

case_detach_logs_exec() {
  local id
  id=$(docker run -d --name compat-web -p 18765:80 nginx:alpine) || return 1
  for _ in $(seq 1 50); do curl -sf localhost:18765 >/dev/null && break; sleep 0.2; done
  curl -sf localhost:18765 | grep -q 'Welcome to nginx' &&
  docker logs compat-web 2>&1 | grep -q 'GET / HTTP' &&
  eq "exec-ok" "$(docker exec compat-web echo exec-ok)" &&
  { docker exec compat-web sh -c 'exit 4'; eq 4 $?; } &&
  eq "running" "$(docker inspect -f '{{.State.Status}}' compat-web)" &&
  eq "18765" "$(docker port compat-web 80/tcp | head -1 | sed 's/.*://')" &&
  docker stop -t 2 compat-web >/dev/null &&
  eq "exited" "$(docker inspect -f '{{.State.Status}}' compat-web)" &&
  docker start compat-web >/dev/null &&
  eq "true" "$(docker inspect -f '{{.State.Running}}' compat-web)"
  local rc=$?
  docker rm -f compat-web >/dev/null
  return $rc
}

case_publish_all() {
  docker run -d --name compat-p -P nginx:alpine >/dev/null || return 1
  local port
  port=$(docker inspect -f '{{(index (index .NetworkSettings.Ports "80/tcp") 0).HostPort}}' compat-p)
  for _ in $(seq 1 50); do curl -sf "localhost:$port" >/dev/null && break; sleep 0.2; done
  curl -sf "localhost:$port" >/dev/null
  local rc=$?
  docker rm -f compat-p >/dev/null
  return $rc
}

case_cp_archive() {
  local d="$WORK/cp"; mkdir -p "$d/src/sub"; echo one >"$d/src/sub/f.txt"
  docker create --name compat-cp alpine:3.20 cat /data/sub/f.txt >/dev/null || return 1
  docker cp "$d/src" compat-cp:/data &&
  eq "one" "$(docker start -a compat-cp)" &&
  docker cp compat-cp:/data/sub "$d/back" &&
  eq "one" "$(cat "$d/back/f.txt")"
  local rc=$?
  docker rm -f compat-cp >/dev/null
  return $rc
}

case_networks_volumes() {
  docker network create compat-net >/dev/null &&
  docker volume create compat-vol >/dev/null &&
  docker run -d --name compat-db --network compat-net --network-alias db.local -v compat-vol:/data alpine:3.20 sh -c 'echo persisted > /data/x; sleep 300' >/dev/null &&
  eq "persisted" "$(docker run --rm --network compat-net -v compat-vol:/data alpine:3.20 cat /data/x)" &&
  docker run --rm --network compat-net alpine:3.20 ping -c1 -W2 compat-db >/dev/null &&
  docker run --rm --network compat-net alpine:3.20 ping -c1 -W2 db.local >/dev/null &&
  ! docker run --rm alpine:3.20 ping -c1 -W1 compat-db >/dev/null 2>&1
  local rc=$?
  docker rm -f compat-db >/dev/null; docker network rm compat-net >/dev/null; docker volume rm compat-vol >/dev/null
  return $rc
}

case_bind_mount() {
  local d="$WORK/bind"; mkdir -p "$d"; echo host >"$d/in.txt"
  eq "host" "$(docker run --rm -v "$d:/w" alpine:3.20 cat /w/in.txt)" &&
  docker run --rm -v "$d:/w" alpine:3.20 sh -c 'echo container > /w/out.txt' &&
  eq "container" "$(cat "$d/out.txt")" &&
  docker run --rm --mount "type=bind,source=$d,target=/w,readonly" alpine:3.20 sh -c '! touch /w/ro 2>/dev/null'
}

case_restart_policy() {
  docker run -d --name compat-rp --restart on-failure:2 alpine:3.20 sh -c 'exit 1' >/dev/null || return 1
  for _ in $(seq 1 40); do [[ "$(docker inspect -f '{{.State.Status}}' compat-rp)" == exited && "$(docker inspect -f '{{.RestartCount}}' compat-rp)" == 2 ]] && break; sleep 0.25; done
  eq "2" "$(docker inspect -f '{{.RestartCount}}' compat-rp)"
  local rc=$?
  docker rm -f compat-rp >/dev/null
  return $rc
}

case_healthcheck() {
  docker run -d --name compat-hc --health-cmd 'test -f /tmp/ok' --health-interval 1s --health-retries 1 alpine:3.20 sh -c 'sleep 2; touch /tmp/ok; sleep 300' >/dev/null || return 1
  for _ in $(seq 1 40); do [[ "$(docker inspect -f '{{.State.Health.Status}}' compat-hc)" == healthy ]] && break; sleep 0.25; done
  eq "healthy" "$(docker inspect -f '{{.State.Health.Status}}' compat-hc)"
  local rc=$?
  docker rm -f compat-hc >/dev/null
  return $rc
}

case_images() {
  docker pull -q busybox:1.36 >/dev/null &&
  docker tag busybox:1.36 compat/busybox:tagged &&
  docker image inspect compat/busybox:tagged >/dev/null &&
  docker images --format '{{.Repository}}:{{.Tag}}' | grep -q '^compat/busybox:tagged$' &&
  docker rmi compat/busybox:tagged >/dev/null &&
  ! docker image inspect compat/busybox:tagged >/dev/null 2>&1 &&
  docker save busybox:1.36 -o "$WORK/bb.tar" &&
  docker rmi busybox:1.36 >/dev/null && docker load -i "$WORK/bb.tar" | grep -q 'Loaded image: busybox:1.36' &&
  eq "ok" "$(docker run --rm busybox:1.36 echo ok)"
}

case_events() {
  local f="$WORK/events.json"
  docker events --format '{{.Action}}' --filter type=container >"$f" &
  local pid=$!
  sleep 1
  docker run --rm alpine:3.20 true
  sleep 1
  kill $pid 2>/dev/null
  grep -q '^start$' "$f" && grep -q '^die$' "$f" && grep -q '^destroy$' "$f"
}

# --- builds and compose ---------------------------------------------------

case_build_classic() {
  local d="$WORK/build"; cp -r "$HERE/build-multistage" "$d"
  DOCKER_BUILDKIT=0 docker build -t compat-ms:1 --build-arg GREETING=hi "$d" &&
  local out && out=$(docker run --rm compat-ms:1) &&
  echo "$out" | grep -q '^built hi for' &&
  eq "1000" "$(echo "$out" | tail -1)" &&
  eq "$(docker image inspect -f '{{.Id}}' compat-ms:1)" "$(DOCKER_BUILDKIT=0 docker build -q -t compat-ms:1 --build-arg GREETING=hi "$d" 2>/dev/null)"
  local rc=$?
  docker rmi compat-ms:1 >/dev/null 2>&1
  return $rc
}

case_compose_basic() {
  local d="$WORK/compose-basic"; cp -r "$HERE/compose-basic" "$d"
  (cd "$d" && docker compose up -d 2>&1 | tail -3) &&
  for _ in $(seq 1 60); do docker compose --project-directory "$d" logs client 2>/dev/null | grep -q hello-from-env-file && break; sleep 0.5; done &&
  docker compose --project-directory "$d" logs client | grep -q '42' &&
  docker compose --project-directory "$d" logs client | grep -q 'alias-ok' &&
  curl -sf localhost:18080 | grep -q 'litek8s compose' &&
  docker compose --project-directory "$d" restart db &&
  docker compose --project-directory "$d" exec -T db pg_isready -U postgres
  local rc=$?
  (cd "$d" && docker compose down -v >/dev/null 2>&1)
  return $rc
}

case_compose_build() {
  local d="$WORK/compose-build"; cp -r "$HERE/compose-build" "$d"
  (cd "$d" && COMPOSE_BAKE=false DOCKER_BUILDKIT=0 docker compose up -d --build 2>&1 | tail -3) &&
  for _ in $(seq 1 60); do curl -sf localhost:18081 >/dev/null && break; sleep 0.5; done &&
  curl -sf localhost:18081 | grep -Eq 'version=1.2.3 hits=[0-9]+ vendor=from-image'
  local rc=$?
  (cd "$d" && docker compose down -v --rmi local >/dev/null 2>&1)
  return $rc
}

# --- Testcontainers and Dev Containers -------------------------------------

case_testcontainers() {
  command -v python3 >/dev/null || return 0
  local venv="${COMPAT_TC_VENV:-$WORK/venv}"
  [[ -x "$venv/bin/pytest" ]] || { python3 -m venv "$venv" && "$venv/bin/pip" install -q "testcontainers[postgres,redis]" psycopg2-binary redis pytest; } || return 1
  "$venv/bin/pytest" -q -p no:warnings "$HERE/testcontainers/test_tc.py"
}

case_devcontainer() {
  command -v npx >/dev/null || return 0
  local d="$WORK/devcontainer"; cp -r "$HERE/devcontainer" "$d"
  (cd "$d" && git init -q . 2>/dev/null; npx -y @devcontainers/cli@latest up --workspace-folder "$d" | tail -1 | grep -q '"outcome":"success"') &&
  eq "1000 devcontainer" "$(npx -y @devcontainers/cli@latest exec --workspace-folder "$d" sh -c 'echo $(id -u) $LITEK8S_TEST' | tail -1)" &&
  npx -y @devcontainers/cli@latest exec --workspace-folder "$d" sh -c 'touch made-in-container' &&
  [[ "$(stat -c %u "$d/made-in-container" 2>/dev/null || stat -f %u "$d/made-in-container")" == "$(id -u)" ]]
  local rc=$?
  docker rm -f $(docker ps -aq --filter "label=devcontainer.local_folder=$d") >/dev/null 2>&1
  return $rc
}

ALL=(run_attach tty detach_logs_exec publish_all cp_archive networks_volumes bind_mount restart_policy healthcheck images events build_classic compose_basic compose_build testcontainers devcontainer)
CASES=("${@:-${ALL[@]}}")
docker version --format 'server: {{.Server.Version}} ({{.Server.Os}}/{{.Server.Arch}})' || exit 1
docker pull -q alpine:3.20 >/dev/null && docker pull -q nginx:alpine >/dev/null
for c in "${CASES[@]}"; do check "$c" "case_$c"; done
echo "passed $PASS / $((PASS + FAIL))${FAILED:+; failed: ${FAILED[*]}}"
[[ $FAIL -eq 0 ]]
