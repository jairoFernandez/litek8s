---
title: "No necesitas un transbordador espacial para entregar tu app"
description: "Por qué litek8s es tan rápido, qué cuesta esa velocidad y por qué empezar pequeño con la API real de Kubernetes te deja migrar después sin reescribir nada."
date: 2026-10-29
tags: [opinión, rendimiento, equipos pequeños]
docs: [numbers, alongside, auth, limitations]
lang: es
translation: you-dont-need-a-space-shuttle
---

Hay una conversación que se repite en casi todos los equipos pequeños:
"¿Usamos Kubernetes?". Y la respuesta suele caer en uno de dos extremos.
O montas EKS/GKE con su VPC, su IAM, sus node groups, su ingress
controller, su stack de Prometheus y Loki, y pagas en dinero y en horas un
sistema diseñado para cientos de equipos. O renuncias y terminas con un
`docker compose up` en una VM y un script de deploy que nadie quiere tocar.

Mi premisa es simple: **yo no necesito un transbordador espacial para
entregar mi app**. Necesito Deployments, Services, rollouts, config,
secretos, Helm, quizá Argo CD, métricas y logs. Y quiero que, el día que
el negocio lo pida, pueda irme a algo enterprise sin reescribir nada.

Este post es un análisis honesto de por qué litek8s es tan rápido, qué
cuesta esa velocidad, y por qué creo que empezar pequeño con la API real de
Kubernetes es una apuesta razonable.

## Los números primero

Todo esto está medido con los scripts del repositorio, contra k3s (la
distribución "ligera" de referencia), kind y k3d. Setup, datos crudos y
letra pequeña en [Números](/es/docs/numbers/).

| | litek8s | referencia |
|---|---|---|
| Control plane completo, en reposo, sin pods (VM x86_64 de 1 GiB) | **23 MiB** | k3s mínimo: 567 MiB |
| Orquestador completo con 5 pods nginx | **33,5 MiB** | k3s mínimo: 579 MiB |
| Argo CD + una app sincronizada, memoria del orquestador | **90 MiB** | k3s mínimo: 900 MiB |
| `dev up` → nodo Ready, Linux / macOS | **0,17 s / 1,28 s** | kind 31,6 s / 34,3 s, k3d 11,1 s / 10,4 s |
| Job de GitHub Actions: e2e de un chart de Helm | **21 s** | kind: 79 s |
| Métricas, logs y alertas, memoria añadida | **+1,8 MiB** | kube-prometheus-stack + Loki: 1.371 MiB |
| Conformance oficial de Kubernetes v1.33 | **413/419** | k3s: 416/419 |

No son números de laboratorio para un tweet: la diferencia de memoria es
lo que le queda libre a tus pods en una VM barata o en una Raspberry Pi.

## Por qué es rápido (de verdad)

Sería fácil decir "porque está escrito en Rust". Sería falso, o al menos
muy incompleto. Rust ayuda (sin GC, binario estático, memoria predecible),
pero la mayor parte de la ganancia viene de **lo que litek8s no hace**.

### 1. Un proceso en vez de una flota

Un Kubernetes clásico son varios procesos que se hablan por red: API
server, etcd, controller-manager, scheduler, kubelet, kube-proxy, CoreDNS,
containerd. Cada uno con su runtime, sus cachés, sus watches y su
serialización JSON/protobuf de ida y vuelta.

En litek8s el API server, el object store, los controllers, el scheduler,
el DNS, el proxy de servicios y el agente de nodo son **un solo proceso**.
Los controllers no hacen una petición HTTP para actualizar el status de un
Deployment: escriben en el store directamente. Medido mientras Argo CD
sincroniza una app, k3s atiende casi el doble de peticiones al API (11,5
vs 6,4 por segundo) porque su kubelet y su controller-manager escriben
status y eventos a través de él.

### 2. Sin etcd

El estado vive en memoria con un write-ahead log append-only en disco. Para
un nodo o un puñado de nodos eso es todo lo que hace falta. Cuando
necesitas alta disponibilidad, tres o cinco servidores replican ese mismo
store con Raft, sin traer un etcd aparte ([HA](/es/docs/ha/)).

### 3. Arrancar un pod es barato

En k3s un pod pasa por kubelet → containerd → un contenedor *pause* →
shim → runc. En litek8s, en Linux, arrancar un pod es un `unshare`, un
`ip -batch` y `crun create/start` por contenedor. No hay contenedor pause:
los namespaces de red e IPC del pod viven como bind mounts. Cada
contenedor tiene un shim de ~1,5 MiB, y las salidas llegan por un pidfd,
así que **nada hace polling**: el runtime en reposo usa 0 % de CPU.

Resultado: un `helm install` de Argo CD hasta los 7 pods Ready tarda 19,9 s
contra 27,1 s en k3s, y un cambio de réplicas re-sincronizado tarda 1,0 s
contra 1,9 s.

### 4. Lo que normalmente instalas aparte, ya viene dentro

DNS de cluster, proxy de servicios, `metrics.k8s.io` para `kubectl top`,
una base de datos de series temporales, búsqueda de logs y alertas. No
como pods adicionales, sino como parte del mismo binario. Por eso
"observabilidad" cuesta 1,8 MiB y no 1,3 GiB.

## Lo que esa velocidad cuesta

Un análisis honesto tiene que incluir esta sección. Si litek8s fuera mejor
en todo, sería sospechoso.

