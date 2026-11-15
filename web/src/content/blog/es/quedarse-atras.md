---
title: "Quedarse atrás: por qué un Deployment esperaba 4,7 segundos a su controlador"
description: "Con una ráfaga de escrituras, los controladores de litek8s se quedaban atrás y pasaban segundos empezando de cero. Cómo se ponen al día ahora."
date: 2026-11-14
series: speed
tags: [rendimiento, controladores, api-server, rust]
docs: [numbers, contributing]
lang: es
translation: falling-behind
---

La [segunda parte](/es/blog/la-copia-escondida-en-json/) de esta serie
terminó con un cabo suelto: durante el escenario `create` del benchmark, el
log se llenaba de una línea, una y otra vez:

```text
controllers: fell behind the change feed, resyncing
```

Todas las peticiones de ese escenario funcionaban y el throughput se veía
bien. Este artículo trata de lo que costaba esa línea, que el benchmark no
medía.

## Dónde estábamos

`cargo bench --bench api_load` en main, en un portátil: 32 clientes contra
un API server en el mismo proceso, con su WAL en disco y sus controladores
corriendo.

| escenario | ops/s | p99 | máx |
|---|---|---|---|
| get | 104.673 | 0,41 ms | 4,3 ms |
| patch | 26.809 | 2,06 ms | 16,4 ms |
| create | 26.384 | 2,42 ms | 81,5 ms |

Y 326 de esas líneas en una corrida, 304 durante `patch`. El máximo de
`create`, 81,5 ms frente a un p99 de 2,4 ms, era el otro cabo suelto.

## Cómo siguen los controladores al store

Cada escritura en el store de litek8s va a un feed de cambios: un canal
broadcast de tokio que guarda los últimos 256 eventos. Lo leen los watches
y los controladores. Un lector que se queda más de 256 eventos atrás recibe
`Lagged` y pierde su sitio.

El store guarda además un historial, los últimos 4.096 cambios, para que un
watch pueda reanudar desde un resourceVersion. Los watches ya lo usaban
cuando se atrasaban: se ponían al día desde el historial y seguían. Los
controladores no. Su bucle hacía esto:

```rust
Some(Err(RecvError::Lagged(_))) => break, // ...y vuelta a empezar
```

Volver a empezar era una resincronización: encolar cada objeto que maneja
un controlador y después dos pasadas de garbage collection, una para los
objetos cuyos dueños ya no existen y otra para los borrados que quedaron a
medias. Las dos parseaban **cada objeto del store** a un
`serde_json::Value`, solo para leer su uid, sus ownerReferences y sus
finalizers.

## Midiendo la resincronización

Le pusimos un cronómetro y corrimos el escenario `create`, que hace crecer
el store hasta unos 100.000 ConfigMaps:

```text
TMPDBG resync 280.99ms
TMPDBG resync 318.97ms
TMPDBG resync 435.56ms
TMPDBG resync 647.73ms
TMPDBG resync 875.47ms
TMPDBG resync 1.02s
TMPDBG resync 1.33s
TMPDBG resync 1.91s
```

A 26.000 escrituras por segundo, los 4.096 cambios del historial cubren
unos 150 ms. Una resincronización tardaba de 0,3 a 1,9 segundos. Al
terminar, los controladores ya estaban otra vez atrás, sin historial del
que ponerse al día, y empezaban otra. Se pasaban la ráfaga resincronizando.

Lo que eso significa para un usuario: mientras corre una resincronización,
los controladores no reaccionan a nada. Para medirlo añadimos un escenario
al benchmark, `rollout_under_load`: mientras 32 clientes crean ConfigMaps,
crear un Deployment y medir cuánto tarda en existir su ReplicaSet. En main:

```text
rollout_under_load    (ReplicaSet after 4754 ms, median of 3; worst 5579 ms)
```

**4,7 segundos** para un paso que en un cluster sin carga tarda unos pocos
milisegundos. Además, la resincronización corría en un worker de tokio y lo
ocupaba todo ese tiempo, y cada petición que le tocaba a ese worker también
esperaba.

## Ponerse al día en vez de empezar de cero

El primer cambio es el que ya tenían los watches. El bucle recuerda el
último resourceVersion que vio y, ante `Lagged`, le pide al store lo que
vino después:

