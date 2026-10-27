# Changelog

## 0.2.10

- **`docker run -d` and pod starts return at once again when litek8s
  inherited `NOTIFY_SOCKET`** (started from a systemd notify unit, or from a
  shell or agent tool that has it): `crun start` waited for the container's
  READY=1, so the call lasted as long as the container, a server's
  forever, and `docker compose up -d` never returned. crun no longer gets
  the variable, as with containerd and Docker.
- `litek8s dev delete` or `dev down` of the last running cluster no longer
  ends with "the dev environment closed the connection": the reply reaches
  the client before the environment stops.
- Docs and skill: name the Docker socket on every command
  (`DOCKER_HOST=unix://... docker ...`); an `export` in one agent shell call
  does not reach the next, which then talks to the machine's own Docker.

## 0.2.9

- **Argo CD Applications in `litek8s ui` and `litek8s mcp`, through the
  Kubernetes API alone** (no argocd-server, no `argocd` CLI): `:apps` lists
  them with sync, health and revision; the tree of an app shows its
  resources and follows them live; `s` syncs (an `operation`, as the CLI
  writes it), `r` refreshes, terminate stops a running sync. Any controller
  that serves the Application CRD works. MCP tools
  `applications_list`, `application_status`, `application_sync`,
  `application_refresh`.
- **An app's diff with git:** `D` in the Applications views and the MCP tool
  `application_diff` show, per resource, the live object against git. They
  read it from a Secret `<app>.diff` (key `diff.json`) that the controller
  publishes; without one the view says so (Argo CD keeps its diffs in
  argocd-server: `argocd app diff`). The format is in docs/ui.md.
- **The dev VM's disk has a size to set:** `disk:` in config.yaml, `dev up
  --disk` and Settings grow the 64 GiB image in place (sparse, data kept,
  ext4 grown online). `dev status`/`dev list` and Home show its use against
  that size and the Mac's free space.
- **Home keeps Docker API containers out of the cluster's figures:** the
  meters are against the VM's or sandbox's own size, and the Docker
  socket's containers, images and volumes are in a box of their own.
- **StatefulSets and DaemonSets find their ControllerRevision again**
  (`creationTimestamp: null` in its data was dropped, so every reconcile
  tried to create it and rollbacks did not renumber it).
- Jobs: a work queue without completions starts no new pod once one
  succeeded. CronJobs: `?` in a day field matches every day and a stepped
  `*` restricts, as robfig/cron; after many missed runs the latest starts.
  Admission: DELETE reviews carry DeleteOptions; conversion webhooks at an
  IPv6 address.

## 0.2.8

- **The platform example's waves no longer stall:** an Application whose
  first sync failed (its CRDs not there yet) was never retried until the
  next commit, and the app of apps waited on it with every later wave.
  Applications now retry without end, and a wave waits until its
  Applications are synced as well as healthy. The README says how much
  memory the platform needs.
- **`litek8s ui` shows a remote cluster as a whole or node by node:** Home
  adds up every node's CPU, memory and disk (it showed only the API
  server's node), lists the nodes with their figures, and `n`/`N` switch to
  one node's breakdown and back. Each node measures its machine only when
  asked, while Home is on screen. The disk's free space is the nodes', not
  this machine's.
- **Connected to a remote litek8s, `litek8s ui` leaves out this machine's
  Docker tabs** (Containers, Images, Volumes, Free space): they showed the
  local engine, not the cluster's. They come back with a dev cluster; the
  other tabs keep their numbers.
- **Pods keep the Docker API across server restarts:** the Docker engine
  also listens on `/run/litek8s/docker-api/docker.sock`, alone in its
  directory. A pod that mounted the socket file (hostPath
  `/run/litek8s/docker.sock`) kept the old, deleted socket after the server
  restarted ("Cannot connect to the Docker daemon") until the pod itself
  restarted; mount the directory instead and set `DOCKER_HOST`. The old
  path is still served. The platform example's runners do so.

## 0.2.7

- **Pods reach the API server on 443 again when the node leaves 443 to
  LoadBalancer Services:** since 0.2.6 the API server opens no port 443 for
  pods on such nodes, but `kubernetes.default.svc` resolves to the API
  server's own address, so clients that use its port 443 (Argo CD's
  `https://kubernetes.default.svc`, pods created before the change) were
  refused. The service proxy now sends that address's port 443 to the port
  the API server listens on.

## 0.2.6

- **Servers leave port 443 to LoadBalancer Services:** `litek8s apiserver`
  also listened on 443 on every address (`--in-cluster-port`), which took
  the port an ingress controller's LoadBalancer Service wants on that
  machine (the request reached the API server: 404). When the node listens
  on LoadBalancer ports (the default since 0.2.5), the API server now opens
  no extra port and pods reach it through the `kubernetes` Service's
  ClusterIP (on 6443); `--in-cluster-port 443` keeps the old behavior.
