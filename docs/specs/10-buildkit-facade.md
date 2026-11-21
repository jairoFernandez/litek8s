# Spec 10: Fachada BuildKit sobre el builder clásico

- Prioridad: P1
- Estado: Draft
- Depende de: ninguno
- Se beneficia de: `compat/docker/run.sh`

## Contexto

La Docker API de litek8s (`src/docker/`) anuncia el builder clásico (`_ping` con `Builder-Version: 1`) y responde 501 a `POST /session` (`src/docker/api.rs`). Con el plugin buildx instalado (Docker Desktop, OrbStack y la mayoría de los paquetes de Linux), `docker build` es `docker buildx build`. buildx elige su driver así (buildx v0.37.2):

- `driver/docker/factory.go`: usa el driver `docker` solo si `DialHijack(ctx, "/grpc", "h2c")` funciona.
- Si no, cae al driver implícito `docker-container`, que levanta un contenedor `moby/buildkit` y tiene `default-load=false` (`build/opt.go`). La imagen se construye pero no llega a `docker images` sin `--load`.

Hoy el usuario necesita `DOCKER_BUILDKIT=0` (y `COMPOSE_BAKE=false` para compose), o crear un builder con `default-load=true` (docs/docker.md, «Builds: two builders»). El quickstart (`src/cli.rs`) y `dev/lk env` ya lo avisan.

## Problema

- `docker build` contra litek8s «funciona» pero deja la imagen fuera del daemon, lo que confunde.
- El camino BuildKit en contenedor es más lento (contenedor extra de 240 MB, export a tar y carga posterior) y no ve imágenes que solo existen en litek8s (`FROM` de una imagen local falla).
- Herramientas que llaman a `docker build` sin control sobre el entorno (Nexus, devcontainers, scripts de terceros) caen en ese camino.

## Objetivos

- `docker build`, `docker compose build` (con bake) y `docker buildx bake` dejan la imagen en litek8s sin variables de entorno ni builders creados a mano.
- El progreso de buildx se ve normal (pasos, logs de `RUN`, cacheados).
- Reutilizar el builder propio (`docker/build::run`): sin contenedor BuildKit, con las imágenes locales visibles para `FROM`.

## No objetivos

- Ejecutar LLB arbitrario (buildctl, Dagger, Earthly, frontends externos por `# syntax=`).
- Multi-platform, cache export/import, export OCI: buildx los apaga solo (ver Diseño).
- Historial de builds (`ListenBuildHistory`).

## Diseño

litek8s se presenta como un BuildKit que solo entiende Dockerfiles y por debajo usa el builder clásico.

### Qué pide buildx con el driver `docker`

1. `POST /grpc` con upgrade a h2c. Sobre esa conexión, dos servicios gRPC:
   - `moby.buildkit.v1.Control`: `Info`, `ListWorkers`, `Solve`, `Status`, `DiskUsage`, `Prune`, `ListenBuildHistory`.
   - `moby.buildkit.v1.frontend.LLBBridge` (gateway): buildx llama `c.Build(...)` (`build/build.go`), que usa `Ping` (caps), `Solve{Frontend:"dockerfile.v0"}`, `Return`, `Evaluate`, `Warn`, a veces `ResolveImageConfig`.
2. `POST /session` con upgrade: el CLI es el servidor gRPC y el daemon el cliente.
   - `FileSync.DiffCopy` (protocolo fsutil: stat, req, data, fin) para los dirs locales `context` y `dockerfile`.
   - `Auth` (`Credentials`, `FetchToken`) para registries.
   - Fase 2: `Secrets.GetSecret`, `SSH.ForwardAgent`, `FileSend` para outputs locales.
3. Features (`driver/docker/driver.go`, `Features`): si `ListWorkers` no trae la etiqueta `org.mobyproject.buildkit.worker.snapshotter`, buildx apaga export OCI/docker, cache export, multi-platform y direct push, y deja `DefaultLoad=true`. Eso coincide con lo que el builder clásico no tiene.

### Mapeo a litek8s