```rust
if lagged {
    let Ok(caught_up) = manager.api.store().subscribe(Some(last_rv)) else { break 'feed };
    live = caught_up.live;
    for event in &caught_up.backlog {
        manager.observe_at(event, &mut last_rv);
    }
}
```

Solo cuando el historial ya no llega tan atrás (`subscribe` devuelve
`Gone`) hace una resincronización completa. Las resincronizaciones
completas por corrida bajaron de 326 a 25.

## Leer solo lo que necesita el garbage collection

Las que quedaban seguían tardando demasiado. El garbage collection solo
necesita cuatro cosas de cada objeto, así que ahora lee solo esas, con
tipos serde estrechos que se saltan el resto del JSON sin construirlo:

```rust
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Meta {
    uid: Option<String>,
    owner_references: Option<Vec<Reference>>, // solo su uid
    finalizers: Option<Vec<String>>,
    deletion_timestamp: Option<serde::de::IgnoredAny>,
}
```

Las dos pasadas pasaron a ser una, que también usa el garbage collection
que corre después de cada borrado. Ese parseaba un namespace entero, y
recorría el store una vez por namespace cuando desaparecía un dueño de
ámbito cluster. Una resincronización sobre 100.000 objetos bajó de
0,3–1,9 s a 0,03–0,25 s.

Sigue siendo más de lo que cubre el historial a 26.000 escrituras por
segundo. Así que el garbage collection de la resincronización corre ahora
en un hilo bloqueante (`spawn_blocking`), y el bucle sigue leyendo el feed
mientras tanto. Cuando termina, el bucle encola los objetos que encontró
esperando su finalización.

## Los 81 ms que no eran de los controladores

El máximo de `create` no se movió con estos cambios. Un cronómetro en la
compactación del WAL lo encontró:

```text
TMPDBG compaction 101744 objects 83.78ms
```

El WAL del store es un archivo con cada cambio, que se reescribe como un
snapshot cada ~20.000 escrituras. La reescritura escribía y hacía `fsync`
de cada objeto con el lock del store tomado, y toda petición necesita ese
lock. Con 1.000 objetos tardaba 6 ms. Con 100.000 objetos tardaba 84 ms, y
todas las peticiones esperaban.

Ahora, con el lock, la compactación solo toma las entradas: cada clave y un
puntero a los bytes que el store ya guarda (`Arc<[u8]>`, sin copiarlos). Un
hilo escribe y sincroniza el snapshot. Mientras tanto, las escrituras
siguen yendo al WAL viejo, que queda completo si el proceso muere, y se
guardan también en un buffer. La primera escritura después de que el
snapshot está listo le añade ese buffer y lo renombra sobre el WAL viejo.
Con 101.744 objetos, el lock se toma unos 11 ms para copiar las entradas y
2 ms para el cambio. Un test nuevo escribe durante una compactación,
reinicia y comprueba que cada escritura está ahí.

## Los números

Main y este cambio, alternados tres veces en el mismo portátil y con el
mismo benchmark:

| | main | ahora |
|---|---|---|
| `rollout_under_load`, mediana | 4.712–4.889 ms | **75–174 ms** |
| resincronizaciones completas por corrida | 285–328 | 41–46 |
| `create`, máx | 83–139 ms | **26–30 ms** |
| `create`, p99.9 | 14,9–17,4 ms | 14,6–17,1 ms |
| get, patch, create, lists | | mismas ops/s, dentro del ruido |

El throughput no cambió, y nunca fue el problema. Lo que cambió es el
tiempo que los controladores pasan sin reaccionar, entre 27 y 65 veces
menos, y la pausa más larga de una petición. Pasan los 773 tests. Sale en
la próxima versión.

## Lo que sigue

- **Quedan 41–46 resincronizaciones completas** por corrida, en las
  ráfagas más fuertes. Cada una es barata y ya no bloquea el bucle, pero el
  historial podría dimensionarse por ritmo de escritura y no por cantidad.
- **Una ráfaga de 26.000 escrituras por segundo no es lo que ve un home
  lab.** Es lo que parece, durante unos segundos, un `helm install` de un
  chart grande o la resincronización de un operador. Y en una Raspberry Pi
  con tarjeta SD, un `fsync` puede tardar mucho más que en el SSD de un
  portátil. Medir ahí es lo siguiente.

La lección: un benchmark que mide peticiones ve el API server. No ve si
los controladores que hay detrás siguen haciendo su trabajo. Eso necesitó
un escenario propio.
