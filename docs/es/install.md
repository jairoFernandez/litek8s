# Instalación

macOS (Apple silicon) y Linux (x86_64, aarch64), con Homebrew:

```bash
brew install jairoFernandez/tap/litek8s
```

o con el script de instalación:

```bash
curl -fsSL https://raw.githubusercontent.com/jairoFernandez/litek8s/main/install.sh | sh
```

El script descarga el último release para tu sistema operativo y arquitectura,
verifica su SHA-256 e instala `litek8s` en `~/.local/bin` (en macOS también el
kernel guest y la imagen raíz de la VM de desarrollo en
`~/.local/share/litek8s/vm`). Opciones: `LITEK8S_VERSION=v0.1.0`,
`LITEK8S_INSTALL_DIR=/usr/local/bin`. O descarga un archivo desde la
[página de releases](https://github.com/jairoFernandez/litek8s/releases) y
verifícalo contra `SHA256SUMS`.

En Linux, los clusters de desarrollo corren rootless y necesitan `crun`,
`uidmap`, `passt` (o `slirp4netns`), `iproute2` e `iptables`: `litek8s dev setup`
lo revisa todo y dice qué falta ([dev.md](../dev.md#qué-necesita)), y
`sudo "$(command -v litek8s)" dev setup` instala los paquetes que faltan (la
ruta completa porque el PATH de sudo deja fuera `~/.local/bin`).
macOS no necesita nada más: ni Docker Desktop, ni Lima, ni sudo.

Correrlo como servicio en servidores Linux (`litek8s install server|agent`,
también por SSH) está en [clusters.md](../clusters.md); registries privados y
mirrors para sus nodos (`--registries registries.yaml`), en
[registries.md](../registries.md).

Autocompletado del shell: Homebrew lo instala; si no, `litek8s completion zsh`
(o bash, fish, powershell, elvish) imprime el script, p. ej. `source <(litek8s
completion zsh)` en `~/.zshrc`.

Desde el código fuente: `cargo install litek8s` (crates.io) o `cargo install --git
https://github.com/jairoFernandez/litek8s` (Rust ≥ 1.90; macOS también necesita
el guest, ver [dev.md](../dev.md)).
