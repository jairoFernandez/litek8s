`litek8s dev` on macOS, run on 2026-10-19 for docs/develop-without-docker.md:
the cluster in its Virtualization.framework VM (`--cpus 4 --memory 4096`),
host binary at main 6172595 and the guest built from 21132e1 (main + the
legacy-runtime probe fix and the Docker API filter/privileged-device
fixes), the guest built by litek8s's own Docker API. Corpus run from the
Mac against 127.0.0.1 (API over vsock), Helm with a private cache, 119
cases, 37 min wall time while the Mac was loaded (load average 60-100
from other work for most of the run).

`helm_metrics_server` failed in the first run because the cluster already
had a `metrics-server` release in kube-system from earlier manual testing
(ClusterRole ownership conflict). After uninstalling it, the case was rerun
on the same cluster: `rerun1/` (pass). results.jsonl merges it, keeping the
first run's status in `detail`.