- **`--docker-socket` starts when a pod got there first:** a pod that
  mounts the socket's path (hostPath) before the server creates it leaves
  an empty directory there, and the server then failed to start ("Address
  in use") until someone removed it. It now removes an empty directory or a
  stale socket at that path, and says so when a non-empty directory is in
  the way.
- **Cluster DNS resolves outside names on hosts that list an IPv6
  nameserver first:** it forwarded only to the host's first nameserver and
  over IPv4, so on such hosts (Debian images on some clouds) every name
  outside the cluster timed out in pods (Helm charts, image registries). It
  now tries each nameserver, IPv4 first, over the matching address family.

## 0.2.5

- **LoadBalancer Services on servers and nodes:** every machine now listens
  on a LoadBalancer Service's own ports (80, 443), not only on its
  NodePorts, like k3s's servicelb: an ingress controller is reachable on a
  cloud VM's public address. A port the machine already uses stays with its
  owner (logged); `--disable-loadbalancer-ports` on `apiserver` / `agent`
  keeps only the NodePorts. Dev clusters are unchanged.

## 0.2.4

- **`litek8s ui --context <a remote litek8s>`:** Home's memory is the whole
  node (litek8s, pods, the operating system, cache) instead of litek8s's few
  MB against the node's RAM; the CPU and memory graphs fill instead of
  staying at "collecting…"; the CPU row reads `litek8s`, not `litek8s (dev)`.
  `u`, `d` and `R`, which act on this machine's dev clusters, are refused
  there, the header names the context (`lab2 · remote`, not `dev`), and the
  Docker views say they read this machine's dev cluster instead of showing
  its socket's error in red. Clusters starts on that context's row.
- **`litek8s ui`:** `c` copies Home's API address and the selected
  context's name in Clusters, as the Kubernetes view already did for names
  and YAML.

## 0.2.3

- **Cluster DNS on multi-node clusters:** a node that joined while the API
  server was still starting was told there was no cluster DNS, and its pods
  resolved Service names on the Internet (e.g. `web` → 127.0.53.53). The
  server now welcomes agents only once DNS and the overlay are ready, and
  ClusterFirst pods get the cluster DNS alone, never the node's resolvers.
- **`litek8s ui`:** the Home breakdown bars agree with their numbers. Each
  bar is its row's value against a scale the box names (CPU: 1 CPU; memory:
  the VM's or sandbox's limit; disk: the total), not against the largest
  row; tiny values read `<0.1%` with at most a hairline and zero has no bar.
- **Private registries and mirrors:** a registry configuration in k3s's
  `registries.yaml` format (a k3s file works as is): mirrors per registry or
  `"*"` (tried in order before the registry, with rewrites), and per host a
  CA (`ca_file`, or `ca` inline), a client certificate,
  `insecure_skip_verify`, credentials (`username`/`password`, `auth`,
  `identitytoken`) and plain HTTP. Pod pulls and the Docker API's pulls use
  it. `litek8s apiserver` / `agent` read `/etc/litek8s/registries.yaml` (or
  `--registry-config FILE`) at start; `litek8s install ... --registries FILE`
  (also over `--ssh`) and `registries:` in cluster files install it;
  `litek8s dev up` gives its clusters `~/.litek8s/registries.yaml`.
  [docs/registries.md](docs/registries.md). Credentials a registry refuses
  now say so instead of `registry answered 401`.

## 0.2.2

- **`litek8s ui`:** `W` in the Kubernetes view switches a cluster that is
  not litek8s between read-only and read-write (confirm by typing the
  context's name; a red WRITE badge while it lasts; "remember" adds it to
  `writableContexts` in `ui.yaml`). Observe says when a cluster is not
  litek8s instead of showing empty charts.
- The API server publishes the OpenAPI definitions of the built-in types
  (Kubernetes 1.33) next to the CRDs' schemas, in `/openapi/v2` (JSON and
  protobuf) and `/openapi/v3`. Argo CD's `ServerSideApply=true` and
  `ServerSideDiff=true` work (they failed with `unable to resolve
  parseableType for GroupVersionKind`), and `kubectl explain` shows built-in
  types. Compat corpus case `argocd_ssa` (L4).
- `examples/platform/`: a developer platform on one dev cluster (Gitea with
  Actions, Argo CD GitOps, Argo Workflows, Atlantis with OpenTofu, OpenBao
  with external-secrets, a sample app deployed on push).
- Docs: [litek8s next to the Kubernetes you already run](docs/alongside.md);
  [CI](docs/ci.md) measured against kind, the runner's dockerd and Docker
  in Docker; memory re-measured on v0.2.1 (the whole control plane idles at
  23.2 MiB, k3s minimal 567 MiB).

