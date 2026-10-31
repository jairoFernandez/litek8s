---
title: "Octubre 2026 en litek8s: 21 versiones, RBAC, NetworkPolicies y una UI para cualquier cluster"
description: "Lo que salió en litek8s en octubre de 2026: RBAC y NetworkPolicies aplicadas, una GitHub Action para CI, una UI de terminal para cualquier cluster y litecd."
date: 2026-10-30
tags: [changelog, novedades, build in public]
docs: [auth, networking, ci, ui, registries, litecd, numbers, limitations]
lang: es
translation: october-2026-in-litek8s
---

Octubre fue el primer mes de litek8s en público. La primera versión, la
v0.1.0, se etiquetó el 20 de octubre; para el 30 ya iban 21, hasta la
v0.2.17. Esto es lo que cambió, agrupado por lo que significa para ti y no
por versión. Cada punto está en el
[CHANGELOG](https://github.com/jairoFernandez/litek8s/blob/main/CHANGELOG.md).

## Las cinco que más importan

**1. Los servidores autentican clientes y aplican RBAC (0.2.12).** Hasta
entonces, cualquier cliente que llegara al puerto de un servidor era admin
del cluster, con token o sin él: cualquier pod podía leer todos los Secrets.
Ahora `litek8s apiserver` autentica tokens de service account, certificados
de cliente y el token de join, y aplica Role, ClusterRole y sus bindings, con
`--authorization-mode RBAC` por defecto. `RBAC-audit` evalúa todo, lo permite
igual y registra cada denegación, para que encuentres el RBAC que tus charts
nunca trajeron antes de aplicarlo de verdad. Los clusters de desarrollo no
cambian. Para los servidores fue un cambio incompatible:
[Autenticación y RBAC](/es/docs/auth/) tiene los pasos para un 401 y para
workloads que reciben 403.

**2. Las NetworkPolicies se aplican (0.2.12).** Antes se guardaban y se
servían, pero todos los pods llegaban a todos los pods. Ahora ingress y
egress, selectores de pods y de namespaces, `ipBlock` con `except`, y puertos
con nombre y por rango sobre TCP, UDP y SCTP se aplican en nodos crun
(Linux), también entre nodos por el overlay de WireGuard, y las reglas de un
pod seleccionado se cargan antes de que arranque su primer contenedor. El
runtime Docker en macOS las sigue guardando sin efecto.
[Red a través de NATs](/es/docs/networking/).

**3. Una GitHub Action para CI (0.2.1).** Un paso le da a un job un cluster y
una API de Docker, en lugar de `helm/kind-action` y el dockerd del runner:

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

Medido en los runners `ubuntu-24.04` de GitHub, medianas de 5 corridas:

| e2e de un chart de Helm | litek8s | kind |
|---|---|---|
| cluster listo | 2.6 s | 44.7 s |
| job completo | 21 s | 79 s |
| pico de memoria | 42 MiB | 549 MiB |

Configuración, rangos y los ejemplos de compose y de varios nodos:
[CI](/es/docs/ci/).

**4. `litek8s ui` funciona en clusters que no son litek8s (0.2.13–0.2.17).**
Home ahora muestra bien clusters k3s, kubeadm y administrados: la
distribución y su versión, el CPU y la memoria de cada nodo desde
`metrics.k8s.io` contra lo asignable, con historia. Debajo de los medidores
muestra lo que los pods *reservan* (requests y limits contra lo asignable, y
cuántos pods no tienen límite), con marcas `│` y `┃` en los medidores donde
llegan los requests y los limits. Una vista nueva, Apps (`0`), lista las
Applications de Argo CD y de litecd con sync, salud, historial y resultado
por recurso, y puede sincronizar, refrescar y abrir diffs usando solo la API
de Kubernetes. [Interfaz de terminal](/es/docs/ui/).

**5. litek8s se achicó a propósito (0.2.0).** Se fueron el daemon original
basado en Docker y el cómputo distribuido de LLMs experimental: unas 25,000
líneas de Rust, cada una con un reemplazo nativo de Kubernetes que ya había
salido. El binario de la release bajó de 38.7 MiB a 32.2 MiB (macOS arm64,
stripped). El CHANGELOG tiene una tabla que lleva de cada comando eliminado
a lo que hay que usar en su lugar.

## Para servidores de verdad

- **Los Services LoadBalancer escuchan en los puertos de la propia
  máquina** (80, 443) en servidores y nodos, como el servicelb de k3s, así
  que un ingress controller responde en la IP pública de una VM en la nube
  (0.2.5). El API server les deja el puerto 443 (0.2.6–0.2.7), y el status
  del LoadBalancer prefiere la ExternalIP del nodo para que external-dns
  publique la dirección pública (0.2.12).
- **Registros privados y mirrors** en el formato `registries.yaml` de k3s:
  un archivo de k3s funciona tal cual, para los pulls de pods y para la API
  de Docker (0.2.3). [Registros privados y mirrors](/es/docs/registries/).
- **El DNS del cluster** resuelve nombres externos en hosts cuyo primer
  nameserver es IPv6 (0.2.6), y los nodos que se unen mientras el servidor
  arranca reciben el DNS del cluster (0.2.3).

## Compatibilidad con Kubernetes

La mayoría salió de correr software real y ver qué se rompía:

- `kubectl exec` y port-forward por WebSocket ya no se cortan al minuto: el
  servidor responde los pings de client-go (0.2.12).
- Un `replace` de JSON patch sobre un miembro que no existe lo crea, como
  hace el json-patch de upstream; los labels de HA de Vault dependen de eso
  (0.2.12).
- OpenAPI v2 y v3 de los tipos integrados, así que el server-side apply y el
  diff de Argo CD funcionan y `kubectl explain` muestra los tipos integrados
  (0.2.2).
- Las columnas de CRDs con filtros muestran su valor: los Certificates de
  cert-manager ya no aparecen como no listos en `kubectl get` (0.2.11).
- exec y attach por SPDY para el `NewSPDYExecutor` de client-go; `type` en
  `hostPath`; `mountPropagation`; los pods pueden usar puertos por debajo de
  1024 y hacer ping sin privilegios; `fsGroup` puede leer archivos con
  `defaultMode: 0600` (0.2.17).
- Certificados de pod y ClusterTrustBundles, suficiente para que Agent
  Substrate se instale y corra en un solo nodo crun (0.2.17,
  [substrate.md](https://github.com/jairoFernandez/litek8s/blob/main/docs/substrate.md)).

## Clusters de desarrollo

- El disco de la VM de desarrollo tiene un tamaño configurable (`disk:` en
  config.yaml, `dev up --disk`), que crece en el lugar sin perder datos
  (0.2.9).
- `sudo litek8s dev setup` instala lo que le falta a un cluster rootless
  (crun, uidmap, passt, iproute2, iptables) con el gestor de paquetes del
  sistema (0.2.11).
- `docker run -d` y `docker compose up -d` vuelven a retornar al instante
  cuando litek8s heredó `NOTIFY_SOCKET` de systemd o de otra herramienta
  (0.2.10).

## Números que publicamos

- Todo el control plane en reposo ocupa **23.2 MiB** con el binario de la
  release v0.2.1, contra 567 MiB de k3s minimal, en una VM x86_64 de 1 GiB.
- Las métricas, logs y alertas integradas suman **1.8 MiB**, contra
  1,371 MiB de kube-prometheus-stack más Loki con la misma carga; lo que esa
  comparación deja afuera está en
  [su propio post](/es/blog/observabilidad-sin-kube-prometheus-stack-ni-loki/).

Todos, con sus fuentes: [Números](/es/docs/numbers/).

## Al lado de litek8s: litecd

[litecd](https://github.com/ideascoldigital/litecd), un proyecto aparte, es
un controlador GitOps que lee los manifiestos `Application`, `AppProject` y
`ApplicationSet` de Argo CD sin cambios: 20 apps en reposo en 6.9 MiB contra
los 310 MiB de Argo CD en el mismo cluster litek8s. Ahora tiene página en la
documentación, [GitOps con litecd](/es/docs/litecd/), y dos posts de cuando
lo pusimos en un cluster de producción:
[Reemplazar el controlador de Argo CD con litecd](/es/blog/reemplazar-argo-cd-con-litecd/)
y [El nodo demasiado lleno](/es/blog/el-nodo-demasiado-lleno/).

## También escribimos

- [No necesitas un transbordador espacial](/es/blog/no-necesitas-un-transbordador-espacial/)
- [Kubernetes en una Raspberry Pi](/es/blog/kubernetes-en-raspberry-pi/)
- [Clonar un cluster de producción en litek8s](/es/blog/clonar-un-cluster-de-produccion/)
- Para agentes: litek8s.dev sirve `/llms.txt` y `/llms-full.txt`, generados
  a partir de la documentación (0.2.12).

## Lo que todavía falta

Según [Limitaciones](/es/docs/limitations/) y la documentación, los huecos
conocidos hoy:

- no hay autenticadores OIDC, de token estático ni webhook, ni revocación de
  certificados de cliente;
- no se hace scraping de los endpoints `/metrics` de las aplicaciones;
- todavía no hay controlador de HorizontalPodAutoscaler;
- no se usan los `imagePullSecrets` de los pods, y la API de Docker todavía
  no puede hacer `docker push`.

## Pruébalo

```bash
brew install jairoFernandez/tap/litek8s
litek8s dev up
litek8s ui
```

O sin Homebrew:

```bash
curl -fsSL https://raw.githubusercontent.com/jairoFernandez/litek8s/main/install.sh | sh
```

Issues e ideas en
[github.com/jairoFernandez/litek8s](https://github.com/jairoFernandez/litek8s).
