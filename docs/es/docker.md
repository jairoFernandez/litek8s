# La Docker API: litek8s en lugar de Docker Desktop / OrbStack

`litek8s dev up` también sirve la Docker Engine API, así que la CLI `docker`
real, `docker compose`, los Dev Containers de VS Code / Cursor y Testcontainers
ejecutan sus contenedores en litek8s:

```bash
litek8s dev up
export DOCKER_HOST=unix://$HOME/.litek8s/docker.sock
docker run --rm -it alpine sh
docker compose up -d --build
```

**Compilar con buildx instalado** (Docker Desktop, OrbStack y la mayoría de
los paquetes de Linux lo incluyen): `docker build` pasa por buildx, así que
agrega `--load` o la imagen no aparece en `docker images` (ni para los pods). O
haz que cargar sea el comportamiento por defecto para este endpoint una sola
vez, o usa el builder propio de litek8s:

```bash
docker buildx create --use --name litek8s --driver docker-container --driver-opt default-load=true
DOCKER_BUILDKIT=0 docker build -t myapp:dev .     # litek8s's builder, no buildx
```

`docker compose up --build` carga sus imágenes de cualquier forma. Detalles en
[Builds](#builds-dos-builders).

Corre **junto a** un Docker u OrbStack instalado, en su propio socket: nada de
ellos se detiene, se modifica ni se usa. Deja `DOCKER_HOST` sin definir para
hablar con ellos como antes. Para cambiar con `docker context` en lugar de una
variable de entorno (esto solo agrega un contexto a la CLI):

```bash
docker context create litek8s --docker host=unix://$HOME/.litek8s/docker.sock
docker context use litek8s        # docker context use default (or orbstack) to go back
```

Con ambos motores arriba, comprueba con cuál habla una shell antes de confiar
en un resultado: `docker version --format '{{.Server.Platform.Name}}'` dice
`litek8s` aquí. Los scripts y los agentes de IA deberían poner
`DOCKER_HOST=unix://...` en cada comando en lugar de depender de un `export`
anterior: la herramienta de shell de un agente suele abrir una shell nueva en
cada llamada, así que la variable desaparece y el comando llega al otro motor,
lo que parece un mal comportamiento de litek8s (un contenedor atascado en
`created` mientras "su" puerto responde es el contenedor del otro motor
ocupando ese puerto). Un puerto que el otro motor ya publica falla aquí con
`port is already allocated`.

El socket es `$LITEK8S_DOCKER_SOCKET` si está definida. `litek8s dev down`
detiene los contenedores junto con el cluster; `dev up` vuelve a levantar los
que tienen una restart policy (`always`, `unless-stopped`), como hace Docker
después de un reinicio.

## Dónde corren los contenedores

- **Linux:** en el sandbox de desarrollo rootless (con sus propios namespaces
  de usuario, red, montaje, PID y cgroup, [dev.md](../dev.md)), sobre crun, sin
  daemon. No se tocan la red del host, sus reglas de iptables ni Docker.
- **macOS:** en la VM de desarrollo (Virtualization.framework), sobre crun. El
  lado del Mac retransmite el socket por vsock, escucha en `127.0.0.1` **y**
  `::1` cada puerto publicado, comparte `$HOME`, `/private/tmp` y
  `/private/var/folders` en las mismas rutas (así funcionan `-v $PWD:/app`,
  `-v /tmp/x:/x` y los archivos temporales de Testcontainers), pasa los eventos
  de cambios de archivos a la VM y comparte Rosetta.

### Contenedores, no pods

Un contenedor de Docker aquí es un contenedor a nivel de runtime, no un Pod de
Kubernetes. La semántica de Docker no encaja en los Pods: un contenedor se crea
y se arranca por separado, se le hace attach antes de que corra, se detiene y
se vuelve a arrancar **con sus archivos**, se copian cosas dentro mientras está
detenido (Testcontainers lo hace), vive en redes propias con alias y conserva
su código de salida hasta que se elimina. Por eso el motor (`src/docker/`)
mantiene sus propios registros y maneja crun a través del mismo shim por
contenedor que usa el runtime de pods, con la capa escribible conservada entre
ejecuciones y un socket de attach para stdin y stdout en vivo. Lo que comparte
con el cluster: el almacén de imágenes (una imagen que construyes con
`docker build` puede ser de inmediato el `image:` de un pod, y los workloads de
`kubectl` reutilizan las capas descargadas), el servidor DNS, el shim de crun y
el código de bundles. `kubectl get pods` no lista los contenedores de Docker.

## Qué funciona

Probado con `compat/docker/run.sh` (17 casos, todos pasan en Linux x86_64
rootless y en la VM de macOS; ejecútalo también contra Docker u OrbStack, donde
se omite el caso `compose_budget`, exclusivo de litek8s):

| área | soportado |
|---|---|
| `docker run` | `-d`, `--rm`, `-it` (pty, redimensionado, teclas de detach), stdin con `-i`, `-p`/`-P`, `-v`/`--mount` (bind, volume, tmpfs), `-e`/`--env-file`, `-w`, `-u`, `--name`, `--network`, `--network-alias`, `--entrypoint`, `--restart`, `--health-*`, `--add-host` (incl. `host-gateway`), `--platform`, `--cap-add/--cap-drop`, `--privileged`, `--read-only`, `--shm-size`, `--ulimit`, `--sysctl`, `--memory`, `--cpus`, `--pid`/`--network container:` |
| contenedores | `ps`, `logs` (`-f`, `--tail`, `--since`, `-t`), `exec` (`-it`, `-u`, `-e`, `-w`), `stop`/`start`/`restart`/`kill`/`pause`, `rm`, `inspect`, `rename`, `wait`, `cp` (en ambos sentidos, también antes de arrancar), `top`, `stats`, `port`, `prune`, `events` |
| imágenes | `pull` (progreso, `--platform`; se pasan las credenciales de `docker login`; CAs, mirrors y credenciales para cada pull: [registries.md](../registries.md)), `images`, `tag`, `rmi`, `inspect`, `history`, `save`, `load`, `prune` |
| build | `docker build` con la API clásica: caché del Dockerfile, multi-stage, `--target`, `--build-arg`, `.dockerignore`, `-f` fuera del contexto; además de la sintaxis de Dockerfile de BuildKit (`RUN --mount=type=cache\|bind\|tmpfs`, `COPY --chmod`/`--link`, heredocs, `TARGETARCH`, etc.) |
| redes | `network create/ls/inspect/rm/connect/disconnect/prune`; un bridge por red; nombres, alias e IDs cortos se resuelven en redes definidas por el usuario; las redes están aisladas entre sí; `host.docker.internal` llega al host |
| volúmenes | volúmenes con nombre y anónimos (llenados desde la imagen en el primer uso, como Docker), opciones de bind del driver `local` (`o=bind,device=`), `volume ls/inspect/rm/prune` |
| compose | `up`/`down`/`ps`/`logs -f`/`exec`/`restart`/`build`/`pull`, redes del proyecto con nombres de servicio y alias, healthchecks y `depends_on: condition: service_healthy`, archivos env, volúmenes con nombre y anónimos, bind mounts, puertos publicados, restart policies, límites por servicio (`mem_limit`, `cpus`, `pids_limit`, `deploy.resources.limits`) y un presupuesto para todo el proyecto (siguiente sección) |
| Dev Containers | la CLI `devcontainer` (lo que ejecutan VS Code y Cursor): features, `postCreateCommand`, `remoteUser`, `containerEnv`, `mounts`, exec, propiedad de archivos |
| Testcontainers | puertos aleatorios (`-P`) mediante inspect, estrategias de espera por log y por puerto, exec, copia hacia dentro y hacia fuera, el reaper Ryuk (monta el socket de Docker: cualquier origen de bind llamado `docker.sock` es, dentro de los contenedores, el propio socket de este motor) |

## Presupuestos de recursos para proyectos de compose

Docker y OrbStack limitan los contenedores uno por uno. Un stack de compose en
una laptop se entiende mejor como una sola máquina: litek8s pone cada
contenedor de un proyecto de compose (label `com.docker.compose.project`) en un
cgroup, `litek8s/docker/projects/<project>`, y ese cgroup puede tener un
presupuesto:

- **memory** (`memory.max`, sin swap): todo el stack junto. Cuando se alcanza,
  el OOM killer del kernel actúa *dentro del proyecto*: elige el proceso más
  grande ahí (el servicio desbocado), no otra cosa de la VM, y los demás
  proyectos y el cluster ni se enteran. Con **oom_group**, en cambio, se mata
  a todos los contenedores del proyecto (`memory.oom.group`), como un servidor
  que se quedó sin memoria.
- **cpus** (`cpu.max`): el stack comparte esa cantidad de CPUs.
- **pids** (`pids.max`): una fork bomb se queda en su proyecto.

Los límites por contenedor (`mem_limit`, `cpus`, ...) siguen aplicándose dentro
del presupuesto. Un presupuesto viene de `~/.litek8s/config.yaml`:

```yaml
docker:
  projects:                 # by compose project name
    myapp: {memory: 2Gi, cpus: 2}
    kafka-lab: {memory: 6Gi, cpus: 3, pids: 2000, oom_group: true}
  default_project:          # any project nothing else gives a budget
    memory: 4Gi
  defaults:                 # a container that sets no limit of its own (off when absent)
    memory: 1Gi
    cpus: 1
```

o de un label en cualquier servicio del proyecto, para que el presupuesto
pueda viajar con el archivo de compose:

```yaml
services:
  api:
    labels:
      dev.litek8s.project.memory: 2Gi
      dev.litek8s.project.cpus: "2"
      dev.litek8s.project.pids: "1000"
      dev.litek8s.project.oom-group: "true"
```

Campo por campo, la entrada de la config para el proyecto gana sobre los labels
(tu máquina, tus límites), y los labels ganan sobre `default_project`; cuando
los labels de varios servicios no coinciden, gana el más pequeño. Los tamaños
son `2Gi`, `512Mi` o `1g` (binarios, como los lee Docker; un número sin unidad
es MiB, como el `memory:` de nivel superior), las CPUs `2`, `1.5` o `"500m"`.
Los labels que no se pueden parsear hacen fallar `docker create` con el motivo.
`defaults` completa solo lo que un contenedor deja sin definir (memory, cpus,
pids); `docker inspect` sigue mostrando lo que pidió el cliente.

`litek8s dev up` envía la sección `docker:` a la Docker API del cluster, que la
guarda (`<data>/docker/settings.json`) y la aplica de inmediato a los proyectos
en ejecución; para un cluster que ya está corriendo, volver a ejecutar
`litek8s dev up` (o guardar el archivo desde Settings en `litek8s ui`) aplica un
archivo editado sin reiniciar nada. Los labels se aplican cuando arranca un
contenedor del proyecto. `GET /litek8s/v1/docker/projects` en el socket de
Docker lista el presupuesto y el uso de cada proyecto;
`GET`/`PUT /litek8s/v1/docker/settings` leen y reemplazan la configuración.

Lo que ves:

- `docker stats` muestra el límite que realmente restringe a un contenedor: el
  más pequeño entre el suyo, el presupuesto de su proyecto y el del cluster
  (`MEM USAGE / LIMIT` es `… / 256MiB` para cada servicio de un proyecto de
  256Mi), y los PIDs.
- Un contenedor matado por OOM sale con **137**, `docker inspect` dice
  `"OOMKilled": true`, y el evento `oom` de Docker indica qué límite alcanzó:
  `litek8s.oom=project` (con `litek8s.project.memory=256Mi`), `container` (su
  propio `mem_limit`) o `system`. El log del motor lo dice en palabras:
  `docker: container shop-worker-1 (3f2a…) was OOM-killed: out of compose
  project shop's budget (memory 256Mi)`.
- La vista Containers de `litek8s ui` muestra en la fila de cada proyecto un
  medidor de memoria contra el presupuesto, las CPUs en uso y los OOM kills,
  marca los contenedores matados como `OOMKilled` y lo indica en la línea de
  estado ([ui.md](../ui.md#contenedores-y-presupuestos)).

`compat/docker/run.sh compose_budget` lo prueba de punta a punta: un proyecto
de 256Mi cuyo servicio `tail /dev/zero` es matado mientras su otro servicio y
otro proyecto siguen corriendo, y lo mismo con `oom-group`.

## Builds: dos builders

El motor reporta el builder clásico (`Builder-Version: 1`). Lo que ejecuta tu
build depende entonces de la CLI `docker`:

- **Sin el plugin buildx**, o con `DOCKER_BUILDKIT=0`, `docker build` usa el
  **builder de litek8s**: cada `RUN` es un contenedor de crun sobre un overlay
  de las capas de la imagen cuyo directorio superior se convierte en la nueva
  capa; los pasos se cachean con una clave calculada sobre las instrucciones,
  los build args y el contenido de los archivos copiados (un rebuild
  completamente cacheado da el mismo ID de imagen). Es lo más rápido aquí; sin
  contenedor extra.
- **Con buildx instalado** (Docker Desktop, OrbStack y la mayoría de los
  paquetes de Linux lo incluyen), `docker build` y los builds de compose pasan
  por buildx, cuyo builder por defecto para un motor sin BuildKit es un
  contenedor `moby/buildkit` (driver `docker-container`), que buildx arranca
  en litek8s. BuildKit completo (secrets, montajes SSH, exportación de caché,
  multiplataforma), y compose carga el resultado en litek8s. Un `docker build`
  simple necesita `--load` para que la imagen aparezca en `docker images`, o un
  builder que cargue por defecto:

  ```bash
  docker buildx create --use --name litek8s --driver docker-container --driver-opt default-load=true
  ```

  (los builders son por endpoint: esto no cambia el builder de tu contexto de
  Docker u OrbStack).

  Para usar el builder de litek8s en su lugar: `DOCKER_BUILDKIT=0` para
  `docker build`, `DOCKER_BUILDKIT=0 COMPOSE_BAKE=false` para compose (la CLI
  imprime un aviso de deprecación del builder legacy; sigue funcionando).

Un contenedor de BuildKit no puede usar imágenes que existen solo en litek8s
(descarga las imágenes de `FROM` desde registries). Eso importa para los **Dev
Containers en Linux**: el paso de la CLI que cambia el UID del usuario remoto
hace un build `FROM` la imagen que acaba de construir. Con buildx instalado,
ejecuta la CLI de devcontainer con el builder clásico:

```bash
DOCKER_BUILDKIT=0 devcontainer up --buildkit never --workspace-folder .
```

o, como litek8s mantiene tu UID igual dentro (ver abajo), define
`"updateRemoteUserUID": false` en `devcontainer.json` cuando tu UID es el del
usuario de la imagen (normalmente 1000). En macOS la CLI omite ese paso.

## Propiedad de archivos en bind mounts

- **Linux:** el sandbox rootless mapea tu propio UID a sí mismo y respalda
  todos los demás UID, root incluido, con tu rango de `/etc/subuid`. Un archivo
  que un contenedor crea en un bind mount como UID 1000 (el usuario `vscode` de
  un dev container, `docker run -u $(id -u)`) es tuyo en el host, como con
  Docker rootful; uno creado por root pertenece a un id subordinado (con Docker
  rootful: a root). `LITEK8S_DEV_IDMAP=root litek8s dev up` te mapea a root en
  su lugar (semántica de Docker rootless: los archivos de root son tuyos, los
  del UID 1000 no).
- **macOS:** los archivos en directorios compartidos siempre te pertenecen, sin
  importar qué UID los escribió.

## Hot reload en macOS

virtiofs le muestra a la VM el contenido nuevo de un archivo de inmediato, pero
no le avisa nada al kernel de la VM cuando cambia en el Mac, así que los
watchers de inotify en los contenedores (uvicorn `--reload`, nodemon, Vite,
webpack, Air) se perderían las ediciones. El lado del Mac vigila los
directorios que los contenedores en ejecución montan con bind (FSEvents) y el
guest vuelve a aplicar el modo de cada archivo cambiado, lo que inotify reporta
como `IN_ATTRIB`: una edición llega a un watcher en el contenedor en 30-70 ms.
Una app uvicorn en un proyecto FastAPI + Postgres se recarga con el cambio unos
3.6 s después de guardar, casi todo ese tiempo es el reinicio de la app.

## linux/amd64 en Apple silicon

Cuando Rosetta está instalado (`softwareupdate --install-rosetta`), la VM lo
comparte y lo registra para binarios x86-64:
`docker run --platform linux/amd64 ...` y `platform: linux/amd64` en compose
ejecutan imágenes amd64. Como con el almacén de imágenes clásico de Docker, un
tag contiene una sola plataforma: descargar `alpine` para amd64 reemplaza el
`alpine:latest` de arm64.

## Puertos

Los puertos publicados escuchan en `127.0.0.1` del host (y `::1` en macOS),
nunca en otras interfaces: `docker ps` muestra `127.0.0.1:8080->80/tcp`. `-P`
elige puertos libres. Los puertos UDP no se publican (siguen siendo alcanzables
en la dirección del contenedor dentro del sandbox/VM). Linux rootless no puede
hacer bind a puertos del host por debajo de 1024.

## No soportado (todavía)

`docker push`, `commit`, `export`/`import`, `diff`, `docker swarm` y services,
plugins y otros drivers de volumen o de red (`overlay`, `macvlan`, `ipvlan`),
IPv6 en redes de contenedores, `--init`, `--device`, GPU, puertos UDP
publicados, la propia API de BuildKit (`/grpc`, `/session`: el driver `docker`
de buildx), `docker stats` más allá de CPU, memoria y PIDs, `docker update`
(se acepta, no cambia nada), `checkpoint`. Los contenedores no sobreviven a
`litek8s dev down` (las restart policies los vuelven a levantar en `dev up`).

## Cifras

`bench/docker-bench.sh` (el mismo protocolo contra cualquier motor; imágenes
descargadas de antemano). Linux: guest Ubuntu 24.04 arm64 (qemu+HVF, 4 vCPU,
6 GiB), litek8s rootless vs Docker 29.1 (el `docker.io` de Ubuntu) en el mismo
guest. macOS: M3 Max, VM de desarrollo de litek8s vs OrbStack (los propios
contenedores y máquinas virtuales de OrbStack estaban corriendo).

| | litek8s (Linux) | dockerd (Linux) | litek8s (VM de macOS) | OrbStack |
|---|---|---|---|---|
| `docker run --rm alpine true` | **54 ms** | 179 ms | **89 ms** | 295 ms |
| nginx `run -d -p` hasta que responde | **52 ms** | 135 ms | **91 ms** | 175 ms |
| compose-basic up (postgres healthy, el cliente se ejecutó) | **1.7 s** | 2.0 s | **2.8 s** | 3.1 s |
| build, sin caché (app de Python) | **0.05 s**¹ | 0.37 s | 0.74 s¹ | **0.61 s** |
| build, todo cacheado | **0.02 s**¹ | 0.15 s | 0.48 s¹ | **0.28 s** |
| memoria, en reposo² | **35 MiB** | 113 MiB | | |
| memoria, con postgres + nginx corriendo² | **64 MiB** | 149 MiB | | |

¹ El builder de litek8s (`DOCKER_BUILDKIT=0`). A través del contenedor BuildKit
de buildx: Linux 0.88 / 0.29 s, macOS 11.5 / 0.76 s (el primer build también
arranca el contenedor de BuildKit; domina la exportación de la imagen a
litek8s).
² `memory.current - inactive_file` del cgroup: todo el sandbox de litek8s
(incluye el API server de Kubernetes, los controllers y el agente de nodo);
dockerd + containerd + los scopes de los contenedores. La memoria de OrbStack
no se puede aislar de sus otras máquinas y contenedores, así que no se compara.

Proyectos reales corren sin cambios (en copias, con otros puertos del host
donde el Docker del usuario los ocupaba): un backend FastAPI + Postgres en
macOS (build con BuildKit, migraciones de Alembic, hot reload de uvicorn con
ediciones hechas en el Mac), y en Linux x86_64 un stack Prometheus + Grafana
(datasource aprovisionado, scraping de `host.docker.internal`), MySQL + Redis
con `restart: always`, y una app Rails con MariaDB compilada desde su
Dockerfile (`bundle install` con gems nativas y un build de assets: 5 min 10 s
con el builder de litek8s, 6 min 28 s a través del contenedor BuildKit de
buildx, exportación incluida).
