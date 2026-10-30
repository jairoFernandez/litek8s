# Cómo contribuir a litek8s

Gracias por probar litek8s. Los reportes de bugs, las brechas de compatibilidad
("este chart / comando de kubectl funciona en k3s pero no aquí") y los arreglos
pequeños son las contribuciones más útiles en este momento. Para cualquier cosa
más grande, abre primero un issue para que acordemos el enfoque.

Al contribuir aceptas que tu contribución se licencia bajo la Apache License 2.0
(la licencia del proyecto, ver [LICENSE](../../LICENSE)), como lo describe la
sección 5 de la licencia.

## Usar litek8s vs. desarrollarlo

Mantén las dos cosas separadas en tu máquina. **Usa** litek8s desde un release
(`brew install jairoFernandez/tap/litek8s`, install.sh o `cargo install
litek8s`): sus clusters, `~/.litek8s/docker.sock` y los contextos
`litek8s-<cluster>` son los tuyos. **Desarróllalo** con `dev/lk`, que ejecuta el
`target/release/litek8s` de este checkout (lo recompila cuando hay un archivo
fuente más nuevo) con `LITEK8S_HOME=~/.litek8s-dev`: su propia VM de desarrollo,
clusters, socket de Docker, configuración, kubeconfig y contextos
`lkdev-<cluster>`. Un build hecho desde un checkout lo indica:
`litek8s --version` y el encabezado de la UI muestran `DEV <commit>` y el
`LITEK8S_HOME` en uso.

```bash
dev/lk build-vm                      # the macOS dev VM's guest from this checkout
dev/lk dev up && dev/lk ui           # a cluster of the checkout's build, next to the release's
eval "$(dev/lk env)"                 # its DOCKER_HOST and KUBECONFIG in this shell
```

Los detalles, el primer guest en un Mac nuevo y el `dev/stable.sh` fijado, en
[docs/develop-without-docker.md](../develop-without-docker.md#usar-litek8s-vs-desarrollarlo).

## Compilar

Rust stable (≥ 1.90; el crate usa la edición 2024):

```bash
cargo build                          # debug
cargo build --release                # what the releases ship
```

En macOS, `litek8s dev up` también necesita el guest de la VM de desarrollo (un
build estático de litek8s para Linux más un kernel y un userland de Alpine):

```bash
dev/lk build-vm                      # needs a Docker API for the musl build
```

Para eso no necesitas Docker ni OrbStack: un litek8s fijado puede proveer la
API de Docker ([docs/develop-without-docker.md](../develop-without-docker.md)).

## Pruebas

```bash
cargo test                           # unit and API tests, no Docker needed
cargo nextest run                    # the same, in parallel processes, as CI runs them
cargo test discovery_and_default_objects        # one test
cargo clippy --all-targets -- -D warnings
cargo fmt
```

- `tests/kube_apiserver.rs`, `tests/mcp.rs`, `tests/obs_api.rs` y
  `tests/ha.rs` corren el API server dentro del proceso y no necesitan Docker.
- Las pruebas unitarias viven junto al código (`#[cfg(test)]`). La interfaz de
  terminal se prueba con el TestBackend de ratatui a 80x24 (`src/ui/tests.rs`).
- El código del runtime crun y del sandbox rootless solo corre en Linux;
  `bench/vm.sh` arranca una VM de Ubuntu para probarlo desde un Mac.

## Corpus de compatibilidad

La vara para medir si "se comporta como Kubernetes" es el corpus de
[compat/](../../compat/README.md): flujos de kubectl y de Helm en los niveles
L0 a L4. Un cambio en el API server, los controladores o el agente de nodo debe
mantenerlo al 100 %:

```bash
cargo run --release -- apiserver --kubeconfig /tmp/litek8s.kubeconfig &
KUBECONFIG=/tmp/litek8s.kubeconfig compat/run.sh --tiers L0,L1   # quick, API only
KUBECONFIG=/tmp/litek8s.kubeconfig compat/run.sh                 # everything (needs helm, ~15 min)
compat/baseline-k3d.sh                                           # the same cases on k3s
```

Necesita kubectl, perl y python3 (y helm para L4). Un caso nuevo debe pasar en
k3s (k3d) antes de que cuente. La suite oficial de conformidad corre con
`compat/conformance.sh` (~30 min, ver [compat/CONFORMANCE.md](../../compat/CONFORMANCE.md)).

## Pull requests

- Limita cada PR a un solo cambio, con pruebas donde el código las tenga.
- `cargo fmt`, `cargo clippy --all-targets -- -D warnings` y `cargo test`
  pasan; CI los corre en Linux y macOS, además de los niveles L0/L1 del corpus
  y los ejemplos de GitHub Actions en Linux
  ([docs/ci.md](../ci.md#ci-del-propio-litek8s)).
- Las afirmaciones sobre rendimiento vienen con números y con cómo se midieron;
  los harnesses están en [bench/](../../bench/).
- Los comentarios y la documentación dicen qué hace el código y por qué, en
  oraciones simples.

## Releases

Un tag `v*` que coincide con la versión de `Cargo.toml` ejecuta
[.github/workflows/release.yml](../../.github/workflows/release.yml): binarios
estáticos para Linux (x86_64, aarch64), el binario arm64 de macOS (firmado
ad-hoc con el entitlement de Virtualization) con el guest de la VM de
desarrollo, y `SHA256SUMS`, adjuntos a un release en borrador. Ejecutar el
workflow a mano compila los mismos archivos como artifacts sin publicar un
release.
