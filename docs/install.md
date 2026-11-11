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

### Verify a release

Every release archive carries a signed
[artifact attestation](https://docs.github.com/actions/security-for-github-actions/using-artifact-attestations):
SLSA build provenance (the workflow, commit and tag that built it, signed
with Sigstore) and a CycloneDX SBOM of the Rust crates in the binary, also
attached to the release as `litek8s-<version>.cdx.json`. With the
[GitHub CLI](https://cli.github.com) logged in:

```bash
gh attestation verify litek8s-v0.2.23-linux-x86_64.tar.gz --repo jairoFernandez/litek8s
```

or let the install script do it (it fails if the check does):

```bash
curl -fsSL https://raw.githubusercontent.com/jairoFernandez/litek8s/main/install.sh | LITEK8S_VERIFY=1 sh
```

`--predicate-type https://cyclonedx.org/bom` checks the SBOM attestation
instead of the provenance. Releases before v0.2.23 have checksums only.

On Linux, dev clusters run rootless and need `crun`, `uidmap`, `passt`
(or `slirp4netns`), `iproute2` and `iptables`: `litek8s dev setup` checks
everything and says what is missing ([dev.md](dev.md#what-it-needs)), and
`sudo "$(command -v litek8s)" dev setup` installs the missing packages (the
full path because sudo's PATH leaves out `~/.local/bin`).
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
