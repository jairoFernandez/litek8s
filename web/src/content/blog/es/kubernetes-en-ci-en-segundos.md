---
title: "Kubernetes en CI en ~2.5 s en vez de ~45 s: litek8s frente a kind"
description: "Un cambio de una línea en GitHub Actions: un cluster Ready en ~2.5 s en vez de ~45 s y jobs de Helm 3.7x más cortos. Medido, y dónde no conviene."
date: 2026-10-30
tags: [ci, github actions, gitlab, kind, helm, rendimiento]
docs: [ci, numbers, dev, limitations]
lang: es
translation: kubernetes-in-ci-in-seconds
---

Casi todos los jobs e2e de Kubernetes arrancan igual: `helm/kind-action`,
y a esperar. kind corre cada nodo como un contenedor de Docker con un nodo
completo adentro (systemd, containerd, kubelet, etcd, el API server,
kube-proxy, CoreDNS, kindnet), y en los runners `ubuntu-24.04` de GitHub
eso tardó una mediana de **44.7 s** hasta que los nodos quedaron Ready. En
cada push y en cada pull request, antes de correr una sola prueba.

Con litek8s el mismo cluster queda Ready en **2.6 s**, contando la descarga
del binario. Este post es sobre ese número: cómo lo medimos, la línea que
cambia en el workflow y los casos en los que cambiarse no vale la pena.

## El diff

Este es un workflow e2e de Helm. Lo único que cambia es el paso de
configuración:

```diff
     steps:
       - uses: actions/checkout@v4
-      - uses: helm/kind-action@v1
+      - uses: jairoFernandez/litek8s/.github/actions/setup-litek8s@v0.2.1
       - run: |
           helm install hello ./chart --wait
           helm test hello
```

El chart, los manifiestos, `kubectl` y `helm` quedan igual. La action
descarga el release para la arquitectura del runner, lo verifica contra el
`SHA256SUMS` del release, corre `sudo litek8s dev setup` por la restricción
de AppArmor de Ubuntu sobre los user namespaces, arranca el cluster como el
usuario del runner y espera a que todos los nodos estén Ready. Los pasos
siguientes reciben `KUBECONFIG` y también `DOCKER_HOST`, porque el mismo
cluster sirve la Docker Engine API: `docker`, `docker compose` y
Testcontainers funcionan en el mismo job.

Tres nodos en vez de uno es un input:

```yaml
      - uses: jairoFernandez/litek8s/.github/actions/setup-litek8s@v0.2.1
        with:
          nodes: 3      # control plane + 2 workers, unidos como nodos reales
```

Y si algo falla, la action imprime `litek8s dev status`, los pods, los
eventos y los logs del cluster. Para tus propios pasos:

```yaml
      - if: failure()
        run: litek8s-diagnose
```

## Los números

Tres workflows de ejemplo (un chart de Helm con install, test, upgrade y
rollback; tres nodos con topology spread, un DaemonSet y un drain; un stack
de compose construido desde un Dockerfile), corridos en el runner
`ubuntu-24.04` de GitHub (4 vCPU, 16 GB) con litek8s v0.2.0 contra
`helm/kind-action@v1` (kind v0.33.0, su imagen de nodo por defecto) y
contra el propio dockerd del runner. Medianas de 5 corridas, todos los jobs
en frío: sin caché, con las imágenes bajadas de Docker Hub en cada job y
por cada motor.

| ejemplo | motor | tiempo a ready | job total | memoria en reposo | memoria pico |
|---|---|---|---|---|---|
| helm | **litek8s** | **2.6 s** | **21 s** | **12 MiB** | **42 MiB** |
| helm | kind | 44.7 s | 79 s | 449 MiB | 549 MiB |
| multinodo (3 nodos) | **litek8s** | **2.5 s** | **15 s** | **28 MiB** | **82 MiB** |
| multinodo (3 nodos) | kind | 49.7 s | 72 s | 715 MiB | 827 MiB |
| compose | litek8s | 2.2 s | **31 s** | 13 MiB | 46 MiB |
| compose | dockerd del runner | **0 s** (ya corría) | 44 s | 0 MiB | **28 MiB** |

En las 5 corridas litek8s estuvo listo en 2.1 a 4.1 s y kind en 40.8 a
53.2 s. Totales por job: litek8s de 13 a 38 s, kind de 67 a 85 s.

En limpio:

- Un job de Kubernetes es **de 3.7 a 4.8 veces más corto**, mediana contra
  mediana.
- El cluster ocupa **de 1/37 (helm) a 1/26 (multinodo) de la memoria de
  kind**.
- Los pasos del ejemplo también son más rápidos (helm 10.2 s contra
  22.7 s, multinodo 4.5 s contra 11.1 s). Estas corridas no desglosan en
  qué gasta kind ese tiempo, así que no vamos a adivinar.

