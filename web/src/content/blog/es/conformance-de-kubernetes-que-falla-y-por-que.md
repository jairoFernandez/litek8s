---
title: "Corrimos la suite oficial de conformance de Kubernetes en litek8s: qué pasa, qué falla y por qué"
description: "413 de 419 specs de conformance, contra 416 de k3s. Las seis fallas, la primera corrida que sacó cero y qué dice cada una sobre Kubernetes."
date: 2026-10-30
tags: [conformance, kubernetes, pruebas, compatibilidad]
docs: [numbers, limitations, crds, auth]
lang: es
translation: kubernetes-conformance-what-fails-and-why
---

"Compatible con Kubernetes" es fácil de decir. El proyecto Kubernetes trae
una forma de comprobarlo: la suite de conformance, los specs
`[Conformance]` del binario `e2e.test` de upstream que toda distribución
certificada tiene que pasar. La corrimos contra litek8s, un API server de
Kubernetes en un solo binario de Rust, y guardamos todos los resultados.

El puntaje, Kubernetes **v1.33.13**, 419 specs:

| | pasaron | tiempo total |
|---|---|---|
| k3s v1.33.4 (k3d, referencia) | 416/419 | 39 min |
| **litek8s, macOS (runtime Docker)**, última corrida completa | **413/419** | 29 min |
| litek8s, Linux (runtime crun), los specs que macOS no puede correr | 9/9 | 1 min |

Seis fallas. Cinco vienen de macOS, no de la API; una era un bug de verdad,
ya corregido. Aquí va cada una, cómo llegamos hasta acá desde una primera
corrida que sacó cero, y qué nos enseñaron sobre Kubernetes.

## La primera corrida: 0 de 419

La primera corrida nunca llegó a un test. Se detuvo en `BeforeSuite`, el
paso de preparación, porque la suite lista los nodos con
`fieldSelector=spec.unschedulable=false` y litek8s devolvía una lista
vacía.

Nuestros nodos simplemente no tenían `spec.unschedulable`. En Kubernetes,
un campo ausente tiene que coincidir con `false`. Esa regla no está escrita
como regla en ninguna parte: sale de los tipos de Go, donde un booleano que
falta es el valor cero y ya. Fue la primera de muchas lecciones con la misma
forma: **el comportamiento de Kubernetes es el comportamiento de su código
Go**, y una reimplementación tiene que calzar con el código, no con la
documentación.

De ahí en adelante, cada corrida arregló lo que la anterior dejó al
descubierto:

| corrida | pasaron | tiempo total |
|---|---|---|
| r1 | 0/419 | (detenida) |
| r2 | 271/419 | 77 min |
| r3 | 346/419 | 59 min |
| r4 | 388/419 | 40 min |
| r5 | 405/419 | 47 min |
| r6 | 412/419 | 27 min |
| r7 | 413/419 | 29 min |

## Lo que hizo falta

Cuatro áreas, más o menos en el orden en que la suite nos las fue
exigiendo.

**Formatos en el cable.** kubectl y client-go hablan protobuf, no JSON. En
protobuf los structs embebidos van aplanados, algunos nombres de campo no
son los del JSON, y los valores cero (que siempre viajan) hay que
descartarlos salvo que el campo en Go sea un puntero. Terminamos generando
esa lista de campos puntero a partir de los tipos de Go. Otra vez: el código
Go es la especificación.

**Maquinaria de la API.** Un intérprete de CEL para ValidatingAdmissionPolicy
y los `matchConditions` de los webhooks; esquemas de CRD con pruning,
defaults, validación y OpenAPI; webhooks de conversión; tokens de
continuación de listas que expiran; reanudación de watches; borrado
graceful, en primer plano y huérfano; ResourceQuota y LimitRange; tokens de
service accounts; y agregación de APIs.

**Controladores.** ReplicationControllers, ControllerRevisions para
StatefulSets y DaemonSets, políticas de fallo y de éxito de los Jobs,
PodDisruptionBudgets y desalojo, preemption en el scheduler, taints.

**El agente de nodo.** Lifecycle hooks, mensajes de terminación,
contenedores efímeros, sysctls, el `/etc/hosts` y el `resolv.conf` que
escribe el kubelet, variables de entorno de los services, el orden de
arranque de los contenedores, conflictos de hostPort, afinidad de sesión,
los subrecursos `proxy` y los registros SRV, PTR y ExternalName en el DNS
del cluster.

Nada de esto es exótico. Cada cosa es algo de lo que depende alguna carga en
algún lado, y por eso la suite lo revisa.

## Las fallas, una por una

Por SIG, la última corrida completa contra k3s:

| SIG | k3s | litek8s |
|---|---|---|
| api-machinery | 93/94 | 93/94 |
| apps | 59/60 | 60/60 |
| architecture | 1/1 | 1/1 |
| auth | 10/10 | 10/10 |
| cli | 17/17 | 17/17 |
| instrumentation | 4/4 | 4/4 |
| network | 43/43 | 38/43 |
| node | 89/89 | 89/89 |
| scheduling | 10/11 | 11/11 |
| storage | 90/90 | 90/90 |

