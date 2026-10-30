# litek8s en CI

> **Si tu CI necesita Kubernetes, usa litek8s.** En los runners de GitHub un
> cluster está Ready en **~2.5 s en lugar de ~45 s**, un job de Helm o
> multinodo termina **3–5× antes** y el cluster ocupa **26–37× menos memoria**
> que kind. En un contenedor privilegiado (la configuración `docker:dind` de
> GitLab) un job de Helm es **1.8× más rápido** que kind en Docker in Docker,
> listo en 9 s en lugar de 40 s. Los mismos kubectl, Helm y manifiestos: solo
> cambia el paso de configuración.

Un job de CI que necesita un cluster de Kubernetes normalmente arranca kind (un
contenedor de Docker por nodo, ~45 s en los runners de GitHub antes de que los
nodos estén Ready); uno que necesita Docker usa el dockerd del runner. litek8s
da ambos en unos 2.5 s, descarga incluida, como el usuario que ejecuta el job:
`litek8s dev up` es el mismo sandbox rootless que en una laptop (API server,
controllers, scheduler, DNS del cluster, agentes de nodo sobre crun, una Docker
Engine API), así que kubectl, Helm, `docker`, `docker compose` y Testcontainers
funcionan sin cambios.

## Cuándo vale la pena

| el job necesita | en lugar de | qué cambia | veredicto |
|---|---|---|---|
| **un cluster de Kubernetes** (kubectl, Helm, operadores, e2e) en los runners de GitHub | kind, k3d (contenedores de Docker como nodos) | listo en ~2.5 s en lugar de ~45 s; **jobs 3–5× más rápidos, 26–37× menos memoria** | **cambia** |
| **un cluster de Kubernetes** en un contenedor privilegiado (GitLab, runners en contenedores) | kind en Docker in Docker (`docker:dind`) | listo en 9 s en lugar de 40 s; **job de Helm de 39 s en lugar de 70 s** | **cambia** |
| solo Docker (build, compose, Testcontainers) en los runners de GitHub | el dockerd del runner | ~10 s menos por job de compose; dockerd no necesita configuración | ganancia pequeña |
| solo Docker en un contenedor privilegiado | Docker in Docker | lo mismo: job de compose de 40 s contra 38 s; el dockerd de dind arranca en ~1 s | sin ganancia |

