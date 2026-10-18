Raw output of bench/dev-bench.sh (linux.jsonl: one Ubuntu 24.04 arm64 qemu
guest of 4 vCPU / 4 GiB per tool) and bench/dev-bench-mac.sh (macos.jsonl:
M3 Max, kind/k3d on OrbStack as installed). Docker engine overhead on the
kind/k3d guest with no cluster: dockerd 53 MiB (44 anon), containerd 168 MiB
(29 anon). See the "Local dev cluster" section of bench/RESULTS.md.