## 0.2.1

- **CI:** a GitHub Action, `jairoFernandez/litek8s/.github/actions/setup-litek8s`,
  gives a job a cluster and a Docker API (replacing `helm/kind-action` and
  the runner's dockerd), with Helm, compose and multi-node examples in
  `examples/github-actions/` and a script for other CI systems
  (`examples/ci/litek8s-ci.sh`). Measurements in [docs/ci.md](docs/ci.md).
- Image pulls no longer hang when a connection to a registry stalls: each
  address gets 10 s to connect and 15 s for the TLS handshake before the
  next one is tried, and small responses must arrive within a minute. On
  GitHub's runners pasta dropped such flows, leaving pods in
  `ContainerCreating` for good.
- `LITEK8S_DEV_NETWORK=slirp4netns` makes `litek8s dev` use slirp4netns
  even where pasta is installed.
- build.rs no longer makes every build rerun it (and rebuild the crate) in a
  checkout without `.git/packed-refs`.

## 0.2.0

litek8s is now only the Kubernetes distribution. The original Docker-backed
daemon (`litek8s serve`, `litek8s.dev/v1alpha1` manifests, SSH nodes, its
own Raft and WireGuard hub) and the experimental distributed LLM compute are
gone: about 25,000 lines of Rust, all with Kubernetes-native replacements that
shipped before this release.

### Removed commands and their replacements

| removed | use instead |
|---|---|
| `litek8s serve` | `litek8s dev up` for a local cluster, `litek8s apiserver` (or `litek8s install server`) for a real one |
| `litek8s apply`, `get`, `describe`, `delete` | `kubectl apply/get/describe/delete` (standard Deployments, Services, ...) against that cluster |
| `litek8s events` | `kubectl get events` (or `kubectl events`) |
| `litek8s proxy` | `kubectl port-forward`; NodePorts appear on `localhost` with `litek8s dev` |
| `litek8s service add` | a Kubernetes Service (`kubectl expose deployment ...`) |
| `litek8s node add` | `litek8s install agent --ssh user@host --join ... --token ...` ([docs/clusters.md](docs/clusters.md)) |
| `litek8s node cordon/drain/uncordon` | `kubectl cordon/drain/uncordon` |
| `litek8s control-plane join/status` | `litek8s apiserver --cluster-init` / `--server URL --token T` (or `install server` with those flags), `litek8s server list/remove`, `litek8s lb` ([docs/ha.md](docs/ha.md)) |
| `litek8s vpn ...` | the pod overlay every node joins on its own, with relaying for nodes behind NAT ([docs/networking.md](docs/networking.md)) |
| `litek8s infra init` | provision machines with your own tooling (Terraform, cloud-init running `install.sh server\|agent ...`), then `litek8s cluster apply -f cluster.yaml` |
| `litek8s cluster` (Docker-container clusters, removed earlier) | `litek8s cluster apply -f` for machines over SSH, `litek8s dev up --nodes N` for local multi-node clusters |
| `litek8s llm`, `model`, `tensor`, `pipeline` | llama.cpp's RPC backend as a DaemonSet: [examples/llm-rpc](examples/llm-rpc/README.md), [docs/llm-cluster.md](docs/llm-cluster.md) |
| `litek8s agent` without `--join` (the daemon's SSH node agent) | `litek8s agent --join URL --token T` (`--join` is now required) |
| `litek8s logs <deployment>` of the daemon | `litek8s logs <pod regex>` or `kubectl logs` |

### Other changes

- `litek8s top` and `litek8s tree` always show the current kube context
  (or `--context`); they no longer look for a daemon first.
- The global `--socket-path` and `--server` flags are gone (`agent`, `lb`
  and `server` keep their own `--server`).
- The `llm` cargo feature is gone and with it the memmap2, safetensors,
  tokenizers (oniguruma) and half dependencies; the crate has no features
  left, so `--no-default-features` is no longer needed for the release build.
- The release binary is 17 % smaller: 32.2 MiB instead of 38.7 MiB
  (macOS arm64, stripped).
- `cargo test` no longer needs a Docker API (the Docker-driven tests were
  the daemon's).
- The daemon's files can be deleted by hand: `state.json`, `vpn/` and
  `weights/` in the data directory (`~/.local/share/litek8s` on Linux,
  `~/Library/Application Support/dev.litek8s` on macOS) and
  `$XDG_RUNTIME_DIR/litek8s/{daemon,agent}.sock`. Nothing else litek8s
  keeps is affected.
- Removed docs: docs/legacy-daemon.md, docs/legacy-daemon-demo.md,
  docs/llm.md; the v1alpha1 examples and the Terraform scaffolding under
  `infra/`.

## 0.1.2

See the [GitHub release](https://github.com/jairoFernandez/litek8s/releases/tag/v0.1.2).