### DNS por TCP (4 specs, solo macOS)

Cada spec de DNS pone a un pod a resolver todos los nombres por UDP **y**
por TCP. La mitad TCP se suele olvidar, pero un cliente DNS se pasa a TCP
cuando la respuesta no cabe en un paquete UDP, así que la suite lo exige.

El DNS del cluster responde por TCP en Linux. En macOS, el puerto 53 es del
resolver del propio sistema, mDNSResponder: escuchar en la dirección
comodín falla con `EADDRINUSE`, y escuchar en una dirección concreta
requiere root. UDP funciona, así que la mitad UDP pasa y la TCP falla. En
Linux con crun pasan todos los specs de DNS.

### HostPort (1 spec, solo macOS)

Tres pods piden el mismo `hostPort`, cada uno con un `hostIP` o un
protocolo distinto. En Kubernetes eso no es un conflicto: un puerto del host
es en realidad la terna de dirección, puerto y protocolo. En macOS, Docker
Desktop y OrbStack corren los contenedores en una VM cuyas direcciones no
son las del nodo, así que no se puede abrir el puerto en la dirección del
nodo. En Linux con crun pasa.

### Un watch que se quedó atrás (1 spec, corregido)

La última falla era nuestra: `Watchers should observe add, update, and
delete watch notifications`. Una ráfaga de escrituras le ganó al feed de
cambios y el watch se cortó. Un watch de Kubernetes es una promesa: vas a
ver cada cambio desde un resourceVersion en adelante, en orden; cortar la
conexión la rompe. Ahora los watches se ponen al día desde el historial del
store, y la corrida siguiente pasó el spec.

Esa corrida siguiente, con el código final, solo falló los cinco specs de
macOS en su fase paralela (385/390). Su fase serial de 29 specs quedó a
medias cuando se llenó el disco del host y Docker se detuvo, así que no
tiene puntaje completo. Esos 29 specs pasaron en las dos corridas
anteriores.

## k3s también falló tres

La referencia tampoco es perfecta. k3s falló un patch de `Namespaces` y un
spec de DaemonSet por timeout, y un spec de preemption perdió la conexión.
Esas corridas compartían la misma VM de Docker con corridas de litek8s, así
que parecen carga, no huecos. Lo contamos porque un puntaje de conformance es
una medición en una máquina, no solo una propiedad del código.

## La suite le pega duro al nodo

Un spec crea Deployments en un loop hasta que una política de admisión con
CEL los rechaza. Antes de que litek8s evaluara esas políticas, el loop no
paraba: pasó por unos 6,000 pods y dejó a Docker colgado. Desde entonces el
agente de nodo además barre, cada minuto, los contenedores de pods que ya no
conoce.

## Lo que el puntaje no dice

Preferimos decirlo nosotros antes que leerlo en los comentarios:

- **Las corridas usaron un store en memoria.** Conformance no prueba
  durabilidad, y aquí tampoco la probamos.
- **Fuera de conformance y sin probar aquí:** durabilidad respaldada por
  etcd, control planes en HA, proveedores cloud, nodos Windows.
- **Sin autorización durante las corridas.** Las notas registran que cada
  SubjectAccessReview se permitía. La aplicación de RBAC llegó después
  ([Autenticación y RBAC](/es/docs/auth/)).
- **Conformance no es "mis cosas corren".** Para eso mantenemos un corpus
  aparte de flujos de kubectl y Helm, con 47 charts populares instalados con
  `--wait`; pasa 119/119, igual que k3s ([Números](/es/docs/numbers/)).
- Una corrida enfocada en los specs de CRDs y server-side apply, de
  conformance y otros, pasó 48/48, igual que k3s
  ([CRDs y server-side apply](/es/docs/crds/)).

El resto de lo que litek8s no hace está en
[Limitaciones](/es/docs/limitations/).

## Pruébalo

El runner descarga `e2e.test` y `ginkgo` v1.33.13 y los corre desde el host
como lo hace la imagen oficial de conformance. Desde un checkout:

```bash
cargo build --release
target/release/litek8s apiserver --kubeconfig /tmp/litek8s.kubeconfig &
target/release/litek8s node-agent --server http://127.0.0.1:6443 \
  --node-name "$(hostname -s | tr A-Z a-z)-2" --runtime docker &
KUBECONFIG=/tmp/litek8s.kubeconfig compat/conformance.sh --target litek8s
KUBECONFIG=/tmp/litek8s.kubeconfig compat/conformance.sh --target litek8s \
  --focus 'Aggregator.*\[Conformance\]' --no-serial   # one spec
```

El segundo agente de nodo está porque algunos specs (anti-afinidad de pods,
DaemonSets, preemption) necesitan dos nodos donde programar pods. Cuenta con
una media hora para la suite completa. El detalle completo, con referencias a los
resultados JUnit crudos y los logs de ginkgo, está en
[compat/CONFORMANCE.md](https://github.com/jairoFernandez/litek8s/blob/main/compat/CONFORMANCE.md).
Si encuentras un spec que nos pasa y no debería, queremos saberlo.
