---
title: "Noviembre 2026 en litek8s, primera mitad: 419/419, tus usuarios y docker push"
description: "Lo que salió en litek8s de la v0.2.18 a la v0.2.25: conformance completa, OIDC y webhook, admisión para los pods de los controladores, docker push y GPUs."
date: 2026-11-14
tags: [changelog, novedades, build in public]
docs: [auth, docker, networking, registries, clusters, install, numbers, limitations]
lang: es
translation: november-2026-in-litek8s
---

Ocho versiones en quince días: la v0.2.18 salió el 30 de octubre, justo
después del [resumen de octubre](/es/blog/octubre-2026-en-litek8s/), y la
v0.2.25 el 14 de noviembre. Esto es lo que cambió, agrupado por lo que
significa para ti y no por versión. Cada punto está en el
[CHANGELOG](https://github.com/jairoFernandez/litek8s/blob/main/CHANGELOG.md).

## Las cinco que más importan

**1. 419/419 en la suite de conformance de Kubernetes v1.33 (0.2.22).**
Todas las specs `[Conformance]` del `e2e.test` de upstream pasan en Linux
con crun, en un cluster de desarrollo rootless de dos nodos; k3s pasa 416
en la misma suite. Los últimos tres fallos eran un emptyDir
`medium: Memory` que era un directorio en disco en vez de un tmpfs, un init
container que decía `ContainerCreating` donde el kubelet dice
`PodInitializing`, y un runner que usaba un kubectl demasiado nuevo para una
spec. La historia está en [419 de 419](/es/blog/419-de-419/).

**2. Tus usuarios desde OIDC, tokens estáticos o un webhook (0.2.25).** Los
servidores solo conocían certificados firmados por su propia CA y tokens de
service account. `litek8s apiserver` acepta ahora los flags
`--oidc-issuer-url`, `--token-auth-file` y
`--authentication-token-webhook-config-file` de kube-apiserver, con sus
valores por defecto, así que los usuarios de Dex, Keycloak, Google o Entra
ID entran con el login que ya tienen, y RBAC los enlaza a ellos y a sus
grupos como a cualquiera. [¿Quién eres?](/es/blog/quien-eres/) explica en
qué orden se prueba un token y lo pone a prueba contra un emisor pequeño.

**3. Los webhooks de admisión ven los pods de los controladores (0.2.25).**
Los pods de un Deployment se saltaban los webhooks de mutación y
validación, así que Istio, Linkerd o Vault Agent nunca les inyectaban su
sidecar, y las políticas de Kyverno o Gatekeeper sobre pods no se les
aplicaban. Cada objeto que el servidor escribe por su cuenta pasa ahora por
admisión como `system:kube-controller-manager`, y un ReplicaSet cuyos pods
rechaza un webhook registra `FailedCreate` y vuelve a intentar.
[El sidecar que nunca llegaba](/es/blog/el-sidecar-que-nunca-llegaba/).

**4. `docker push` (0.2.25).** La API Docker respondía `501` a un push.
Ahora sube a cualquier registry OCI con la salida de progreso de Docker, y
un `alpine:3.20` descargado y subido a tu propio registry conserva el
digest que publica Docker Hub, aunque litek8s guarda las capas
descomprimidas. [Subir lo que ya no tienes](/es/blog/subir-lo-que-ya-no-tienes/)
cuenta cómo, y qué comprobamos.

**5. GPUs sin device plugin (0.2.23).** En los nodos crun, el agente lee
las especificaciones de Container Device Interface que escriben
`nvidia-ctk cdi generate` o `amd-ctk`, anuncia cada tipo como un recurso
extendido (`nvidia.com/gpu: 2`) y le da a un contenedor que pide uno sus
propios dispositivos, montajes y hooks. Sin operador que instalar. Ver
[docs/gpus.md](https://github.com/jairoFernandez/litek8s/blob/main/docs/gpus.md).

## Kubernetes se comporta más como Kubernetes

- **Los objetos inválidos se rechazan (0.2.25)**, con los mensajes de
  kube-apiserver: nombres según el tipo, claves y valores de labels,
  selectores de workloads, specs de pods, restartPolicy de los Jobs, puertos
  de Services. Las actualizaciones hacen ratcheting: lo que un objeto ya
  guardado tiene mal no lo bloquea, solo se rechazan los problemas nuevos.
  Un Deployment guardado por 0.2.24 con el label `team: "web team!"` sigue
  escalando en 0.2.25, mientras que un label nuevo inválido se rechaza:

  ```text
  $ kubectl scale deployment web --replicas=3
  deployment.apps/web scaled
  $ kubectl patch deployment web --type merge -p '{"metadata":{"labels":{"tier":"front end"}}}'
  The Deployment "web" is invalid: metadata.labels: Invalid value: "front end": a valid label must be ...
  $ kubectl create -f job.yaml    # restartPolicy: Always
  The Job "j" is invalid: spec.template.spec.restartPolicy: Unsupported value: "Always": supported values: "OnFailure", "Never"
  ```

- **Los rollouts que dejan de avanzar fallan (0.2.24)** con
  `ProgressDeadlineExceeded`, así que `kubectl rollout status` termina en
  vez de esperar y Argo CD muestra la app Degraded.
- **El plazo de un Job vence aunque su pod siga Pending (0.2.21).** Un pod
  que no cabe en ningún nodo nunca cambia, así que el controlador nunca
  volvía a mirar el Job. [El pod que espera](/es/blog/el-pod-que-espera/).
- **Los eventos del kubelet (0.2.22)**: `Unhealthy`, `Killing`, `BackOff` y
  los fallos de descarga de imágenes, contados en un solo Event cuando se
  repiten.
- **Se usan los `imagePullSecrets` de los pods (0.2.22)**, o los de su
  ServiceAccount. [Registries privados](/es/docs/registries/).
- **Corregido: un PV con `Retain` perdía sus datos (0.2.19)** al borrar su
  objeto. Ahora solo `Delete` borra los datos.

## Red

- **NetworkPolicies en el runtime Docker (0.2.25).** En octubre solo se
  aplicaban en nodos crun; los servidores macOS y otros nodos Docker las
  guardaban sin efecto. Ahora aplican las mismas reglas, cargadas en el
  namespace de red de Docker antes de que arranque un pod seleccionado. Un
  host Docker que no puede filtrar el tráfico de los bridges deja los pods
  aislados en `ContainerCreating` con el motivo, en vez de correrlos sin
  protección. Por ahora probado en OrbStack.
  [Redes](/es/docs/networking/#networkpolicies).
- **Relays elegidos por tiempo de ida y vuelta (0.2.22).** Los nodos se
  hacen ping por el overlay cada 10 s, y un par sin camino directo pasa por
  el relay con el menor a → relay → b. `litek8s_overlay_peer_rtt_seconds`
  muestra cada ida y vuelta.

## Más rápido

Dos artículos de una serie sobre dónde gasta su tiempo litek8s, los dos con
el benchmark que encontró el problema:

- **Lists hasta 13× más rápidos (0.2.18)**: un list copia los bytes JSON
  guardados en vez de parsear y volver a serializar cada objeto. 1.000
  ConfigMaps: de 507 a 6.616 lists/s. [Pequeño no basta](/es/blog/pequeno-no-basta/).
- **`kubectl get` 2,4× más rápido (0.2.20)**: construir una Table copiaba
  cada fila dos veces. [La copia escondida en json!](/es/blog/la-copia-escondida-en-json/)

Con ellos llegaron `cargo bench --bench api_load` y `--bench micro`, y cada
pull request se compara ahora con su base en el mismo runner de CI.

## Servidores y operación

- **`litek8s volume move <claim> --to <node>` (0.2.19)** mueve un volumen
  `local-path` a otro nodo: baja sus workloads a cero, copia y comprueba los
  datos por SSH, vuelve a crear el PV y el claim, y los sube de nuevo.
  [Clusters](/es/docs/clusters/).
- **Un token de agente (0.2.24)** une nodos y nada más: no puede añadir un
  servidor al plano de control y no es una credencial de admin. Los agentes
  lo reciben de `install` y `cluster apply`; el token de join es para
  servidores.
- **Releases firmados con SBOM (0.2.23)**: cada archivo tiene una
  atestación de procedencia firmada con Sigstore y un SBOM CycloneDX;
  `LITEK8S_VERIFY=1` hace que install.sh la compruebe.
  [Verificar un release](/es/docs/install/#verificar-un-release).
- **Debian sin sbin en el PATH (0.2.24)**: `dev up` fallaba con
  `failed to run iptables` ([#38](https://github.com/jairoFernandez/litek8s/issues/38)).
  litek8s añade ahora los directorios del sistema que le faltan a su PATH.
- **Un entorno de desarrollo rootless que no arranca lo dice en un segundo
  (0.2.24)**, con su log, en vez de esperar un minuto y dejar procesos
  colgados.

## También escribimos

- [Pequeño no basta](/es/blog/pequeno-no-basta/) y
  [La copia escondida en json!](/es/blog/la-copia-escondida-en-json/), la
  serie de velocidad
- [El pod que espera](/es/blog/el-pod-que-espera/)
- [419 de 419](/es/blog/419-de-419/)
- [El sidecar que nunca llegaba](/es/blog/el-sidecar-que-nunca-llegaba/),
  [Subir lo que ya no tienes](/es/blog/subir-lo-que-ya-no-tienes/) y
  [¿Quién eres?](/es/blog/quien-eres/), sobre la 0.2.25

## Lo que todavía falta

De lo que listaba el resumen de octubre, ya están OIDC, tokens estáticos y
autenticación por webhook, los `imagePullSecrets` y `docker push`. Lo que
se sabe que falta hoy ([Limitaciones](/es/docs/limitations/)):

- un solo emisor OIDC (sin `--authentication-config`), la API
  `TokenReview` solo comprueba tokens de service account, y los
  certificados de cliente no se pueden revocar;
- `docker push` sube una plataforma, nunca un índice multiplataforma;
- no se recogen los endpoints `/metrics` de las aplicaciones;
- todavía no hay controlador de HorizontalPodAutoscaler.

## Pruébalo

```bash
brew install jairoFernandez/tap/litek8s
litek8s dev up
litek8s ui
```

O, sin Homebrew:

```bash
curl -fsSL https://raw.githubusercontent.com/jairoFernandez/litek8s/main/install.sh | sh
```

Issues e ideas en
[github.com/jairoFernandez/litek8s](https://github.com/jairoFernandez/litek8s).
