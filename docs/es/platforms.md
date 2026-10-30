# Plataformas

| | clusters de desarrollo | servidor / nodo (`apiserver`, `agent`) | binario de release |
|---|---|---|---|
| **macOS, Apple silicon** (13+) | sí, en una VM (Virtualization.framework) | solo para desarrollo (runtime de Docker) | `darwin-arm64`, con el guest de la VM incluido |
| **Linux x86_64 / aarch64, rootless** | sí, sandbox con user namespaces (kernel ≥ 5.11, cgroup v2) | – | `linux-x86_64`, `linux-aarch64` (estáticos) |
| **Linux, root con crun** | sí | sí: la configuración prevista para servidores y nodos | los mismos |
| macOS Intel, Windows | no (WSL2 sin probar) | no | – |

Probado en Ubuntu 24.04 (arm64 y x86_64) y en macOS reciente sobre Apple
silicon. Ubuntu ≥ 23.10 necesita `sudo "$(command -v litek8s)" dev setup` una
vez (AppArmor y user namespaces sin privilegios).
