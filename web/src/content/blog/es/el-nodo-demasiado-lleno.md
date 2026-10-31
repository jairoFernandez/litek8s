---
title: "El nodo demasiado lleno: por qué Kubernetes desalojó nuestra base de datos"
description: "Un nodo al 97% de memoria, una base de datos desalojada dos veces y un scheduler que veía espacio. Requests contra uso, y cómo movimos pods sin caídas."
date: 2026-10-30
tags: [scheduling, memoria, eviction, k3s, operación]
docs: [ui, litecd]
lang: es
translation: the-node-that-was-too-full
---

El monitoreo decía que todo estaba bien. Todos los nodos Ready, ningún pod
en Pending, la CPU en 12%. Y aun así la base de datos de producción había
sido desalojada la noche anterior, por segunda vez en pocas semanas.

Esto es lo que encontramos en un cluster k3s de producción, seis nodos de
unas 4 CPUs y 5.2 GiB asignables cada uno, que llamaremos A a F. La versión
corta: **Kubernetes ubica los pods según lo que piden y los desaloja según
lo que usan.** Cuando esas dos cosas se separan, un nodo puede estar lleno
y parecer vacío.

## Los síntomas

Por nodo, como porcentaje de la memoria asignable (working set, desde
metrics-server):

| nodo | A | B | C | D | E | F |
|---|---|---|---|---|---|---|
| memoria usada | **93%** | 81% | 45% | 71% | 62% | 52% |

El nodo A llevaba horas entre 92% y 97%. Ahí vivía la base de datos, un
StatefulSet de una sola réplica, y su evento de desalojo era directo:

```
The node was low on resource: memory. Threshold quantity: 100Mi,
available: ~40Mi. Container was using ~2.3Gi
```

Reinició sin problema y el scheduler la puso otra vez en A. Semanas antes la
misma base de datos había sido desalojada cuando un build de CI sin request
de memoria creció a 3 GiB en su nodo, y los builds de imágenes habían
muerto por OOM en otros nodos. Un patrón, no mala suerte.

## Por qué el scheduler veía espacio

El scheduler no mira el uso. Suma **requests**. En A sumaban 4,250 de
5,327 MiB, 80%: para el scheduler, cerca de un GiB libre. Cada carga en A
parecía honesta por sí sola:

| en el nodo A | request | usado | límite |
|---|---|---|---|
| base de datos | 2 GiB | 1.9 GiB | 3 GiB |
| proveedor de identidad (1 réplica, Recreate) | 768 MiB | ~610 MiB | |
| API de backend (1 de 2 réplicas) | 640 MiB | ~620 MiB | |

El problema se esconde en la primera fila. La base de datos pedía 2 GiB pero
podía llegar a 3: un gigabyte de crecimiento que nadie había reservado. Y lo
"usado" en un nodo es más que sus pods: en todo el cluster los working sets
de los pods sumaban 12.5 GiB mientras los nodos reportaban 21.1 GiB usados.
Los otros 8.6 GiB son caché de páginas (buena parte, los archivos de la
propia base de datos), el kernel y k3s.

Y la regla del kubelet cierra la historia: con presión de memoria, desaloja
primero **los pods que usan más de lo que pidieron**. La base de datos, cada
vez que pasaba de 2 GiB.

## El número que asustaba y no era para tanto

El resumen del cluster decía que los límites de memoria estaban en **171%**
de lo asignable. Suena a sobrecompromiso del peor, pero engaña por dos
lados:

- lo inflan unos pocos límites grandes muy por encima del uso real: un
  runner de CI (límite 4 GiB, ~220 MiB en reposo, los jobs necesitan
  2–3 GiB), un servidor de LLM local (límite 3 GiB, ~60 MiB en reposo, carga
  modelos bajo demanda), workers de workflows (3 GiB cada uno, ~265 MiB
  usados). Están dimensionados para picos reales;
- solo cuenta los pods que **tienen** límite. Los 41 pods sin límite de
  memoria, incluidos dos runners de CI que piden 2 GiB, pueden crecer sin
  tope y son el riesgo mayor.

