# Platforms

| | dev clusters | server / node (`apiserver`, `agent`) | release binary |
|---|---|---|---|
| **macOS, Apple silicon** (13+) | yes, in a VM (Virtualization.framework) | for development only (Docker runtime) | `darwin-arm64`, VM guest included |
| **Linux x86_64 / aarch64, rootless** | yes, user-namespace sandbox (kernel ≥ 5.11, cgroup v2) | – | `linux-x86_64`, `linux-aarch64` (static) |
| **Linux, root with crun** | yes | yes: the intended server and node setup | same |
| macOS Intel, Windows | no (WSL2 untested) | no | – |

Tested on Ubuntu 24.04 (arm64 and x86_64) and recent macOS on Apple
silicon. Ubuntu ≥ 23.10 needs `sudo "$(command -v litek8s)" dev setup` once
(AppArmor and unprivileged user namespaces).
