litek8s {{VERSION}}: a lightweight Kubernetes in one binary (the real API) for local development, home labs and the edge. Early preview: see the README's Limitations, especially that the API has no client authentication yet.

## Install

```bash
curl -fsSL https://raw.githubusercontent.com/jairoFernandez/litek8s/main/install.sh | LITEK8S_VERSION={{VERSION}} sh
```

or download the archive for your platform below, check it against `SHA256SUMS` (`shasum -a 256 -c SHA256SUMS --ignore-missing`) and its signed build provenance (`gh attestation verify <archive> --repo jairoFernandez/litek8s`, or `LITEK8S_VERIFY=1` with install.sh), and put `bin/litek8s` on your PATH (on macOS keep `share/litek8s/vm` next to it: `<prefix>/bin/litek8s` + `<prefix>/share/litek8s/vm`).

| archive | for |
|---|---|
| `litek8s-{{VERSION}}-darwin-arm64.tar.gz` | macOS 13+ on Apple silicon; includes the dev VM's guest (Alpine kernel and userland, see THIRD-PARTY.md) |
| `litek8s-{{VERSION}}-linux-x86_64.tar.gz` | Linux x86_64, static |
| `litek8s-{{VERSION}}-linux-aarch64.tar.gz` | Linux aarch64 (Raspberry Pi 4/5, Graviton, ...), static |
| `litek8s-{{VERSION}}.cdx.json` | CycloneDX SBOM of the Rust crates in the binary (also attested on each archive) |

macOS: the binary is ad-hoc signed (not notarized). Downloaded with curl or install.sh it just runs; if you downloaded the archive with a browser, clear the quarantine flag first: `xattr -dr com.apple.quarantine litek8s-{{VERSION}}-darwin-arm64`.

## Quickstart

```bash
litek8s dev up
kubectl get nodes
litek8s ui
```

## Changes
