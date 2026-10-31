---
title: "Pequeño no basta: medir la velocidad de litek8s, y un list 13× más rápido"
description: "Teníamos números de memoria y disco, ninguno de velocidad. Un load test del API server, un list 13× más rápido y cómo CI ahora detecta regresiones."
date: 2026-10-30
series: speed
tags: [rendimiento, benchmarks, api-server, ci, rust]
docs: [numbers, contributing]
lang: es
translation: small-is-not-enough
---

Todos los números que habíamos publicado sobre litek8s eran de **tamaño**:
128 MiB de memoria con el cluster vacío frente a los 572 de k3s, un binario
de 22 MiB, 285 MiB de disco. Salían de `bench/run.sh`, que arranca una VM
nueva de 1 GiB por sistema y la mira en reposo. Nada en el repositorio
decía qué tan **rápido** responde el API server cuando kubectl, Helm, Argo
CD y unos cuantos controllers le hablan a la vez.

Así que nos pusimos una meta: litek8s tiene que ser veloz, no solo
pequeño. El primer paso era poder medirlo.

## Dos benchmarks, sin VM

Los dos corren en un portátil con `cargo bench`, sin Docker ni VM.

**`benches/micro.rs`** usa criterion sobre los caminos calientes: el store
de objetos (create, get, update, list, escritura y relectura del WAL, una
escritura repartida a 100 watchers), los patches merge y strategic, y JSON.

**`benches/api_load.rs`** arranca un `apiserver` completo dentro del mismo
proceso, con el WAL en disco y todos los controllers corriendo, y lo carga
por HTTP/1.1 keep-alive de verdad, con 32 clientes a la vez. Cada escenario
reporta ops/s y p50/p90/p99:

| escenario | qué hace |
|---|---|
| `get`, `create`, `patch` | un ConfigMap por petición |
| `list_all` | 1.000 ConfigMaps |
| `list_selector` | `labelSelector`: 100 de los 1.000 |
| `list_table` | lo que pide `kubectl get` (una Table del lado del servidor) |
| `watch_latency` | de un create hasta que llega su evento de watch |
| `schedule` | un Deployment de 100 réplicas hasta que cada Pod tiene Node |

La primera baseline, en un portátil Apple Silicon:

| escenario | ops/s | p99 |
|---|---|---|
| get | 73.600 | 0,57 ms |
| create | 23.300 | 3,8 ms |
| patch | 19.100 | 4,7 ms |
| list_selector | 865 | 91 ms |
| list_all | **507** | **33 ms** |
| list_table | 335 | 41 ms |
| watch (create → evento) | | 0,25 ms |
| 100 réplicas → todas asignadas | | 275 ms en total |

Las escrituras y las lecturas sueltas se veían bien. Los list no: 507 list
de 1.000 objetos pequeños por segundo, y un label selector que devuelve la
décima parte de los objetos era apenas el doble de rápido que devolverlos
todos. Las dos cosas sugerían que el costo no dependía de lo que se
mandaba de vuelta.

## En qué se iba el tiempo de un list

Los micro-benchmarks lo contaron. Listar desde el store costaba cerca de
**1,1 µs por objeto**, igual con 100, 1.000 o 10.000 objetos, y más o
menos lo que cuesta parsear un objeto pequeño desde JSON. El store guarda
cada objeto como bytes de JSON compacto. En cada list el servidor:

1. copiaba los bytes del store,
2. **parseaba cada objeto** a un `serde_json::Value`,
3. filtraba por los selectores,
4. armaba la List,
5. y **lo serializaba todo de nuevo** al mismo JSON del que partió.

Los pasos 2 y 5 son un viaje de ida y vuelta que termina donde empezó.
Para un recurso built-in, el JSON guardado es justo lo que sale por la red.

## El arreglo: mandar los bytes que ya tienes

Los list de recursos built-in ahora van por un camino aparte,
`list_stored`. Cubre todo list que no pida Table ni
PartialObjectMetadata y que no sea de un custom resource ni de los dos
tipos de certificados servidos en dos versiones. Copia los bytes guardados
directo a la respuesta:

