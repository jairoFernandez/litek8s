---
title: "El pod que espera: cómo funciona Pending y el plazo que nunca vencía"
description: "Qué significa que un pod esté en Pending, cómo el scheduler decide si cabe, y el bug que dejaba a un Job esperar para siempre aunque tuviera plazo."
date: 2026-11-04
tags: [scheduling, jobs, controladores, pending]
docs: [multi-node, dev]
lang: es
translation: the-pod-that-waits
---

Un pod en `Pending` no está roto: está esperando. Kubernetes lo usa como una
cola implícita. Si un pod pide más de lo que hay libre, se queda quieto
hasta que algo termine y le haga espacio. Esa espera es útil, pero tiene una
trampa con los Jobs que tienen plazo. Y en litek8s encontramos que ese plazo,
mientras el pod esperaba, nunca vencía.

Este artículo explica cómo se maneja `Pending`, qué medimos en un cluster
real y qué cambió en la 0.2.21.

## Dos clases de Pending

`Pending` es una fase del pod, y cubre dos situaciones distintas:

1. **Nadie lo ha ubicado.** Ningún nodo tiene espacio, o ninguno cumple sus
   reglas (afinidad, taints, selectores). El pod no tiene `nodeName` y su
   condición `PodScheduled` está en `False`, con razón `Unschedulable`.
2. **Ya tiene nodo, pero sus contenedores aún no arrancan.** Lo típico es la
   descarga de la imagen: el contenedor queda en `waiting` con razón
   `ContainerCreating` o, si falla, `ErrImagePull`.

`kubectl get pods` muestra `Pending` en ambos casos. Para saber cuál es,
mira la condición y el estado del contenedor:

```bash
kubectl get pod <pod> -o jsonpath='{.status.conditions[?(@.type=="PodScheduled")].message}'
kubectl get pod <pod> -o jsonpath='{.status.containerStatuses[0].state}'
```

Este artículo trata sobre todo de la primera clase: el pod que no cabe.

## Cómo decide el scheduler si un pod cabe

El scheduler no mira cuánta CPU o memoria se está **usando**. Mira cuánto
**pidieron** los pods (`resources.requests`) frente a lo que el nodo ofrece
(`status.allocatable`). Si la suma de lo pedido por los pods del nodo, más
lo que pide el nuevo, pasa lo asignable, el pod no cabe en ese nodo.

En litek8s eso vive en `src/kube/controllers/scheduler.rs`:

- `pod_requests` suma los `requests` de CPU y memoria de todos los
  contenedores, o toma el init container más grande si pide más (los init
  corren uno a la vez).
- Para cada nodo, compara lo pedido con `allocatable` y anota la razón si no
  cabe: `Insufficient cpu`, `Insufficient memory` o el recurso extendido que
  falte.
- Si ningún nodo sirve, marca el pod con `PodScheduled=False`, razón
  `Unschedulable`, y un mensaje como
  `0/1 nodes are available: Insufficient cpu.` Si el pod tiene más prioridad
  que otros ya ubicados, intenta desalojar a los de menor prioridad.

Los controladores de litek8s se mueven por el flujo de cambios del store:
cuando un pod termina y libera su espacio, ese cambio vuelve a poner en
marcha al scheduler, que intenta otra vez con los pods que esperan.

Un pod que nunca pide recursos (sin `requests`) siempre cabe, aunque el nodo
esté saturado. Por eso conviene declararlos: son lo único que el scheduler
sabe de tu pod. El artículo [El nodo demasiado
lleno](/es/blog/el-nodo-demasiado-lleno/) cuenta qué pasa cuando lo pedido y
lo usado se separan.

## Lo que medimos

En un cluster de desarrollo de un nodo con 7 CPU asignables creamos cuatro
Jobs. Cada uno pide 3 CPU:

- `a` y `b` duermen 40 segundos.
- `c` duerme 5 segundos.
- `dl` duerme 5 segundos y tiene `activeDeadlineSeconds: 15`.

Caben dos (6 de 7 CPU). Así se vieron cada 5 segundos:

| segundo | a | b | c | dl |
|---|---|---|---|---|
| 5 a 35 | Running | Running | Pending | Pending |
| 40 | Succeeded | Succeeded | Running | **DeadlineExceeded** |
| 45 | Succeeded | Succeeded | Succeeded | DeadlineExceeded |

Los pods `c` y `dl` mostraban `0/1 nodes are available: Insufficient cpu.`.
Cuando `a` y `b` terminaron, `c` arrancó solo: la cola implícita funciona.

Pero `dl` tenía un plazo de 15 segundos y siguió activo hasta el segundo 40.

