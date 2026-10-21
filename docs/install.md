# Install

macOS (Apple silicon) and Linux (x86_64, aarch64), with Homebrew:

```bash
brew install jairoFernandez/tap/litek8s
```

or with the install script:

```bash
curl -fsSL https://raw.githubusercontent.com/jairoFernandez/litek8s/main/install.sh | sh
```

The script downloads the latest release for your OS and architecture, checks its
SHA-256, and installs `litek8s` to `~/.local/bin` (on macOS also the dev
VM's guest kernel and root image to `~/.local/share/litek8s/vm`). Options:
`LITEK8S_VERSION=v0.1.0`, `LITEK8S_INSTALL_DIR=/usr/local/bin`. Or
download an archive from the [releases page](https://github.com/jairoFernandez/litek8s/releases)
and check it against `SHA256SUMS`.

On Linux, dev clusters run rootless and need `crun`, `uidmap`, `passt`
(or `slirp4netns`), `iproute2` and `iptables`: `litek8s dev setup` checks
everything and says what is missing ([dev.md](dev.md#what-it-needs)).
macOS needs nothing else: no Docker Desktop, no Lima, no sudo.

Running it as a service on Linux servers (`litek8s install server|agent`,
also over SSH) is in [clusters.md](clusters.md); private registries and
mirrors for their nodes (`--registries registries.yaml`) in
[registries.md](registries.md).

Shell completion: Homebrew installs it; otherwise `litek8s completion zsh`
(or bash, fish, powershell, elvish) prints the script, e.g. `source <(litek8s
completion zsh)` in `~/.zshrc`.

From source: `cargo install litek8s` (crates.io) or `cargo install --git
https://github.com/jairoFernandez/litek8s` (Rust ≥ 1.90; macOS also needs the
guest, see [dev.md](dev.md)).