```rust
out.extend_from_slice(br#"{"apiVersion":"#);
serde_json::to_writer(&mut out, &rt.api_version())?;
out.extend_from_slice(br#","items":["#);
for (i, (_, json)) in items.iter().enumerate() {
    if i > 0 { out.push(b','); }
    out.extend_from_slice(json);
}
// ...luego "kind" y "metadata", en el orden en que los escribe serde_json
```

Los selectores todavía tienen que leer algo. Un label selector solo
parsea únicamente `metadata.labels`, en un struct pequeño; serde salta lo
demás sin reservar memoria. Los field selectors, menos comunes, siguen
parseando el objeto entero. La paginación (`limit`, `continue`) trabaja
sobre las claves del store y la comparten los dos caminos. La respuesta es
byte por byte la misma de antes.

| escenario | antes | después |
|---|---|---|
| list_all | 507 ops/s, p99 33 ms | **6.616 ops/s, p99 2,1 ms** (×13) |
| list_selector | 865 ops/s, p99 91 ms | **5.667 ops/s, p99 11 ms** (×6,5) |
| list_table | 335 ops/s | 412 ops/s (+23%) |

`kubectl get` (la Table) todavía parsea cada objeto, porque armar sus
filas necesita los campos. Es el siguiente objetivo.

## Detectar regresiones sin engañarnos

`api_load` compara cada corrida con `bench/perf-baseline.json` y sale con
código 1 si un escenario pierde más de 15% de sus ops/s o si su p99 crece
lo mismo. Las primeras corridas nos enseñaron tres cosas sobre esa
comparación.

**Una corrida es ruido.** El mismo código, tres corridas seguidas: patch
dio 22.397, luego 20.139, luego 11.535 ops/s. Con una sola corrida
saltarían falsas alarmas, así que cada escenario ahora corre tres veces y
se queda con la mediana de cada cifra por separado. La cola es todavía más
ruidosa: con la máquina ocupada, el p99 del mismo código se duplicaba entre
corridas, así que el p99 tiene el doble de margen que el throughput.

**La máquina pesa más que el código.** Una comparación mostró todos los
escenarios 40% más lentos, incluido código que no habíamos tocado. El load
average estaba en 25: había otros compiladores corriendo. `api_load` ahora
avisa cuando la carga pasa de la mitad de las CPUs y pide no guardar
baseline.

**Una baseline solo vale en su propia máquina.** Un JSON grabado en un
portátil no dice nada de un runner compartido de CI. Por eso CI compara
cada pull request con su base **en el mismo runner, en el mismo job**:
`bench/perf-compare.sh` saca la base en un git worktree, corre su
`api_load` hacia una baseline temporal y luego corre el pull request
contra ella. Una regresión de más de 20% se mide otra vez, base y luego
rama, y el job falla solo si **el mismo escenario** regresó las dos veces:
nuestra primera versión fallaba con "alguna regresión en cada intento", y
comparando código idéntico marcó `list_selector` una vez y `list_all` la
siguiente. Las tablas quedan
en el resumen del job. El mismo script corre en local:

```bash
bench/perf-compare.sh main
```

Comparar el código consigo mismo fue la mejor prueba de la comparación, y
encontró un bug en el benchmark. Con tres corridas por escenario, la
segunda corrida de `create` reusaba los nombres de la primera: decenas de
miles de 409 AlreadyExists, contados en silencio como errores junto a
números que parecían razonables. Ahora los nombres son únicos entre
corridas y cualquier petición fallida cuenta como regresión.

## Lo que sigue

- **`kubectl get` (Tables)**: armar las filas desde un parseo parcial, no
  desde el objeto entero.
- **Controllers bajo carga de escritura**: durante `create`, los
  controllers registran "fell behind the change feed, resyncing". Un
  resync recorre todo el store; un buffer más grande o eventos más baratos
  deberían mantenerlos al día.
- **Un store más grande**: los escenarios corren con unos mil objetos por
  namespace; los que recorren el store también deberían correr con
  100.000.

Pequeño fue la primera promesa. Veloz ahora se mide, y se revisa en cada
pull request.
