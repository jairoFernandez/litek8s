---
title: "Clonar un cluster de producción en litek8s: lo que nos enseñó Kubernetes"
description: "Copiamos un cluster de producción en marcha a litek8s sin tocarlo. Cuatro bugs, dos piezas que le faltaban y qué dice cada una de cómo funciona Kubernetes."
date: 2026-10-29
tags: [migración, rbac, networkpolicy, gitops]
docs: [auth, networking, clusters, agents]
lang: es
translation: cloning-a-production-cluster
---

La pregunta era simple: ¿podía un cluster de producción real (k3s en un
proveedor cloud, GitOps con Argo CD, Vault y external-secrets, varias APIs de
producto con sus MongoDB, Postgres, Redis y MinIO, n8n, runners de CI) correr
sobre litek8s? La única forma honesta de responderla era construir una copia,
a partir de su propio repositorio GitOps y de los backups de la noche
anterior, y ver qué se rompía.

Se rompió bastante. La mayor parte no era culpa de litek8s, y lo que sí lo era
nos enseñó más sobre Kubernetes que cualquier lectura de la referencia de la
API.

## Regla uno: la copia no puede tocar producción

Una copia de un cluster es una copia de sus efectos secundarios. Los mismos
Secrets que le permiten a un backend cobrar una tarjeta, mandar un correo o
contestar por WhatsApp funcionan igual de bien desde un segundo cluster. Antes
de que corriera nada, listamos lo que podía llegar al mundo exterior y cerramos
cada puerta:

- **Credenciales**: Vault se restauró desde su snapshot de raft y después se
  sobrescribió, en la Vault de la copia, cada llave de terceros (pagos, correo,
  APIs de LLM, mensajería, almacenamiento cloud, el bucket de backups, DNS), y
  también cada valor con una URL a un host fuera del cluster. Esa última regla
  atrapó una cadena de conexión que apuntaba a un MongoDB administrado que
  habíamos olvidado.
- **Jobs programados**: todos los CronJobs suspendidos. Uno de ellos corría
  `rclone sync` contra el bucket de backups y borraba archivos viejos: una
  copia que lo ejecutara habría sobrescrito los backups reales.
- **Tokens guardados en bases de datos**: los motores de workflows y las apps
  multi-tenant guardan los tokens OAuth y de mensajería de sus usuarios en sus
  propias bases, donde neutralizar Vault no llega. Esos deployments arrancaron
  en cero réplicas.
- **Controladores que actúan sobre el mundo**: external-dns, el operador de
  Terraform, los runners de CI y la automatización de PRs quedaron fuera.
- **Exposición**: el ingress de la copia respondía solo por la VPN, con
  hostnames como `app.<vpn-ip>.sslip.io` y certificados de la CA propia de la
  copia.

Al cluster de producción solo se le leyó, a través de un wrapper de `kubectl`
que rechaza cualquier verbo que no sea `get`, `logs` y similares.

## Lo que GitOps no registra

Un repositorio GitOps describe el estado estable del cluster, no cómo llegó
ahí. Reconstruirlo solo a partir de él encontró tres cosas que ningún cluster
en marcha muestra:

- Una sync wave que contenía un `ExternalSecret` iba antes que la wave que
  instala su CRD. En el cluster vivo la CRD llevaba meses ahí; en uno nuevo la
  sincronización se traba, también con Argo CD.
- El chart de external-secrets corría con `serviceAccount.create: false`; la
  ServiceAccount que necesitaba la creó una vez el bootstrap del instalador y
  nunca quedó escrita.
- La autenticación de Kubernetes de Vault apuntaba a la dirección pública de
  la API del cluster viejo, así que una Vault restaurada validaba tokens contra
  el cluster equivocado.

## Bug 1: `replace` de JSON Patch y las etiquetas de Vault

Vault, en modo HA, etiqueta su propio pod (`vault-active`, `vault-sealed`)
para que un Service pueda seleccionar el nodo activo. En litek8s las etiquetas
nunca aparecían, el Service `vault-active` no tenía endpoints y la UI
respondía 503. El log decía que el PATCH del pod se rechazaba con 422.

