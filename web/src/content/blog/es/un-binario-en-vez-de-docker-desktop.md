---
title: "Un binario en vez de Docker Desktop, y además es Kubernetes"
description: "litek8s dev up sirve la Docker Engine API: docker, compose y Testcontainers sin cambios, y kubectl y Helm sobre el mismo cluster. También lo que falta."
date: 2026-10-30
tags: [docker, compose, testcontainers, desarrollo local, macos, linux]
docs: [docker, dev, quickstart, numbers, platforms, limitations]
lang: es
translation: one-binary-instead-of-docker-desktop
---

La laptop típica de alguien que hace backend tiene dos motores. Uno corre
contenedores: Docker Desktop, OrbStack, Colima o dockerd en Linux. El otro
corre Kubernetes: kind o k3d encima del primero, o el Kubernetes que se
puede activar en Docker Desktop. Dos cosas que instalar, dos que mantener
al día, dos lugares donde viven tus imágenes.

`litek8s dev up` es un solo comando que te da las dos. Arranca un cluster
de Kubernetes local y, en un socket al lado, la Docker Engine API. El CLI
real de `docker`, `docker compose`, Dev Containers y Testcontainers hablan
con ese socket sin cambios; `kubectl` y Helm hablan con el mismo cluster.

## Dos minutos

```bash
litek8s dev up
export DOCKER_HOST=unix://$HOME/.litek8s/docker.sock

docker run --rm -it alpine sh
docker compose up -d --build

kubectl create deployment web --image=nginx
kubectl expose deployment web --type=NodePort --port 80
curl localhost:$(kubectl get svc web -o jsonpath='{.spec.ports[0].nodePort}')
```

Lo que más usamos: el socket de Docker y el cluster comparten un mismo
almacén de imágenes, así que una imagen que construyes sirve de inmediato
como `image:` de un pod. Sin registry, sin `kind load`:

```bash
DOCKER_BUILDKIT=0 docker build -t myapp:dev .   # el builder de litek8s
kubectl create deployment myapp --image=myapp:dev
```

Corre **al lado** de un Docker u OrbStack instalado, en su propio socket.
No detiene, cambia ni usa nada de ellos. Si dejas `DOCKER_HOST` sin
definir, les hablas como siempre, o cambias con un contexto:

```bash
docker context create litek8s --docker host=unix://$HOME/.litek8s/docker.sock
docker context use litek8s        # docker context use default (u orbstack) para volver
```

O sea que probarlo no cuesta nada: no tienes que desinstalar nada antes.

## Dónde corren los contenedores

- **macOS (Apple silicon):** en una VM Linux chiquita sobre
  Virtualization.framework de Apple, que levanta el propio litek8s. Sin
  Docker Desktop, sin Lima, sin sudo. `$HOME`, `/private/tmp` y
  `/private/var/folders` se comparten en **las mismas rutas**, así que
  `-v $PWD:/app` y los archivos temporales de Testcontainers funcionan, y
  los puertos publicados escuchan en `127.0.0.1` y `::1`. Con Rosetta
  instalado, corren imágenes `--platform linux/amd64`.
- **Linux (x86_64, aarch64):** en un sandbox rootless con sus propios
  namespaces, sobre crun, sin daemon, sin grupo `docker` y sin binario
  setuid. No toca las interfaces de red del host, sus reglas de iptables
  ni un Docker instalado.

## Los números

Operaciones de contenedores, medidas con `bench/docker-bench.sh` (el mismo
protocolo contra cada motor, con las imágenes ya descargadas). Linux: un
guest Ubuntu 24.04 arm64 (4 vCPU, 6 GiB), litek8s rootless contra Docker
29.1 en el mismo guest. macOS: una M3 Max, la VM de litek8s contra
OrbStack.

| | litek8s (Linux) | dockerd (Linux) | litek8s (VM macOS) | OrbStack |
|---|---|---|---|---|
| `docker run --rm alpine true` | **54 ms** | 179 ms | **89 ms** | 295 ms |
| nginx `run -d -p` hasta que responde | **52 ms** | 135 ms | **91 ms** | 175 ms |
| compose up (postgres healthy, cliente corrió) | **1.7 s** | 2.0 s | **2.8 s** | 3.1 s |
| build sin caché (app de python) | **0.05 s** | 0.37 s | 0.74 s | **0.61 s** |
| build todo en caché | **0.02 s** | 0.15 s | 0.48 s | **0.28 s** |
| memoria en reposo | **35 MiB** | 113 MiB | | |
| memoria con postgres + nginx | **64 MiB** | 149 MiB | | |

Los builds usan el builder propio de litek8s (`DOCKER_BUILDKIT=0`). Ojo:
en macOS OrbStack construye más rápido. La columna de memoria en Linux es
todo el sandbox de litek8s, con el API server de Kubernetes, los
controllers y el agente de nodo incluidos, contra dockerd + containerd +
los contenedores. La memoria de OrbStack no se puede separar de sus otras
máquinas, así que no se compara, y Docker Desktop no entró en el benchmark.

Y del lado de Kubernetes, desde `litek8s dev up` hasta un nodo Ready,
contra kind y k3d ([Números](/es/docs/numbers/)):

