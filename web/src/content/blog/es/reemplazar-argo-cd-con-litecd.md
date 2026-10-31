---
title: "Reemplazar el controlador de Argo CD con litecd: de 666 MiB a 100, y lo que costó"
description: "Cómo cambiamos el application controller de Argo CD por litecd en un cluster k3s de producción: el plan, las trampas, los números y lo que cedimos."
date: 2026-10-30
tags: [gitops, argo-cd, litecd, memoria, migración]
docs: [litecd, ui]
lang: es
translation: replacing-argo-cd-with-litecd
---

En un cluster pequeño, lo más caro que corre muchas veces no es tu
aplicación. En el propio bench de litek8s, una máquina de 2 GiB con una app
sincronizada, todo el control plane de Kubernetes ocupaba 90 MiB y los siete
pods de Argo CD 359 MiB. GitOps costaba cuatro veces lo que costaba
Kubernetes.

Ese número es la razón de que exista
[litecd](https://github.com/ideascoldigital/litecd): un binario de Rust que
lee los manifiestos `Application`, `AppProject` y `ApplicationSet` de Argo
CD sin cambios y escribe el mismo `.status`. Esta es la historia de
construirlo y luego ponerlo en lugar del controlador de Argo CD en un
cluster de producción real, incluidas las partes que no salieron según el
plan.

## A dónde va la memoria de Argo CD

El costo de Argo CD no es desperdicio; son decisiones de diseño que le
sirven a clusters grandes:

- el application controller observa **cada tipo de recurso que sirve el
  cluster** (79 watches en litek8s, 103 en k3s) y guarda el cluster en
  memoria, para responder al instante "qué le pertenece a esta app";
- siete procesos (application controller, repo server, API server,
  controlador de ApplicationSets, notificaciones, Dex, Redis), cada uno con
  su propio runtime de Go y sus cachés.

litecd toma las decisiones contrarias. Observa solo los tipos que aparecen
en los manifiestos de alguna app, descarta al llegar los objetos que no
rastrea, corre como un solo proceso, y llama a `git`, `helm` y `kustomize`
exactamente como lo hace el repo server de Argo CD, así que el render se
comporta igual.

## Ser compatible, medido

"Lee los manifiestos de Argo CD" no significa nada sin un árbitro. Antes de
escribir el controlador construimos un corpus de compatibilidad: conjuntos
de manifiestos con sus resultados esperados, donde los resultados esperados
solo salen de correr Argo CD real (v3.5.5). Cada caso tiene que pasar
primero en Argo CD; litecd después tiene que producir los mismos objetos y
el mismo status. El corpus llegó a 38 casos: sync waves, todo tipo de
hooks, varias fuentes con valores `$ref`, `ignoreDifferences` con rutas jq,
la propia biblioteca de salud en Lua de Argo CD (vendorizada), generadores
de ApplicationSet, ventanas de sync, apps fuera del namespace de Argo CD.

En un cluster litek8s con 20 Applications, lado a lado:

| | Argo CD (5 pods) | litecd (1 pod) |
|---|---|---|
| memoria, 20 apps en reposo | 310 MiB | 6.9 MiB |
| instalación → listo | 28.7 s | 0.6 s |
| 19 apps a la vez → Synced + Healthy | 5.7 s | 3.3 s |

Buenos números de laboratorio. Producción es otro animal.

## El cluster

Un cluster k3s de 6 nodos, unas 4 CPUs y 5 GiB asignables cada uno. Argo CD
v3.4 en HA: Redis HA detrás de haproxy, dos repo servers, dos API servers,
login por OIDC. 59 Applications, ~690 recursos gestionados: directorios
simples, Kustomize con una base remota, charts umbrella de Helm locales
desde un repositorio git por SSH, y 13 charts de repositorios de Helm. Sync
automático, prune y self-heal en cada app, sync waves, hooks de Helm,
`ignoreDifferences`, checks de salud en Lua. Y el detalle que más importa:
un app of apps que además instala al propio Argo CD.

El namespace de Argo CD usaba 1,322 MiB en 15 pods; solo su application
controller, 666 MiB.

## Paso 0: preguntarle a los manifiestos

`litecd check` lee un directorio de manifiestos y dice qué funciones usan y
en qué fase las soporta litecd. Con el repositorio GitOps y el `argocd-cm`
del cluster: 54 apps necesitaban funciones de la fase 1, 5 de la fase 2,
nada sin soporte. Eso hizo que valiera la pena el siguiente paso.

## Paso 1: observar sin tocar

La regla para cualquier cosa nueva cerca de producción: primero no escribe
nada. litecd ganó un modo `--observe` que renderiza y compara cada
Application como lo haría, registra dónde su resultado difiere del status
que guardó Argo CD, y nunca escribe un status, un sync, un prune ni un
finalizer.

Corriendo desde una laptop, reportó diferencias que no eran reales: un
repositorio de charts al que se llega por DNS interno del cluster, y una
anotación de checksum que cambiaba porque la laptop tenía Helm 4 y Argo CD
corre Helm 3. Lección: observa donde va a correr el controlador.

Así que entró al cluster como su propio Deployment, con un ServiceAccount
que puede leer todo y hacer `patch` (el diff se confirma con un dry run de
server-side apply). Para que "no escribe nada" fuera una garantía y no una
promesa, una ValidatingAdmissionPolicy rechaza cualquier petición de ese
ServiceAccount que no sea un dry run (`request.dryRun == true`). Así el
código puede estar mal y producción igual no puede cambiar.

Resultado: **58 de 59 apps idénticas** al status que guardó Argo CD. La 59
renderiza un Secret con `randAlphaNum` de Helm: cada render es distinto.
Argo CD lo oculta cacheando los manifiestos renderizados; litecd, con su
caché de render en un `emptyDir`, sincroniza esa app una vez cada vez que
reinicia. No es un bug de ninguno, pero conviene saberlo antes del cambio.

## Paso 2: el cambio, y la trampa que tiene

El plan fueron tres scripts, en dry run por defecto: un precheck (solo
lectura: nada sincronizando, todo Synced y Healthy, el observador limpio,
una foto de cada Application y de las réplicas de Argo CD), el takeover y
un rollback.

La trampa es el app of apps. Si Argo CD gestiona su propia instalación y
escalas su controlador a 0, algo con self-heal va a notar que el
StatefulSet se alejó de git y lo va a escalar de vuelta. Incluido litecd,
una vez que toma el control. Así que el orden es:

1. Apagar el sync automático de la app raíz y de la app que instala Argo
   CD, **mientras Argo CD todavía corre**, y comprobar que quedó así.
2. Escalar `argocd-application-controller` a 0 y esperar a que se vaya su
   pod.
3. Arrancar litecd.

Escribir ese orden produjo una funcionalidad. Dos controladores
reconciliando la misma Application pelean para siempre, cada uno
deshaciendo el status del otro, así que litecd ahora **se hace a un lado
solo** mientras el application controller de Argo CD tenga réplicas: no
escribe nada para ninguna Application y registra por qué, y toma el control
segundos después de que desaparece el último pod del controlador. Los
ApplicationSets esperan igual al controlador de ApplicationSets de Argo CD.
Una app con la anotación `litecd.dev/manage: "true"` es de litecd de todos
modos, para probar una sola app primero.

Se conservó: los CRDs, cada Application, el AppProject, los Secrets de
repositorios y clusters, `argocd-cm`, y el API server, los repo servers y el
Redis de Argo CD, así que el equipo mantuvo su interfaz web y el webhook de
GitHub (que pone la anotación de refresh que litecd respeta). Después,
notificaciones (sin configurar), el controlador de ApplicationSets (sin
ApplicationSets) y Dex (OIDC configurado directo) también fueron a 0.

Lo que pasó: litecd registró *Argo CD's application controller is gone:
taking over Applications*, los conteos coincidieron, el chart aleatorio se
sincronizó una vez y limpió sus hooks, y no se borró nada.

## Los números

Medidos con metrics-server, muestreando cada 5 minutos durante horas:

| namespace de Argo CD | antes | después |
|---|---|---|
| memoria usada | 1,322 MiB, 15 pods | 594 MiB, 11 pods |
| memoria reservada (requests) | 1,216 MiB | 608 MiB |
| CPU | ~293m | ~198m |
| el controlador | 666 MiB | litecd ~100 MiB |

Unos 730 MiB de memoria de vuelta, y 610 MiB de reservas que el scheduler
puede darle a otra cosa, conservando la interfaz de Argo CD. En nodos de
5 GiB eso es espacio de verdad.

La nota honesta: litecd estaba en 7 MiB en el laboratorio y en ~100 MiB
aquí. Crece con lo que renderiza (59 apps, umbrellas de Helm, una caché de
checkouts de git) y luego se mantiene plano; nunca se reinició. Seis veces
menos que el controlador que reemplazó, no cincuenta.

Los despliegues siguieron funcionando a través de él: commits detectados
por polling en 13 s a 2 min (ese repositorio no tiene webhook), syncs de
menos de un segundo a 49 s con hooks, rollouts con todas las réplicas
disponibles todo el tiempo, y sin presión de memoria, evicciones ni OOM en
los nodos.

## Lo que costó el cambio

Cambiar un controlador no es gratis, y la lista es corta pero real:

- **El árbol de recursos de la interfaz quedó vacío.** argocd-server lee el
  árbol de Deployments, ReplicaSets y Pods de una caché en Redis que solo
  llena el controlador de Argo CD. Las listas, el sync y la salud siguieron
  funcionando; el árbol no.
- **La pestaña Diff** lee otra caché de ese controlador: sigue vacía.
- **Sin SSO, notificaciones ni el CLI `argocd` por gRPC** en litecd; aquí o
  no se usaban o quedaron del lado de Argo CD.
- **Un chart aleatorio se sincroniza una vez por reinicio**, como arriba.

Para el árbol, litecd ganó un modo de compatibilidad que lo escribe en el
Redis de Argo CD con el formato de Argo CD. Lo publicamos **deprecado desde
que nació**: un tag de imagen aparte, apagado salvo que se pase un flag, un
aviso en el log al arrancar, y la promesa de borrarlo. Depende de un
formato interno de caché que nadie prometió mantener estable, y lo
demostró rápido: la primera versión escribió nodos hijos sin `kind` ni
`version` (los objetos de un watch no traen su tipo), y la interfaz de Argo
CD se cayó con ellos. Volvimos a la imagen normal y borramos los árboles
que había escrito; la siguiente release lo arregló y ahora se niega a
escribir cualquier árbol con un nodo que no pueda describir por completo.
Existe solo hasta que litecd tenga su propia vista de los recursos de una
app, calculada desde la API de Kubernetes. Mientras tanto, `litek8s ui`
muestra las Applications, sus recursos y su diff con git en cualquier
cluster, solo a través de la API de Kubernetes.

## Pruébalo

Lo que en ese cluster tomó scripts y cuidado ahora es una línea en el tuyo:

```bash
curl -fsSL https://raw.githubusercontent.com/ideascoldigital/litecd/main/install.sh | sh
```

En un cluster sin Argo CD instala todo. Junto a un Argo CD existente
instala solo litecd, no toca el Service, los ConfigMaps ni los CRDs de Argo
CD, y espera a que escales a 0 el controlador de Argo CD. Para ver el ciclo
completo en local, `examples/litecd/up.sh` en el repositorio de litek8s
levanta un cluster de desarrollo con litecd y un app of apps en más o menos
un minuto.

Lo que queda en ese cluster de producción: quitar las cargas restantes de
Argo CD liberaría unos 600 MiB más, y eso espera a la única cosa para la
que el equipo todavía abre Argo CD: una interfaz web.
