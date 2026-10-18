`litek8s dev` on macOS: the cluster in its Virtualization.framework VM
(4 vCPU, 4 GiB), corpus run from the Mac against 127.0.0.1 (API over
vsock), Helm 4.3.0. The Mac slept during the first run's L4 (timeouts,
unreachable repos) and later its disk filled up (100 %), which gave the VM's
sparse disk I/O errors, so the cases that did not pass were rerun on a
fresh VM: `rerun1/`, `rerun2/` (all passed). results.jsonl merges them,
keeping the first run's status in `detail`.
