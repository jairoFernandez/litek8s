The k3s reference (`compat/baseline-k3d.sh`: k3s v1.33.4+k3s1 via k3d
v5.8.3, 1 server + 1 agent + k3d's load balancer) on litek8s's Docker API
instead of OrbStack: `DOCKER_HOST=unix://$HOME/.litek8s/docker.sock`, the
k3d nodes as privileged containers (nested containerd) in the macOS dev VM
(`--cpus 4 --memory 4096`, guest at 21132e1). Run in two parts on the same
cluster: L0-L3 here (263 s), L4 in `l4/` (36 min); results.jsonl has both.
Every case passes, as on k3d under OrbStack.