Cifras: [runners de GitHub](#cifras), [en un contenedor](#en-un-contenedor-docker-in-docker).

## GitHub Actions

```yaml
jobs:
  e2e:
    runs-on: ubuntu-24.04
    steps:
      - uses: actions/checkout@v4
      - uses: jairoFernandez/litek8s/.github/actions/setup-litek8s@v0.2.1
      - run: |
          helm install hello ./chart --wait
          helm test hello
```

La action ([action.yml](../../.github/actions/setup-litek8s/action.yml))
descarga el release para la arquitectura del runner y lo verifica contra los
SHA256SUMS del release (con `install.sh`), instala lo que el cluster rootless
necesita si la imagen no lo tiene (crun, uidmap, slirp4netns; las imágenes de
Ubuntu de GitHub los traen), ejecuta `sudo litek8s dev setup` por la
restricción de AppArmor de Ubuntu sobre los user namespaces, arranca el cluster
y espera a que cada nodo esté Ready. Los pasos siguientes reciben `KUBECONFIG`
y, salvo con `docker: false`, `DOCKER_HOST` (además de `DOCKER_BUILDKIT=0` y
`COMPOSE_BAKE=false`, para que los builds usen el builder propio de litek8s).
Si falla un paso de la action, imprime `litek8s dev status`, los pods, los
eventos y los logs del cluster; `litek8s-diagnose` (en el PATH,
[diagnose.sh](../../.github/actions/setup-litek8s/diagnose.sh)) hace lo mismo
para tus propios pasos:

```yaml
      - if: failure()
        run: litek8s-diagnose
```

| input | por defecto | |
|---|---|---|
| `version` | `latest` | un tag de release, p. ej. `v0.2.1` |
| `binary` | | un binario de litek8s para usar en su lugar (compilado antes en el job) |
| `cluster-name` | `ci` | contexto de kube `litek8s-<name>` |
| `nodes` | `1` | control plane + N-1 workers, unidos como nodos reales |
| `docker` | `true` | exporta `DOCKER_HOST` para docker, compose, Testcontainers |
| `builder` | `litek8s` | `buildkit` para compilar a través del contenedor BuildKit de buildx |
| `network` | `slirp4netns` | egress de los pods; `pasta` es el valor por defecto de litek8s en escritorio (ver abajo) |
| `publish` | `localhost` | `all` para publicar NodePorts y puertos en todas las direcciones |
| `wait` | `120` | segundos de espera por los nodos |

Outputs: `version`, `context`, `kubeconfig`, `api-url`, `docker-socket`,
`docker-host`, `ready-seconds`. Solo runners de Linux (`ubuntu-*`, x86_64 y
arm64): en macOS el cluster de desarrollo es una VM, y los runners de macOS de
GitHub no permiten virtualización anidada.

Fija la action a un tag de release (`@v0.2.1` o posterior), como arriba;
`@main` sigue al repositorio.

### Ejemplos

[examples/github-actions/](../../examples/github-actions/README.md) tiene tres
workflows para copiar, cada uno ejecutado en cada commit de este repositorio
(el job `examples` de [test.yml](../../.github/workflows/test.yml), mediante
[run.sh](../../examples/github-actions/run.sh), que ejecuta en orden los pasos
`run:` de un ejemplo):

- **[helm](../../examples/github-actions/helm/workflow.yml)**: `helm lint`,
  `helm install --wait`, `helm test`, upgrade y rollback de un chart pequeño
  (un Deployment detrás de un Service con readiness probe, un ConfigMap, un pod
  de prueba que llega al Service por DNS). Reemplaza a `helm/kind-action`.
- **[compose](../../examples/github-actions/compose/workflow.yml)**:
  `docker compose up --build --wait` de una app en Python (compilada desde su
  Dockerfile) y Redis con healthchecks, probada desde el runner a través del
  puerto publicado y desde un contenedor en la red del proyecto. Reemplaza al
  dockerd del runner.
- **[multi-node](../../examples/github-actions/multi-node/workflow.yml)**: tres
  nodos; un Deployment repartido con una réplica por nodo mediante un topology
  spread constraint, un DaemonSet, un Service alcanzado desde cada nodo, el pod
  de un worker drenado que lo abandona.

## Cifras

Los mismos ejemplos en el mismo tipo de runner (`ubuntu-24.04` de GitHub,
4 vCPU, 16 GB), con litek8s v0.2.0 desde su release, contra kind
(`helm/kind-action@v1`, kind v0.33.0, su imagen de nodo por defecto) y contra
el propio dockerd del runner; [ci-bench.yml](../../.github/workflows/ci-bench.yml),
ejecutado a mano. Medianas de 5 ejecuciones ([run 39559413930](https://github.com/jairoFernandez/litek8s/actions/runs/39559413930), intentos 1 a 5); cada job arranca en frío (sin caché: las
imágenes se descargan de Docker Hub en cada job, con cada motor).

| ejemplo | motor | tiempo hasta listo | pasos del ejemplo | total del job | memoria en reposo | pico de memoria |
|---|---|---|---|---|---|---|
| helm | **litek8s** | **2.6 s** | **10.2 s** | **21 s** | **12 MiB** | **42 MiB** |
| helm | kind | 44.7 s | 22.7 s | 79 s | 449 MiB | 549 MiB |
| multi-node (3 nodos) | **litek8s** | **2.5 s** | **4.5 s** | **15 s** | **28 MiB** | **82 MiB** |
| multi-node (3 nodos) | kind | 49.7 s | 11.1 s | 72 s | 715 MiB | 827 MiB |
| compose | **litek8s** | 2.2 s | **21.0 s** | **31 s** | 13 MiB | 46 MiB |
| compose | dockerd del runner | **0 s** (ya en ejecución) | 34.0 s | 44 s | 0 MiB | **28 MiB** |

Rangos en las 5 ejecuciones: tiempo hasta listo de litek8s 2.1 a 4.1 s, kind
40.8 a 53.2 s; totales del job litek8s 13 a 38 s, kind 67 a 85 s, dockerd 37 a
49 s.

- Un job de Kubernetes es **de 3.7 a 4.8 veces más corto** (medianas de los
  totales del job), y el cluster ocupa **de 1/37 (helm) a 1/26 (multi-node) de
  la memoria de kind**: kind ejecuta un nodo completo (systemd, containerd,
  kubelet, etcd, el API server, kube-proxy, CoreDNS, kindnet) por contenedor.
- Los pasos del ejemplo en sí también son más rápidos (helm 10.2 s contra
  22.7 s, multi-node 4.5 s contra 11.1 s); estas ejecuciones no desglosan en
  qué se va el tiempo de kind (descargas de imágenes a los nodos, arranque de
  pods, reglas de Service).
- Frente al dockerd del runner, compose compila y arranca más rápido en litek8s
  (el builder de litek8s contra el de BuildKit) y el job completo es 13 s más
  corto, aunque dockerd no necesita ninguna configuración. Sus contenedores
  ocupan menos memoria que todo el sandbox de litek8s, que además contiene un
  API server y un nodo de Kubernetes; dockerd y containerd mismos no se
  cuentan.
- No comparado: el kind preinstalado en el runner (`kind create cluster` sin la
  action) y k3d; una imagen del cluster cacheada entre ejecuciones. Las cifras
  de litek8s son de v0.2.0 con los valores por defecto de la action
  (slirp4netns).

### En un contenedor (Docker in Docker)

Donde corre un job de GitLab.com: un contenedor privilegiado en el mismo
runner. Docker in Docker es la imagen `docker:27-dind` arrancando su propio
dockerd (más kind v0.33.0 dentro para helm); litek8s es
[litek8s-ci.sh](../../examples/ci/litek8s-ci.sh) en `ubuntu:24.04`, con sus
paquetes de apt y la descarga incluidos en el tiempo hasta listo. Las
herramientas propias del harness están instaladas antes de que empiece a correr
el reloj. [in-container.sh](../../.github/ci-bench/in-container.sh), el job
`in-container` de [ci-bench.yml](../../.github/workflows/ci-bench.yml),
litek8s v0.2.1; medianas de 5 ejecuciones (kind en dind: 4, un pod de
`helm test` falló dentro de kind).

| ejemplo | motor | tiempo hasta listo | pasos del ejemplo | total del job | pico de memoria del contenedor |
|---|---|---|---|---|---|
| helm | **litek8s** | **9.2 s** | **12.5 s** | **39 s** | **356 MiB** |
| helm | kind en dind | 40.0 s | 18.5 s | 70 s | 2,960 MiB |
| compose | litek8s | 5.3 s | 19.3 s | 40 s | 353 MiB |
| compose | dind | **1.0 s** | 21.0 s | **38 s** | 345 MiB |

- Con Kubernetes, litek8s está listo **4× antes** y el job de Helm es **1.8×
  más corto**; kind en dind descarga su imagen de nodo de ~1 GB y ejecuta un
  nodo completo dentro del contenedor del job.
- Solo con Docker es un empate: el dockerd de dind está arriba en
  aproximadamente un segundo, y compose corre al mismo ritmo en ambos.
- El pico de memoria es el del cgroup del contenedor del job (`memory.peak`),
  page cache incluida: la imagen de nodo de kind explica la mayor parte de sus
  2.9 GiB.

Qué miden las columnas:

- **Tiempo hasta listo**: desde el inicio del paso de configuración hasta que
  cada nodo está Ready: descarga y verificaciones incluidas para litek8s; la
  descarga de la imagen de nodo y la creación del cluster para kind (cuya
  action espera al control plane; luego el job espera a los nodos, como hace la
  action de litek8s). dockerd ya está en ejecución en el runner, así que no
  tiene nada que esperar.
- **Pasos del ejemplo**: los propios pasos `run:` del ejemplo (descargas de
  imágenes, Helm, las pruebas).
- **Total del job**: el job según lo cronometra GitHub, checkout y pasos de
  medición incluidos.
- **Memoria en reposo / pico**: `memory.current - inactive_file` del cgroup (lo
  que muestra `docker stats`), muestreada cada 200 ms desde que el cluster está
  listo hasta el final del ejemplo: para litek8s todo su sandbox (API server,
  controllers, agentes de nodo, slirp4netns, pods y contenedores de la Docker
  API); para kind y dockerd, cada contenedor de Docker (los contenedores de
  nodo de kind con todo lo que tienen dentro; los servicios de compose). El
  dockerd y el containerd del runner corren en todos los casos y no se cuentan.

No se ofrece cachear el almacén de imágenes entre ejecuciones (`actions/cache`):
las capas desempaquetadas pertenecen a los user ids subordinados del cluster,
que el usuario del runner no puede volver a leer para meterlos en un archivo de
caché, y las descargas desde Docker Hub tardaron de 0.7 a 6 s por imagen en
estos runners.

Para el CI propio de este repositorio, ver [CI del propio litek8s](#ci-del-propio-litek8s).

## Otros sistemas de CI

En cualquier máquina o VM Linux que controle un job (GitLab CI con un executor
shell o Docker privilegiado, agentes de Jenkins, Buildkite, el executor machine
de CircleCI), [examples/ci/litek8s-ci.sh](../../examples/ci/litek8s-ci.sh) hace
lo mismo que la action e imprime el entorno que hay que exportar:

```bash
eval "$(sh examples/ci/litek8s-ci.sh)"
kubectl get nodes
docker compose up -d --wait
```

O a mano:

```bash
sudo apt-get install -y crun uidmap slirp4netns
curl -fsSL https://raw.githubusercontent.com/jairoFernandez/litek8s/main/install.sh | sh
export PATH="$HOME/.local/bin:$PATH"
sudo "$(command -v litek8s)" dev setup     # Ubuntu >= 23.10: AppArmor allows litek8s user namespaces
LITEK8S_DEV_NETWORK=slirp4netns litek8s dev up --name ci
export DOCKER_HOST="unix://$HOME/.litek8s/ci/docker.sock"
```

GitLab CI (los runners de Linux de GitLab.com ejecutan los jobs en contenedores
privilegiados, que `docker:dind` también necesita; como root el cluster corre
sin user namespace). No se ejecutó en GitLab mismo: la misma configuración en
contenedor se prueba en el runner de GitHub, abajo.

```yaml
k8s-e2e:
  image: ubuntu:24.04
  script:
    - apt-get update -qq && apt-get install -y -qq curl ca-certificates
    - eval "$(sh examples/ci/litek8s-ci.sh)"
    - curl -fsSLo /usr/local/bin/kubectl "https://dl.k8s.io/release/v1.33.0/bin/linux/amd64/kubectl" && chmod +x /usr/local/bin/kubectl
    - kubectl apply -f deploy/ && kubectl rollout status deployment/myapp --timeout=2m
```

`litek8s-ci.sh` se prueba en [ci-bench.yml](../../.github/workflows/ci-bench.yml)
de ambas formas en el runner de Ubuntu de GitHub: como el usuario del runner
con sudo, y como root en un contenedor `ubuntu:24.04` privilegiado. En 5
ejecuciones de cada una, el cluster estuvo arriba y un Deployment de nginx
terminó su rollout con una mediana de tiempo de job de 26 s (usuario) y 28 s
(contenedor, apt incluido). En un contenedor `$HOME` está sobre el overlayfs
del contenedor, que no puede alojar los montajes overlay propios de litek8s
(como pasa con Docker in Docker); ahí el script pone los datos de litek8s en un
tmpfs.

## pasta, slirp4netns y los runners de GitHub

Los pods (y el agente de nodo al descargar imágenes) llegan a Internet a través
de un stack de red en espacio de usuario: pasta cuando está instalado, si no
slirp4netns. En los runners de Ubuntu de GitHub, pasta (el `passt 2024-02-20`
de Ubuntu 24.04, y también el build estático del 2026-10-02) pierde flujos TCP
de vez en cuando: la conexión parece establecida dentro del cluster, pero el
servidor nunca ve los datos. De 5 ejecuciones de cada uno que arrancaban un
cluster, descargaban dos imágenes y arrancaban un pod, 3 se colgaron con el
pasta de Ubuntu, 1 con el pasta más reciente y ninguna con slirp4netns; con el
cliente de registry corregido que se describe abajo (tres imágenes por
ejecución), pasta todavía falló 2 de 5, slirp4netns ninguna, y sus descargas
fueron igual de rápidas (0.7 a 5.9 s por imagen contra 1.7 a 5.0 s). Por eso:

- la action usa slirp4netns (`network: slirp4netns`, que define
  `LITEK8S_DEV_NETWORK=slirp4netns`; litek8s 0.2.0 todavía no lee esa
  variable, así que para esa versión la action aparta `/usr/bin/pasta` en el
  runner);
- el cliente de registry de litek8s ahora le da a cada dirección de un registry
  10 s para conectar y 15 s para el handshake TLS antes de probar la siguiente,
  y lee las respuestas pequeñas en menos de un minuto: antes, un flujo perdido
  dejaba la descarga, y el pod en `ContainerCreating`, colgados para siempre.

En escritorio pasta sigue siendo el valor por defecto; `LITEK8S_DEV_NETWORK=slirp4netns
litek8s dev up` elige slirp4netns ahí también.

## CI del propio litek8s

[test.yml](../../.github/workflows/test.yml) en cada push y pull request:

| job | qué |
|---|---|
| `cargo test (ubuntu-latest)`, `(macos-latest)` | todas las pruebas unitarias y de integración con [cargo-nextest](https://nexte.st) |
| `build-linux` | el binario del commit para los jobs de abajo |
| `compat` | los niveles de API del corpus de compatibilidad (L0 descubrimiento, L1 objetos y CRDs, 50 casos) contra `litek8s apiserver --no-node` en el runner: sin Docker, sin VM |
| `example (helm\|compose\|multi-node)` | los ejemplos de GitHub Actions sobre el binario del commit a través de la action local |

Qué lo hizo más rápido (varias ejecuciones de cada uno, cachés calientes):

| | antes (cargo test) | después (cargo nextest, sin debug info) |
|---|---|---|
| job `cargo test (ubuntu-latest)` | 171 s (158 a 213) | **98 s** (95 a 156) |
| su paso de pruebas (build + ejecución) | 158 s | **85 s** |
| job `cargo test (macos-latest)` | 293 s (237 a 371) | **218 s** (105 a 250) |
| su paso de pruebas (build + ejecución) | 267 s | **187 s** |
| workflow, del inicio del primer job al fin del último | 298 s (240 a 378) | **224 s** (109 a 257), con los jobs compat y example añadidos |
| solo la ejecución de pruebas (ubuntu) | 65 s | 41 a 47 s |

Antes: el test.yml de main en 690a2d5, reejecutado sobre ese commit con la
caché caliente de main, 4 veces ([run 39557051657](https://github.com/jairoFernandez/litek8s/actions/runs/39557051657));
después: este test.yml, 5 intentos de [run 39559413919](https://github.com/jairoFernandez/litek8s/actions/runs/39559413919),
caché de la rama caliente. Medianas, rangos entre paréntesis. Los runners de
macOS de GitHub son los que más varían (105 a 250 s para el mismo job). Los
jobs nuevos corren junto al de macOS: `build-linux` 66 s, luego `compat` 52 s
y los ejemplos de 13 a 33 s cada uno.

- **Sin debug info** (`CARGO_PROFILE_DEV_DEBUG=0`): el crate y sus pruebas
  compilan y enlazan más rápido, y la caché es más pequeña (Linux 268 MiB en
  lugar de 346, macOS 293 en lugar de 314). Los backtraces conservan los
  nombres de las funciones, no los números de línea.
- **cargo-nextest**: cada prueba en su propio proceso, los binarios de prueba
  en paralelo. Solo `tests/ha.rs` tomaba 38 s de los 65 s de `cargo test`,
  ejecutado después de las 400 pruebas unitarias; ahora el resto corre en
  paralelo con él. `.config/nextest.toml` mantiene las pruebas de HA de una en
  una, como hacía `cargo test`, y detiene una prueba colgada después de 5
  minutos. No hay doctests, así que no se pierde nada.
- **build.rs se reejecutaba en cada build en CI**: le pedía a cargo vigilar
  `.git/packed-refs`, que un checkout nuevo no tiene, y cargo reejecuta un
  build script (y recompila el crate) por un archivo vigilado que no existe.
  Inofensivo con un solo `cargo test`, ~50 s con un paso de build y uno de
  pruebas.
- **La caché**: las últimas ejecuciones en main decían "No cache found" porque
  0.2.0 cambió la matriz de jobs, y con ello todas las claves de caché; las
  claves no tenían nada mal. Ahora la caché se guarda solo desde main (las
  ramas y los pull requests restauran la de main).

No hecho: unir los cuatro `tests/*.rs` en un solo binario de prueba (tres
enlazados menos del crate). Con nextest ejecutándolos en paralelo y sin debug
info, no valía la pena perder `cargo test --test ha` y los archivos por área.

`cargo test` sigue ejecutando todas las pruebas en local; `cargo nextest run`
coincide con el CI.