## El plazo de un Job cuenta desde que existe

`activeDeadlineSeconds` cuenta desde `status.startTime` del Job, que se
escribe cuando el controlador lo ve por primera vez, no cuando su pod
arranca. **El tiempo en Pending cuenta.** En Kubernetes estándar, un Job con
15 segundos de plazo cuyo pod no cabe falla a los 15 segundos con
`DeadlineExceeded`, aunque nunca haya corrido.

Eso tiene una consecuencia práctica: si tu sistema crea muchos Jobs de
golpe y deja que el cluster los encole, los últimos pueden morir por plazo
antes de correr. Con el plazo apagado tampoco se arregla: sin plazo, un pod
que nunca cabe espera para siempre.

## Por qué en litek8s no vencía

El controlador de Jobs de litek8s ya calculaba el plazo correctamente:
compara `startTime` con la hora actual y, si se pasó, falla el Job y borra
sus pods. El problema era **cuándo** hacía esa cuenta.

Los controladores reaccionan a cambios. Un Job se revisa cuando él o sus
pods cambian. Un pod en Pending porque no cabe no cambia: no lo ubican, no
arranca, no termina. Así que nada traía al controlador de vuelta al Job, y
el plazo se quedaba sin revisar. En la tabla, `dl` falló en el segundo 40
solo porque el fin de `a` y `b` provocó una revisión.

Las condiciones que dependen solo del paso del tiempo necesitan un
temporizador. El controlador ya lo tenía para otras dos cosas: la espera
entre reintentos (backoff) y `ttlSecondsAfterFinished`. Le faltaba para el
plazo.

## La corrección

Al revisar un Job activo que tiene plazo, el controlador ahora pide que lo
vuelvan a revisar cuando el plazo venza. Si ya había un reintento agendado
antes, gana el más próximo:

```rust
// Come back when the deadline passes: a pod left Pending (nothing
// fits) changes nothing, so no event would bring the controller back.
let deadline_left = i(&job, "/spec/activeDeadlineSeconds").and_then(|d| {
    let start = status.get("startTime").and_then(Value::as_str).and_then(util::parse_time)?;
    Some((d - (now - start).num_seconds()).max(1) as u64)
});
if let Some(left) = deadline_left {
    let left = Duration::from_secs(left);
    requeue = Some(requeue.map_or(left, |r: Duration| r.min(left)));
}
```

Cada clave de trabajo tiene como máximo un temporizador pendiente, así que
esto no acumula revisiones.

Para comprobarlo usamos `litek8s apiserver --no-node`: un servidor sin
nodos, donde todo pod se queda en Pending. Creamos un Job con 10 segundos de
plazo en la 0.2.20 y en la rama con la corrección, y miramos a los 14
segundos:

| versión | estado del Job | pods |
|---|---|---|
| 0.2.20 | activo | 1 en Pending |
| con la corrección | `Failed`, `DeadlineExceeded` | 0 |

El test `an_active_job_is_requeued_for_its_deadline` cubre el caso: un Job
activo con plazo vuelve a la cola con el tiempo que le queda.

## Qué hacer con esto en tu sistema

Pending es una buena red de seguridad, pero no una cola de trabajo. Si
creas Jobs con plazo en ráfagas, tienes dos opciones:

- **Que el plazo no incluya la espera:** no crees el Job hasta que haya
  espacio. Es lo que hicimos en xsec, un servicio que corre escaneos como
  Jobs: los escaneos se guardan en cola en su propia base de datos y la API
  crea Jobs solo mientras haya menos de N corriendo. El Job, y su plazo,
  empiezan cuando hay lugar.
- **Que el plazo sí la incluya, a propósito:** si un trabajo que no arranca
  en 10 minutos ya no sirve, el plazo con la espera incluida es justo lo que
  quieres. Desde la 0.2.21, litek8s lo cumple igual que Kubernetes.

En ambos casos, declara `requests`. Son lo único que el scheduler usa para
decidir si un pod cabe.

## Cómo diagnosticar un pod en Pending

```bash
# ¿Lo ubicaron? Si no, por qué.
kubectl describe pod <pod> | sed -n '/Conditions/,/Events/p'

# Lo que piden los pods de un nodo frente a lo que ofrece.
kubectl describe node <nodo> | sed -n '/Allocated resources/,/Events/p'

# Jobs que vencieron por plazo.
kubectl get jobs -o custom-columns=NAME:.metadata.name,REASON:.status.conditions[0].reason
```

Si el pod ya tiene nodo y sigue en Pending, el problema casi siempre es la
imagen: revisa `containerStatuses[0].state.waiting`.
