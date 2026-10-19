# Kubernetes compatibility corpus

The goal is "at least 90 % compatible for most real use". This corpus defines
that number. Each case is a kubectl or Helm workflow that a real user or chart
depends on. The score is the share of cases that pass against a cluster.

Every case must pass on a reference cluster (k3s through k3d). A case that
fails there is a bug in the case, not a compatibility gap.

## Tiers

| tier | what it covers | needs |
|---|---|---|
| L0 | discovery: `/version`, `/healthz`, `api-versions`, `api-resources`, `/openapi/v3` | API server |
| L1 | generic object semantics on config objects: create/get/list/watch, apply (client- and server-side), merge/JSON/strategic patch, labels and field selectors, conflicts, `generateName`, dry-run, namespace cascade, CRDs, table output | API server |
| L2 | workloads actually running: Deployment rollout/scale/undo, logs, exec, port-forward, Job, CronJob, StatefulSet, DaemonSet, multi-container pods, ConfigMap/Secret injection, probes, restarts, PVCs | controllers + node agent |
| L3 | networking: ClusterIP, DNS, EndpointSlices, headless and NodePort Services, pod-to-pod, in-cluster API access | service proxy, DNS, CNI |
| L4 | 47 popular Helm charts install with `--wait` and uninstall cleanly (`charts.txt`) | everything above |

The overall rate is unweighted (passed / total). L4 is the closest thing to
"does my stuff run": charts exercise CRDs, webhooks, RBAC, probes and
StatefulSets all at once.

## Running

```bash
compat/run.sh                                   # all tiers, current kubeconfig
compat/run.sh --tiers L0,L1                     # some tiers
compat/run.sh --only cm_create_get,patch_json   # specific cases
compat/run.sh --max-case-seconds 45             # cap timeouts for a quick score
COMPAT_HELM_TIMEOUT=2m compat/run.sh --tiers L4 # shorter helm --wait
compat/baseline-k3d.sh                          # reference run on k3s (k3d), any Docker API incl. litek8s's
compat/baseline-k3s-vm.sh                       # reference run on k3s in a qemu VM, no Docker at all
```

Without Docker or OrbStack (litek8s's own cluster and Docker API): see
[docs/develop-without-docker.md](../docs/develop-without-docker.md).

Against the litek8s API server:

```bash
cargo run --release -- apiserver --kubeconfig /tmp/litek8s.kubeconfig &
KUBECONFIG=/tmp/litek8s.kubeconfig compat/run.sh --target litek8s
```

Results go to `compat/results/<timestamp>-<target>/` (`summary.md`,
`results.jsonl`, one log per case). Each case runs in its own namespace
`compat-<case>`, which is deleted afterwards.

Requires bash, kubectl, perl (for timeouts) and python3. L2–L4 also need
images pulled from Docker Hub and charts from their repos, and L4 needs helm.

## Adding cases

Add a `case_def <id> <tier> <timeout-secs> "<description>"` and a `t_<id>`
function to the matching `cases/L*.sh` file (bash 3.2 compatible: no
associative arrays). The function runs with `set -e`; any failing command fails
the case, and the last line it prints to stderr becomes the case's detail. Use
`use_ns` for a scratch namespace, and `k` instead of `kubectl` (60 s timeout).
Then run the case on k3d before counting it.