El workflow que produjo esto está en el repositorio
([ci-bench.yml](https://github.com/jairoFernandez/litek8s/blob/main/.github/workflows/ci-bench.yml)),
y también [la corrida](https://github.com/jairoFernandez/litek8s/actions/runs/39559413930).
Córrelo en tu fork antes de creerle a un post de blog.

## A dónde se van los 42 segundos

Los 45 s de kind son trabajo honesto: bajar una imagen de nodo, levantar
systemd y containerd dentro de un contenedor, arrancar etcd y el control
plane, y después esperar a que el nodo reporte Ready.

`litek8s dev up` no tiene nada que bootear. Crea un sandbox rootless (user,
network, mount, PID y cgroup namespaces) y arranca el API server, los
controllers, el scheduler, el DNS del cluster y un agente de nodo sobre
crun en un solo proceso, sin etcd. Es el mismo comando que se usa en una
laptop, donde llega a un nodo Ready en 0.17 s en Linux
([Números](/es/docs/numbers/)). En el runner, casi todo lo que queda de los
2.5 s es la descarga y las verificaciones.

## GitLab y Docker in Docker

Los runners Linux de GitLab.com corren los jobs en contenedores
privilegiados, que es justo lo que necesita `docker:dind`. No lo corrimos
en GitLab; reprodujimos ese entorno en el runner de GitHub: un contenedor
privilegiado, con `docker:27-dind` más kind v0.33.0 de un lado, y
[litek8s-ci.sh](https://github.com/jairoFernandez/litek8s/blob/main/examples/ci/litek8s-ci.sh)
en `ubuntu:24.04` del otro, con sus paquetes de apt y la descarga contados
en el tiempo a ready. litek8s v0.2.1, medianas de 5 corridas (kind en dind:
4, porque un pod de `helm test` falló dentro de kind).

| ejemplo | motor | tiempo a ready | job total | pico de memoria del contenedor |
|---|---|---|---|---|
| helm | **litek8s** | **9.2 s** | **39 s** | **356 MiB** |
| helm | kind en dind | 40.0 s | 70 s | 2,960 MiB |
| compose | litek8s | 5.3 s | 40 s | 353 MiB |
| compose | dind | **1.0 s** | **38 s** | 345 MiB |

Con Kubernetes, listo 4× antes y el job de Helm 1.8× más corto: kind en
dind baja su imagen de nodo de ~1 GB y corre un nodo completo dentro del
contenedor del job. El job de GitLab:

```yaml
k8s-e2e:
  image: ubuntu:24.04
  script:
    - apt-get update -qq && apt-get install -y -qq curl ca-certificates
    - eval "$(sh examples/ci/litek8s-ci.sh)"
    - curl -fsSLo /usr/local/bin/kubectl "https://dl.k8s.io/release/v1.33.0/bin/linux/amd64/kubectl" && chmod +x /usr/local/bin/kubectl
    - kubectl apply -f deploy/ && kubectl rollout status deployment/myapp --timeout=2m
```

El mismo script sirve en cualquier máquina o VM Linux que controle el job
(agentes de Jenkins, Buildkite, el machine executor de CircleCI): imprime
las variables de entorno que hay que exportar.

## Cuándo no conviene

La versión "miren este número" del post terminaría acá. La útil tiene esta
tabla:

| el job necesita | en vez de | veredicto |
|---|---|---|
| un cluster de Kubernetes en los runners de GitHub | kind, k3d | **cámbiate**: jobs 3–5× más rápidos, 26–37× menos memoria |
| un cluster de Kubernetes en un contenedor privilegiado | kind en dind | **cámbiate**: job de Helm de 39 s en vez de 70 s |
| solo Docker (build, compose, Testcontainers) en los runners de GitHub | el dockerd del runner | ganancia chica: 31 s contra 44 s en el job de compose, y dockerd no necesita configuración |
| solo Docker en un contenedor privilegiado | Docker in Docker | **sin ganancia**: 40 s contra 38 s; el dockerd de dind arranca en ~1 s |

Si tu pipeline solo construye imágenes y corre compose, quédate con lo que
tienes, o cámbiate por la ganancia chica si ya quieres el mismo motor que
usas en local. La ganancia grande es para los jobs que necesitan
Kubernetes.

## Lo que se cede, sin rodeos

- **Solo runners Linux** (`ubuntu-*`, x86_64 y arm64). En macOS el cluster
  de desarrollo es una VM, y los runners macOS de GitHub no permiten
  virtualización anidada.
- **No hay caché de imágenes entre corridas.** Las capas desempaquetadas
  pertenecen a los user ids subordinados del cluster, que el usuario del
  runner no puede volver a leer para meterlas en un archivo de caché. Las
  descargas desde Docker Hub tardaron de 0.7 a 6 s por imagen en estos
  runners.
- **La red es slirp4netns, no pasta.** En los runners de GitHub pasta
  perdía flujos TCP de vez en cuando (3 de 5 corridas se colgaron con el
  pasta de Ubuntu, ninguna con slirp4netns), así que la action usa
  slirp4netns por defecto.
- **No comparado:** el kind preinstalado del runner sin la action, k3d, y
  una imagen de nodo de kind en caché entre corridas. Podrían cerrar parte
  de la brecha; no los medimos.
- **No es Kubernetes upstream.** Pasa 413 de las 419 specs de conformance
  de Kubernetes v1.33 (k3s: 416), y litek8s es un proyecto 0.x joven cuyos
  flags y formatos todavía pueden cambiar. Si tus pruebas e2e dependen de
  un comportamiento exacto del kubelet, lee primero
  [Limitaciones](/es/docs/limitations/). Si prueban tu chart, tu operador o
  tus manifiestos, para eso está.

## Pruébalo

Cambia el paso de configuración en un workflow y compara el tiempo del job
en tu propia pestaña de Actions:

```yaml
      - uses: jairoFernandez/litek8s/.github/actions/setup-litek8s@v0.2.1
```

Fíjalo a un tag de release, como arriba. Todos los inputs (`nodes`,
`docker`, `builder`, `version`, `wait`), tres workflows para copiar (Helm,
compose, multinodo) y el método completo están en [CI](/es/docs/ci/).