- **La autenticación y el RBAC son más simples que los de upstream.** Los
  servidores autentican clientes (tokens de service account y certificados
  de cliente) y aplican RBAC por defecto, pero no hay OIDC ni SSO, y no se
  puede revocar un solo certificado de cliente: para cortarlo hay que rotar
  la CA de clientes. Para un equipo, esto es lo más importante de esta
  lista. Aun así, deja el API (puerto 6443) detrás de un firewall o una VPN
  ([Autenticación y RBAC](/es/docs/auth/)).
- **Es joven y es el proyecto de una persona.** Versiones 0.x: flags, APIs
  y formatos en disco pueden cambiar sin migración.
- **No es 100 % kubelet.** Faltan algunos comportamientos (eventos
  `Unhealthy`/`BackOff`), ValidatingAdmissionPolicy se acepta pero no se
  aplica, y `imagePullSecrets` no se usa: los nodos tiran de su propia
  configuración de registros. La lista completa está en
  [Limitaciones](/es/docs/limitations/).
- **No todo es más rápido.** El primer sync de Argo CD, que incluye bajar
  una imagen, tardó 18,1 s contra 15,7 s en k3s; probablemente el pull y
  desempaquetado de imágenes. Y en una versión temprana litek8s perdía en
  CPU frente a k3s con 5 cargas por su propio polling, hasta que se
  eliminó. Medir a tu competidor también te enseña dónde pierdes.
- **Sin load balancer de nube.** Un Service `LoadBalancer` se publica en
  los puertos de cada nodo, como el servicelb de k3s. Funciona para un
  ingress controller en 80/443; no es un ALB.
- **Conformance 413/419**, frente a 416/419 de k3s. Cinco de los seis
  fallos son solo de macOS y pasan en Linux.

## Lo que un equipo pequeño necesita de verdad

Haz el ejercicio con tu equipo. De todo lo que trae un cluster gestionado,
¿qué usas cada semana?

Lo que casi todo equipo usa:

- Deployments con rollouts y rollback, réplicas, health checks.
- Services, DNS interno, un ingress hacia fuera.
- ConfigMaps y Secrets.
- Jobs y CronJobs.
- Helm, y si haces GitOps, Argo CD.
- Métricas, logs y alguna alerta que te despierte.
- Más de una máquina, y que si una cae, los pods se muevan.

Todo eso está en litek8s, con la API real: es lo que miden los 119 casos
del corpus de compatibilidad (kubectl, CRDs, cargas, red y 47 charts de
Helm) y la conformance oficial.

Lo que de verdad justifica el transbordador:

- Varios equipos en el mismo cluster, entrando con el SSO de la empresa y
  con un registro de auditoría de quién hizo qué.
- Autoescalado de nodos y upgrades del control plane gestionados por otro.
- Integración profunda con la nube: IAM por pod, volúmenes de bloque,
  load balancers gestionados.
- Requisitos de compliance que piden un proveedor y un SLA con firma.

Si tu lista se parece a la primera, el "80 % de las ventajas de un cluster"
no es una exageración. No es una cifra medida, es una estimación; pero
mira qué hay en el 20 % restante: sobre todo seguridad multi-equipo y
operaciones delegadas. Son justo las cosas que no necesitas el primer año
y que sí necesitas cuando ya hay dinero y gente para pagarlas.

## Empezar pequeño sin quedarte atrapado

Lo que hace que esta estrategia funcione no es que litek8s sea rápido, es
que **es la API de Kubernetes**. No hay formato propio de manifiestos, ni
anotaciones obligatorias, ni SDK. Lo que aplicas a litek8s lo aplicas a
EKS, y al revés ([Junto a tus clusters](/es/docs/alongside/)).

En la práctica, migrar se parece a esto:

1. Creas el cluster nuevo (EKS, GKE, lo que toque). Su contexto aparece en
   el mismo `~/.kube/config` junto al de litek8s.
2. Apuntas los mismos charts, o la misma Application de Argo CD, al
   cluster nuevo.
3. Cambias el DNS.
4. Cuando todo está estable, `sudo litek8s uninstall --purge` en tus
   máquinas.

Lo honesto: **los datos son la parte difícil**, en litek8s y en
cualquier migración. litek8s provisiona volúmenes locales en el disco del
nodo; moverlos a volúmenes de la nube es trabajo tuyo (un dump y restore de
la base de datos, rsync, lo que use tu app). Por eso mi recomendación es
que, si puedes, la base de datos viva fuera del cluster desde el día uno
(un Postgres gestionado o una VM con backups), y el cluster solo tenga
cosas sin estado. Así migrar es casi solo cambiar de contexto.

## Cuándo es hora de migrar

Señales concretas, no una sensación:

- Más de un equipo necesita acceso al cluster y repartir certificados de
  cliente a mano ya no escala: quieres SSO y logs de auditoría.
- Un cliente o una auditoría te pide un SLA o compliance que no puedes
  firmar tú.
- Pasas más tiempo gestionando máquinas que entregando producto.
- Necesitas escalar nodos automáticamente con la carga.

Si no te reconoces en ninguna, probablemente sigues en la fase en la que un
binario, unas cuantas máquinas baratas y la API de siempre te bastan.

## En resumen

litek8s es rápido porque hace menos: un proceso en vez de ocho, un store en
memoria en vez de etcd, un pod sin contenedor pause, observabilidad dentro
en vez de al lado. Eso tiene un precio real, sobre todo un control de acceso
más simple que el de upstream y la juventud del proyecto, y conviene
conocerlo antes de usarlo con un equipo.

Pero la idea de fondo no depende de litek8s: **escribe contra la API
estándar, empieza con lo mínimo que te sirva, y deja la puerta abierta**.
El día que necesites el transbordador, tus manifiestos ya saben volar.