Las vistas que más nos dijeron fueron requests contra uso por nodo, pods
ordenados por uso menos request (los candidatos a desalojo) y pods sin
límites.

## Lo que hicimos

**1. Liberar memoria en otro lado.** Esto llegó primero y fue por otra
razón: esa misma semana [reemplazamos el controlador de Argo CD por
litecd](/es/blog/reemplazar-argo-cd-con-litecd/), lo que liberó unos 730 MiB
usados y 610 MiB reservados. El controlador estaba en el nodo F, que quedó
como el más vacío, con unos 2.4 GiB reservables.

**2. Mover un pod, y que el scheduler nos sorprenda.** La API de backend
tiene dos réplicas y `maxUnavailable: 0`, así que borrar su pod en A no
cuesta nada. El reemplazo fue al nodo B, que pasó de 81% a 86%. El
scheduler no elige el nodo más vacío: les pone puntaje, y los requests más
la distribución apuntaron a B.

**3. Moverlo bien.** Cordon a lo que no quieres, borrar, uncordon:

```bash
kubectl cordon node-a node-b
kubectl -n prod delete pod backend-api-<hash>     # → nodo C
kubectl -n auth delete pod identity-<hash>         # → nodo F, junto a su base de datos
kubectl uncordon node-a node-b
```

El proveedor de identidad tiene una réplica y estrategia Recreate, así que
moverlo costó unos 2 minutos 20 segundos sin poder iniciar sesión. Ese es el
precio de una sola réplica, y valía la pena saberlo de antemano.

Por nodo después, y una hora más tarde:

| nodo | A | B | C | D | E | F |
|---|---|---|---|---|---|---|
| antes | 93% | 81% | 45% | 71% | 62% | 52% |
| después de mover | 70% | 77% | 59% | 71% | 57% | 64% |
| una hora después | 70% | 81% | 57% | 68% | 56% | 69% |

**4. Arreglar la causa, en git.** Preparado pero todavía sin aplicar:

- el **request de memoria de la base de datos igual a su límite** (3 GiB).
  Así nunca puede usar más de lo que pidió, deja de ser la primera opción
  del kubelet para desalojar, y el scheduler mantiene los 3 GiB libres en su
  nodo;
- un límite de 4 GiB en los runners de CI que no tenían.

A propósito no se tocó: el request de 2 GiB del runner de CI y los límites
de los workers de workflows, porque los picos reales los necesitan. Sigue
pendiente: aplicar el cambio de la base de datos (reiniciar una sola réplica
es una caída corta, así que va en una ventana), límites para los pods de
sistema que quedan, y quizá un séptimo nodo.

Un giro honesto: estos arreglos **suben** el 171% que asustaba, porque pods
que no tenían límite ahora lo tienen. El número sube mientras el riesgo
baja.

## Lo que nos enseñó

- **Los requests deciden dónde va un pod; el uso decide a quién se
  desaloja.** Vigila esa brecha. Un pod que corre cerca o por encima de su
  request en un nodo ocupado es el próximo desalojo.
- **Para lo que no puedes perder, pide lo que puedes usar.** Request igual a
  límite hace que un pod nunca esté por encima de lo que pidió.
- **"Límites al 171%" dice poco.** Los pods sin límite dicen más.
- **Mover un pod es cordon, borrar, uncordon**, y antes revisar cuántas
  réplicas tiene y su estrategia.

## Y para litek8s

Lo que hizo visible al nodo A fue la vista Home de `litek8s ui` para
clusters que no son litek8s: la memoria por nodo como porcentaje de lo
asignable, sobre el propio cluster k3s. Este incidente escribió sus
siguientes funciones: requests y uso lado a lado por nodo, una lista de
candidatos a desalojo (uso por encima del request), pods sin límites, una
explicación simple de qué significa un porcentaje de límites, y un "mover
este pod" guiado que hace cordon a los demás nodos y avisa cuando una carga
tiene una sola réplica o estrategia Recreate. Y para el propio scheduler de
litek8s, un chequeo de desvío que marque los pods que viven cerca de su
request, mucho antes de que el kubelet tenga que elegir a quién desalojar.
