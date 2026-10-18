Full run of the 101-case corpus against `litek8s dev` (rootless) inside an
Ubuntu 24.04 arm64 VM (4 vCPU, 4 GiB), run from inside the VM with Helm 4.3.0.
The machine stalled overnight during L4 (timeouts of hours, chart repos
unreachable) and the 11 GiB guest disk filled up with chart images (jellyfin
and home-assistant need 2 GiB free), so the cases that did not pass were rerun
(`lx-corpus-rerun*.log`) after clearing the image store; results.jsonl merges
them, keeping the first run's status in `detail`. The per-case logs of these
runs were lost (they lived in the VM's synced source tree); the runner output
is in `lx-corpus*.log`.