| | litek8s | kind | k3d |
|---|---|---|---|
| Linux (guest Ubuntu 24.04 arm64) | **0.17 s** | 31.6 s | 11.1 s |
| macOS (M3 Max; kind y k3d sobre OrbStack) | **1.28 s** | 34.3 s | 10.4 s |
| Memoria en reposo, Linux | **47 MiB** | 526 MiB | 398 MiB |

## Lo que ya funciona

`compat/docker/run.sh` corre 17 casos que pasan en Linux x86_64 rootless y
en la VM de macOS. A grandes rasgos: `docker run` con los flags de siempre
(puertos, volúmenes, variables de entorno, healthchecks, `--restart`,
límites, `--privileged`), `exec`, `logs -f`, `cp`, `stats`, redes propias
con aliases, `host.docker.internal`, volúmenes con nombre, compose con
healthchecks y `depends_on: condition: service_healthy`, el CLI
`devcontainer` que usan VS Code y Cursor, y Testcontainers con puertos
aleatorios, estrategias de espera y su reaper Ryuk.

Proyectos reales corrieron sin cambios, en copias: un backend FastAPI +
Postgres en macOS con hot reload de uvicorn al editar en la Mac (un cambio
llega a un watcher dentro del contenedor en 30-70 ms), y en Linux x86_64 un
stack de Prometheus + Grafana, MySQL + Redis con `restart: always`, y una
app Rails con MariaDB construida desde su Dockerfile (5 min 10 s con el
builder de litek8s, 6 min 28 s con el contenedor BuildKit de buildx).

Algo que funciona distinto a propósito: Docker y OrbStack limitan los
contenedores uno por uno, mientras que litek8s pone todos los contenedores
de un proyecto de compose en un mismo cgroup, así que le puedes dar un
presupuesto al stack entero:

```yaml
services:
  api:
    labels:
      dev.litek8s.project.memory: 2Gi
      dev.litek8s.project.cpus: "2"
```

Cuando el stack llega al límite, el OOM killer actúa dentro del proyecto,
sobre el servicio que se desbocó, y el resto de tu máquina ni se entera.

## Lo que falta o es distinto

Esta es la sección que hay que leer antes de desinstalar cualquier cosa.

- **No soportado (todavía):** `docker push`, `commit`, `export`/`import`,
  `diff`, swarm y services, plugins y otros drivers de volumen o de red,
  IPv6 en redes de contenedores, `--init`, `--device`, GPU, puertos UDP
  publicados, la API de BuildKit en sí (el driver `docker` de buildx) y
  `docker update` (lo acepta, pero no cambia nada).
- **buildx necesita `--load`.** Si tienes buildx instalado (Docker Desktop,
  OrbStack y la mayoría de los paquetes de Linux lo traen), un
  `docker build` a secas pasa por un contenedor de BuildKit y la imagen no
  aparece en `docker images`, ni para los pods, sin `--load`. Usa
  `DOCKER_BUILDKIT=0` o un builder con `default-load=true`.
  `docker compose up --build` carga la imagen de cualquier forma.
- **Los contenedores no son pods.** `kubectl get pods` no lista los
  contenedores de Docker. Comparten con el cluster el almacén de imágenes y
  el DNS, no el modelo de objetos.
- **Los contenedores no sobreviven a `litek8s dev down`**; las políticas de
  reinicio los traen de vuelta con `dev up`.
- **Puertos:** se publican solo en `127.0.0.1` (y `::1` en macOS), nunca en
  otras interfaces; en Linux rootless no se pueden usar puertos del host
  por debajo de 1024.
- **Dos motores, una terminal:** si Docker u OrbStack también están
  corriendo, revisa con cuál estás hablando (`docker version --format
  '{{.Server.Platform.Name}}'` dice `litek8s` aquí). Un puerto que el otro
  motor ya publica falla con `port is already allocated`.
- **Memoria en macOS:** Virtualization.framework nunca le devuelve a macOS
  la memoria que el guest ya tocó. litek8s mantiene el guest chico y lo
  hace crecer según haga falta, pero después de un build grande la forma
  de recuperar memoria es `litek8s dev down && litek8s dev up`.
- **Un proyecto joven:** litek8s es 0.x; los flags y los formatos en disco
  pueden cambiar entre versiones ([Limitaciones](/es/docs/limitations/)).

## Plataformas

| | clusters de desarrollo y Docker API |
|---|---|
| macOS, Apple silicon (13+) | sí, en una VM (Virtualization.framework) |
| Linux x86_64 / aarch64 | sí, rootless (kernel ≥ 5.11, cgroup v2) |
| macOS Intel, Windows | no (WSL2 sin probar) |

En Linux necesita `crun`, `uidmap`, `passt` (o `slirp4netns`), `iproute2` e
`iptables`; `litek8s dev setup` revisa y dice qué falta, y Ubuntu ≥ 23.10
necesita `sudo "$(command -v litek8s)" dev setup` una vez por AppArmor.
macOS no necesita nada más.

## Pruébalo

```bash
brew install jairoFernandez/tap/litek8s
```

o

```bash
curl -fsSL https://raw.githubusercontent.com/jairoFernandez/litek8s/main/install.sh | sh
```

Después `litek8s dev up`, apunta `DOCKER_HOST` al socket y corre el
archivo de compose que ya tienes. Cada flag y lo que soporta cada comando
está en [API de Docker](/es/docs/docker/); el lado del cluster, en
[Clusters de desarrollo](/es/docs/dev/).