| Pieza BuildKit | En litek8s |
|---|---|
| `/grpc` h2c | Servidor tonic sobre el IO que entrega el upgrade de hyper, en `api.rs` (donde hoy está el 501). Necesita `http2` en hyper. |
| `/session` | Registrada por `X-Docker-Expose-Session-Uuid`; métodos expuestos en `X-Docker-Expose-Session-Grpc-Method`. litek8s abre un cliente gRPC h2 sobre esa conexión. |
| `Info` / `ListWorkers` | Versión de buildkit compatible; un worker con la plataforma del host, sin etiqueta de snapshotter. |
| `Solve` de `dockerfile.v0` | Opciones del frontend (`filename`, `target`, `build-arg:*`, `label:*`, `platform`, `no-cache`, `add-hosts`) → `build::Options`. Contexto por DiffCopy a un directorio; `run()` hoy toma `context.tar` (decidir si acepta un directorio). |
| `Status` | Cada `Step N/M` es un `Vertex` (digest del texto del paso; started, completed, cached, error); la salida de `RUN` va como `VertexLog`. |
| Exporter `moby` / `image` | Tags desde `name=` (como `-t`); `ExporterResponse` con `containerimage.digest` y `containerimage.config.digest`. |
| `DiskUsage` / `Prune` | `build::usage` / `build::prune`. |
| `ListenBuildHistory` | Unimplemented (buildx lo toma como «sin historial»). |
| `ResolveImageConfig` | Resolución de imágenes de `images.rs`. |
| LLB crudo, `--call` | Unimplemented con mensaje claro. |
| `# syntax=` | Se ignora con un warning: el builder ya entiende la sintaxis BuildKit. |

`_ping` sigue diciendo `Builder-Version: 1`, así los CLIs sin buildx siguen usando el builder clásico.

### Activación

Cuando `/grpc` responda, buildx elegirá el driver `docker` para todos los usuarios. Hoy, sin querer, tienen BuildKit completo en contenedor (multi-platform, cache export) y lo perderían sin aviso; pueden volver con `docker buildx create --driver docker-container`. Por eso:

- Primero detrás de un setting en `~/.litek8s/config.yaml` (`docker.buildkit: true`), leído por `dev up`.
- Por defecto cuando pase el corpus.

### Dependencias

- `tonic` (+ prost ya presente), hyper con `http2`.
- Protos de buildkit vendorizados (control, gateway, filesync, fsutil wire/stat, auth, secrets, sshforward), compilados en `build.rs` como `proto/`.

## Fases

1. **Fase 1 (4–6 días):** `/grpc`, `/session`, Control, LLBBridge mínimo, DiffCopy con los patrones de `.dockerignore` que manda el cliente, Auth, Status y exporter moby. Meta: los tres comandos de Objetivos sin variables.
2. **Fase 2 (3–5 días):** `RUN --mount=type=secret` y `type=ssh` por la sesión, contextos con nombre (`--build-context`), `--output type=local|tar` por la sesión.

## Pruebas

- Unit tests: receptor fsutil (stat, req, data, fin; patrones), mapeo de pasos a vertices, mapeo de opciones del frontend.
- `compat/docker/run.sh`: casos con buildx real (`docker build`, `-t`, `--target`, `--build-arg`, `FROM` de imagen local, `compose build`, `buildx bake`), comprobando que la imagen queda en `docker images`.
- CI con al menos dos versiones de buildx (mínima soportada y última): los caps del gateway cambian entre versiones.

## Riesgos

- Protocolo poco documentado (caps del `Ping` del gateway, wire de fsutil): seguir el código de buildkit y probar contra el buildx real desde el primer día.
- Superficie gRPC nueva en el socket de Docker: mismo modelo de acceso que el resto del socket (local, permisos de archivo).

## Decisiones tomadas

- Corto plazo (hecho): aviso de `DOCKER_BUILDKIT=0` en el quickstart y en `dev/lk env`.
- Descartado como default: crear y seleccionar un builder `docker-container` con `default-load=true` en `dev up` (modifica `~/.docker/buildx` del usuario, build más lento, no ve imágenes locales).

## Siguiente paso recomendado

Fase 1, empezando por `/grpc` + `Info`/`ListWorkers` para confirmar que buildx elige el driver `docker`, luego `/session` + DiffCopy.
