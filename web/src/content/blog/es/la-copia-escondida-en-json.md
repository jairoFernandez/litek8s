---
title: "kubectl get 2,4× más rápido: la copia escondida en json!"
description: "Un perfil de la Table de kubectl get mostró la mitad del tiempo copiando valores. La causa era una macro de serde_json; el arreglo fue mover en vez de copiar."
date: 2026-10-31
series: speed
tags: [rendimiento, profiling, api-server, rust]
docs: [numbers, contributing]
lang: es
translation: the-copy-hidden-in-json
---

La [primera parte](/es/blog/pequeno-no-basta/) de esta serie hizo 13× más
rápidos los list de recursos built-in mandando los bytes que el store ya
guarda. Un list quedó fuera a propósito: la **Table**, lo que pide
`kubectl get`. Sus filas necesitan campos leídos de cada objeto, así que
seguía parseando cada objeto y armando una respuesta nueva. Mejoró 23% y
quedó como el siguiente objetivo.

## Dónde estábamos

`api_load` en 0.2.19, 1.000 ConfigMaps, 32 clientes:

| escenario | ops/s | p50 |
|---|---|---|
| list_all | 5.938 | 1,3 ms |
| list_table | **364** | **20,8 ms** |

Dieciséis veces más lento que el list normal. El parseo explica una parte:
cerca de 1,1 µs por objeto, o sea alrededor de 1 ms para 1.000. Pero a 364
list por segundo en 14 núcleos, cada Table costaba decenas de milisegundos
de CPU. Otra cosa hacía la mayor parte del trabajo.

## Mirar en vez de adivinar

macOS trae `sample`, un profiler por muestreo que no necesita
configuración. El benchmark corre un solo escenario si se le da su nombre,
así que dejamos `list_table` corriendo 15 segundos y muestreamos el
proceso durante 5:

```bash
PERF_SECS=15 PERF_RUNS=1 target/release/deps/api_load-<hash> --bench list_table &
/usr/bin/sample $! 5 -file sample.txt
```

Sumando las muestras bajo cada función del handler de la petición:

| dónde | parte del handler |
|---|---|
| `table::build` | **52%** |
| `Store::list` (parsear los objetos) | 21% |
| serializar la respuesta | 6% |

Arriba de las pilas estaban `malloc`, `free`, `BTreeMap::insert` y
`Value::serialize`. Armar una Table reservaba y liberaba mucha más memoria
de la que debería una tabla de 1.000 filas.

## La copia que no se ve

`table::build` armaba cada fila así:

```rust
let cells: Vec<Value> = extractors.iter().map(|f| f(obj)).collect();
let mut row = json!({"cells": cells});
row["object"] = json!({
    "kind": "PartialObjectMetadata",
    "apiVersion": "meta.k8s.io/v1",
    "metadata": obj.get("metadata").cloned().unwrap_or(json!({})),
});
```

y después la tabla con `json!({..., "rows": rows})`.

Se lee bien. La trampa está en la macro `json!` de serde_json: un literal
entra al valor tal cual, pero **cualquier expresión** pasa por
`serde_json::to_value(&expr)`. Eso recibe una referencia, así que no puede
mover el valor: lo serializa a un árbol `Value` nuevo, entrada por
entrada, y el original se libera justo después. Las celdas y la metadata
de cada fila se copiaban una vez, y luego `"rows": rows` volvía a copiar
todas las filas. Todo para armar algo que un momento después se
serializaba a bytes.

Eso es el `Value::serialize` y el `BTreeMap::insert` de arriba del perfil,
y también la mayor parte del `free`.

## Mover en vez de copiar

El que llama ya era dueño de los objetos, así que `table::build` ahora los
recibe por valor. La metadata de cada fila se saca de su objeto (sin
clonar) y las filas y la tabla se arman como mapas directamente:

```rust
let mut row = Map::with_capacity(2);
row.insert("cells".into(), Value::Array(cells));
// ...la metadata del objeto, movida con take()
table.insert("rows".into(), Value::Array(rows));
```

El mapa de serde_json es un `BTreeMap` (no activamos `preserve_order`), así
que las claves salen ordenadas sin importar cómo se inserten, y la
respuesta es la misma byte por byte. El camino general de list tenía el
mismo patrón, `json!({"items": items})`, para custom resources y
PartialObjectMetadata. Ahora pasa por una pequeña función `list_value` que
mueve los items adentro.

| escenario | antes | después |
|---|---|---|
| list_table | 364 ops/s, p50 20,8 ms | **864 ops/s, p50 7,6 ms** (×2,4) |

Los 700 tests unitarios y del API server pasan sin cambios. Sale en 0.2.20.

## Lo que sigue

- **Una Table sigue siendo unas 8× más lenta que un list normal.** El
  siguiente costo es ese 21% que parsea cada objeto entero, `managedFields`
  incluido, cuando las columnas leen unos pocos campos. Un parseo parcial,
  como ya hacen los label selectors, debería cerrar otra parte de la
  distancia.
- **Controllers bajo carga de escritura**: durante el escenario create
  siguen registrando "fell behind the change feed, resyncing".

La lección es pequeña: `json!` es para literales. Cuando un valor ya está
armado y es tuyo, muévelo tú al mapa.