Vault envía un JSON Patch con `"op": "replace"` para etiquetas que todavía no
existen. El RFC 6902 dice que eso debería fallar. Kubernetes usa
[evanphx/json-patch](https://github.com/evanphx/json-patch) v4, que deja que
`replace` cree un miembro que falta en un objeto existente, y Vault depende de
eso. litek8s ahora hace lo mismo: un miembro que falta en un objeto se crea; un
padre que falta o un índice de array fuera de rango sigue siendo un error. La
compatibilidad con Kubernetes muchas veces consiste en los bugs de los que la
gente depende.

## Bug 2: `kubectl exec` que moría al minuto

Restaurar una base de datos pasando un dump por `kubectl exec -i` fallaba
después de unos sesenta segundos con `next reader: i/o timeout`, incluso con
datos fluyendo.

Desde la 1.30, el exec de kubectl va por WebSocket (`v5.channel.k8s.io`) y
client-go envía un ping cada pocos segundos, y cierra el stream si no llega un
pong a tiempo. El lector WebSocket de litek8s se saltaba los frames de
control, así que los pings quedaban sin respuesta. Ahora contesta cada ping
con un pong, escrito como un frame completo bajo el mismo lock que los datos.
Un exec silencioso de noventa segundos y una subida de 60 MB por stdin pasan.

## El bloqueo de verdad: todos eran admin

Cualquier cliente que llegaba a un servidor litek8s era administrador del
cluster, con o sin token. En una laptop eso es cómodo. En un cluster que corre
workflows de n8n y jobs de CI, cualquier pod podía leer todos los Secrets,
incluidas las llaves de unseal de Vault. Así que los servidores litek8s ahora
[autentican y autorizan](/es/docs/auth/) como lo hace un API server de
Kubernetes:

- **Quién es quién.** Los pods presentan el token de su service account; se
  verifican la firma, la cuenta, el pod al que está ligado y su audiencia (un
  token emitido para Vault se rechaza). Las personas presentan un certificado
  de cliente de la CA del cluster: el `CN` es el usuario, cada `O` un grupo, y
  `system:masters` puede todo. Los nodos usan su join token.
- **Qué pueden hacer.** Roles y ClusterRoles, con subrecursos como
  `pods/exec`, nombres de recursos y comodines, las ClusterRoles por defecto
  con aggregation, y el chequeo de escalación que impide conceder lo que no
  tienes. `kubectl auth can-i` responde de verdad.
- **Cómo activarlo sin romper nada.** `RBAC-audit` decide, registra
  `DENY user=... get secrets namespace=...` y permite igual. Corrimos la copia
  en modo audit, no encontramos ningún workload que necesitara más de lo que
  sus charts concedían, y pasamos a `RBAC`. La service account `default` de un
  namespace de producto ya no puede leer los Secrets de Vault ni crear pods en
  `kube-system`; litecd, external-secrets, cert-manager y Vault funcionan sin
  cambios, porque sus charts traen su RBAC.

La lección para cualquier cluster: un ClusterRoleBinding a `admin` "para el
runner de CI" no significaba nada mientras todos eran admin. Con RBAC significa
que el runner de CI puede leer todos los Secrets de todos los namespaces.

## NetworkPolicy sin plugin CNI

Los charts protegían sus bases de datos con NetworkPolicies ("solo la API y el
job de backup pueden llegar a este Postgres"), y nada las aplicaba. litek8s no
tiene un plugin CNI en quien delegar, así que
[hace la selección él mismo](/es/docs/networking/#networkpolicies): el API
server convierte los selectores de cada policy en direcciones de pods y
puertos, le envía a cada nodo las reglas de sus pods, y el agente del nodo
escribe una cadena de iptables por cada pod aislado. Las reglas de un pod están
puestas antes de que arranque su primer contenedor, así que no hay ventana en
la que un pod seleccionado quede abierto. Las respuestas siempre pasan; el
tráfico del propio nodo (probes, NodePorts) se permite, como lo necesita el
kubelet.

En la copia, un pod que no está entre los clientes permitidos de una base de
datos ya no obtiene nada de ella, mientras que la aplicación a la que
pertenece sigue conectándose.

## En la 0.2.12

Todo lo anterior (los arreglos de JSON Patch y WebSocket, la autenticación y
RBAC con el modo audit, la aplicación de NetworkPolicies) sale en litek8s
0.2.12. Para actualizar un servidor creado con una versión anterior, lee
primero
[Actualizar un cluster creado con una versión anterior](/es/docs/auth/#actualizar-un-cluster-creado-con-una-versión-anterior):
los contextos con el token de relleno viejo reciben 401.

## Lo que falta

La copia funciona. Antes de que pudiera reemplazar a la original: volúmenes
que sobrevivan a la pérdida de una máquina (los PVs local-path están atados al
disco de un nodo), un balanceador con una dirección estable para el DNS,
credenciales de registry por pod en lugar de por nodo, y una UI web para el
controlador GitOps. Y quizá el resultado más útil sea la copia misma:
reconstruida periódicamente desde los backups y el repositorio GitOps, es un
simulacro de restauración que prueba que los backups funcionan.
