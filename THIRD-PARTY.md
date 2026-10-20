# Third-party software

litek8s itself is Apache-2.0 (see [LICENSE](LICENSE) and [NOTICE](NOTICE)).

## Rust crates

The binary statically links the crates in `Cargo.lock`. They are under
permissive licenses (MIT, Apache-2.0, BSD, ISC, Zlib, Unicode-3.0 and
similar), with two exceptions, both used unmodified: `option-ext`
(MPL-2.0, file-level copyleft; via `directories`) and `webpki-roots`
(CDLA-Permissive-2.0, Mozilla's root certificates). To list them:

```bash
cargo install cargo-license
cargo license --avoid-build-deps
```

## Vendored files

- `proto/k8s.io/`: Kubernetes `generated.proto` files, Apache-2.0,
  Copyright The Kubernetes Authors ([proto/README.md](proto/README.md)).
- `src/kube/cel/cost.rs`: ported from cel-go's `checker/cost.go`,
  Apache-2.0, Copyright Google LLC, with the Kubernetes CEL library costs,
  Apache-2.0, Copyright The Kubernetes Authors.

## The macOS dev VM guest

The macOS release archive includes `vm/`: the Linux guest `litek8s dev up`
boots. Everything in it except `/init` (the litek8s binary, Apache-2.0) is
an unmodified binary from Alpine Linux's package repository
(`dl-cdn.alpinelinux.org`), built by `dev/vm/build-guest.py`: the
`linux-virt` kernel and modules (GPL-2.0), busybox (GPL-2.0), crun
(GPL-2.0-or-later), iptables (GPL-2.0), iproute2 (GPL-2.0), e2fsprogs
(GPL-2.0 / LGPL-2.0 / BSD) and their libraries (musl: MIT; others as
listed).

`vm/PACKAGES.txt`, generated with the guest, lists every package with its
exact version, license and the Alpine `aports` commit that builds it, e.g.

```
crun 1.21-r0 GPL-2.0-or-later https://gitlab.alpinelinux.org/alpine/aports/-/tree/<commit>/community/crun
```

The complete corresponding source for each package is that `APKBUILD` and
its patches at that commit, plus the upstream source archive the `APKBUILD`
downloads (mirrored by Alpine at `https://distfiles.alpinelinux.org/distfiles/`).

Written offer: for three years from the date of a release that contains
`vm/`, the source code of any of these GPL-licensed packages, as included in
that release, is available on request from the contact in
[SECURITY.md](SECURITY.md) (or an issue on this repository), for no more
than the cost of providing it.
