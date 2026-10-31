---
title: "Métricas, logs y alertas sin kube-prometheus-stack ni Loki: de 1,371 MiB a 1.8 MiB"
description: "En un nodo k3s pequeño el stack de siempre sumó 1,371 MiB. Las métricas, logs y alertas integradas de litek8s sumaron 1.8 MiB. Qué ganas y qué queda afuera."
date: 2026-10-30
tags: [observabilidad, prometheus, loki, alertas, memoria]
docs: [observability, numbers, limitations, ui]
lang: es
translation: observability-without-kube-prometheus-stack-and-loki
---

Para que un equipo pequeño pueda operar un cluster pequeño hacen falta tres
cosas: saber cuánto cuestan sus pods, encontrar el error en los logs de
anoche y recibir un aviso cuando un disco se llena o un pod entra en crash
loop. La respuesta de manual es kube-prometheus-stack más Loki y un shipper
de logs. En un nodo pequeño, esa respuesta es lo más pesado que corre.

Lo medimos. Misma carga, mismo tipo de guest (2 vCPU, 3 GiB, sin swap,
Ubuntu 24.04 sobre KVM): tres pods de nginx en reposo y dos generadores de
logs escribiendo 10 líneas por segundo cada uno.

| | k3s + kube-prometheus-stack + Loki + Alloy | litek8s, integrado |
|---|---|---|
| memoria que suma la observabilidad (PSS) | **1,371 MiB** | **+1.8 MiB** |
| CPU que suma, % de una vCPU | 4.2 % | +0.03 % |
| lo que instalas | 2.1 GiB de imágenes | nada (+2.1 MiB de binario) |
| métricas en disco | 2.0–4.1 GB para 10 días | ≈ 10 MB para 90 días |
| logs en disco además de los archivos de CRI | ≈ 52 MiB/día | 0 |

Eso es con Grafana apagado del lado de k3s, porque litek8s tampoco trae
Grafana. La configuración, los datos crudos y el script para repetirlo están
en
[bench/RESULTS.md](https://github.com/jairoFernandez/litek8s/blob/main/bench/RESULTS.md)
("Observability: built in vs kube-prometheus-stack + Loki"); el resumen, en
[Números](/es/docs/numbers/).

Antes de compartir esa tabla: los dos lados no recolectan lo mismo. Eso
viene más abajo, y pesa.

## Pruébalo en un minuto

La observabilidad viene activada por defecto en `litek8s dev` y en
`litek8s apiserver`. En un cluster de desarrollo:

```bash
litek8s dev up
kubectl create deployment web --image=nginx
```

Y después, tal cual está en la [documentación de observabilidad](/es/docs/observability/):

```bash
litek8s logs -l app=web --since 1h --grep 'ERROR|panic'   # todos los pods de app=web, todos los nodos
litek8s logs -n shop -f                                    # seguir un namespace
litek8s query 'topk(5, sum by (namespace, pod) (rate(container_cpu_usage_seconds_total[5m])))'
litek8s alerts                                             # qué está disparándose
litek8s alerts ack KubePodCrashLooping                     # silenciar hasta que se resuelva
litek8s top                                                # nodos y pods, en vivo
kubectl top pods -A --containers                           # sin necesidad de metrics-server
```

Sin chart de Helm, sin operador, sin CRDs que instalar antes. `kubectl top`
funciona porque el propio API server sirve `metrics.k8s.io/v1beta1` desde el
mismo almacén.

## En qué se van los 1.8 MiB

**Métricas.** Cada agente de nodo muestrea su máquina y cada contenedor cada
10 segundos desde cgroup v2: CPU, memoria separada en anónima y page cache,
I/O, OOM kills, throttling, la red del pod en su veth y el uso de los
volúmenes. El API server agrega el estado de los objetos con los nombres de
kube-state-metrics, y sus propias métricas de peticiones. Las series llevan
los labels `job` e `instance` que pondrían los scrape jobs habituales de
Prometheus, así que los selectores de los dashboards coinciden.

Todo va a un almacén de series temporales embebido: chunks con compresión
Gorilla, un write-ahead log y tres niveles de resolución (10 s durante 6
horas, 1 minuto durante 7 días, 10 minutos durante 90 días por defecto). En
memoria solo viven el índice de labels y un head chunk por serie; los bloques
se leen en cada consulta. Un presupuesto estricto (`--obs-memory`, 24 MiB por
defecto, 8 MiB en un cluster de desarrollo) pone tope a la cantidad de
series: las que lo superan se rechazan, se cuentan y disparan una alerta
integrada, en lugar de hacer crecer la memoria.

Con la carga del bench guardó 1.60 bytes por muestra. En un cluster simulado
de 3 nodos con 240 contenedores (5,560 series, seis horas de muestras cada
10 s), un test del repositorio midió 4 millones de muestras por segundo de
ingesta, un índice de 1 MiB, 1.8 MiB de head chunks y un select de una hora
sobre 240 series en 6 ms.

**Logs.** No hay almacén de logs. Los nodos ya escriben archivos de log de
CRI; una búsqueda va a todos los nodos, cada agente filtra sus propios
archivos (namespace, selector de labels, nombre del pod, contenedor, rango de
tiempo, una regex sobre el texto) y devuelve las coincidencias más recientes,
y el API server las ordena por tiempo. Cuando se elimina un contenedor (se
borró su pod, o se reinició dos veces), sus archivos de log pasan a un
archivo histórico por nodo y siguen buscables durante 7 días o hasta 512 MiB
por nodo, empezando a borrar por los más viejos. Así que "qué imprimió el pod
que borré hace una hora" tiene respuesta:

```bash
litek8s logs api- -n prod -f                      # pods cuyo nombre coincide con api-
litek8s logs --grep '(?i)timeout' --since 2026-10-19T08:00:00Z --until 30m --json
```

(El archivo histórico es cosa del runtime crun: con el runtime Docker, en
macOS sin VM de desarrollo, los logs se leen a través de Docker y no hay
archivo histórico.)

**Alertas.** El API server evalúa las reglas cada 30 segundos. Diez vienen
integradas: crash loops, OOM kills, bucles de reinicio, pods que no quedan
listos, nodos no listos, presión en nodos, discos y volúmenes llenos, poca
memoria, y el almacén rechazando series. Las tuyas son objetos `AlertRule`
cuyo spec es el de un `PrometheusRule`; si el CRD
`monitoring.coreos.com/v1` está instalado, también se leen los
PrometheusRule existentes:

```yaml
apiVersion: observability.litek8s.dev/v1
kind: AlertRule
metadata: {name: web, namespace: shop}
spec:
  groups:
    - name: web
      rules:
        - alert: WebErrorsHigh
          expr: sum(rate(http_errors_total{app="web"}[5m])) > 1
          for: 5m
          labels: {severity: critical, team: web}
          annotations: {summary: "web serves {{ $value | humanize }} errors/s"}
```

Las notificaciones van a objetos `AlertReceiver`: un webhook con el payload
de Alertmanager, ntfy o Telegram. Los silences y los acks (un silence que se
borra solo cuando la alerta se resuelve) se manejan desde la CLI. El estado
de las alertas se guarda en disco, así que un reinicio ni olvida una alerta
ni la manda dos veces.

## Grafana sigue funcionando

El almacén responde la API HTTP de Prometheus bajo `/prometheus` en el puerto
del API: `query`, `query_range`, `series`, `labels`, `rules`, `alerts`, y
`/federate` para exportar todo a un Prometheus real. Agrega un data source
Prometheus con la URL `http://<api server>:6443/prometheus` (o arranca el
API server con `--prometheus-listen 0.0.0.0:9090`). Lo probamos con Grafana
12.2: health check, consultas de rango, explorador de métricas y valores de
labels. Los dashboards hechos sobre las series crudas de cAdvisor,
kube-state-metrics y node-exporter funcionan. Los de kubernetes-mixin que
trae kube-prometheus-stack no, porque leen recording rules que litek8s no
tiene.

Si prefieres no correr Grafana, `litek8s ui` tiene una vista Observe:
alertas activas, pods por CPU o memoria contra sus límites con sparklines de
30 minutos, y búsqueda de logs entre pods. En reposo ocupa 4.4 MiB de RSS.

## Lo que no es igual

Esta es la parte que hay que leer antes de citar la primera tabla.

- **Series: 342 contra 69,672.** kube-prometheus-stack guarda cada métrica
  de cAdvisor y del kubelet, los histogramas por recurso de kube-apiserver,
  métricas de nodo por CPU y las de sus propios componentes. litek8s guarda
  un conjunto seleccionado que cubre los paneles habituales de un dashboard
  de Kubernetes y sus alertas integradas, no todo lo que Prometheus puede
  responder. Eso sí, muestrea tres veces más seguido (10 s contra 30 s).
- **Todavía no hace scraping de tus aplicaciones.** Prometheus lee el
  `/metrics` de cualquier pod; litek8s no, ni por anotaciones ni por
  ServiceMonitor. La `AlertRule` de arriba necesita que `http_errors_total`
  salga de algún lado: hoy, de un Prometheus que apuntes a tu app.
- **El PromQL es un subconjunto.** Están los selectores,
  `rate`/`irate`/`increase`, la familia `*_over_time`, agregaciones con
  `by`/`without`, operadores binarios con vector matching,
  `histogram_quantile`, `label_replace` y la mayoría de las funciones. No
  están las subconsultas (`x[1h:5m]`), el modificador `@`, los histogramas
  nativos, las recording rules ni los exemplars.
- **Los datos viejos son más gruesos.** Después de 6 horas hay un punto por
  minuto, después de 7 días uno cada 10 minutos: los contadores guardan su
  último valor por bucket, los gauges el promedio. Así que `max_over_time`
  sobre el mes pasado ve promedios, no picos.
- **Los logs se recorren, no se indexan.** Cada archivo se biseca hasta el
  inicio del rango de tiempo, pero días de logs de un cluster verboso se leen
  completos en cada nodo (en paralelo). Loki indexa labels y guarda chunks
  comprimidos durante toda su retención, más allá de pods borrados y de la
  rotación en los nodos.
- **Consultas.** Las instantáneas fueron tan rápidas como en Prometheus
  (0.2–0.4 ms contra 0.3–0.5 ms); las de rango de una hora, más lentas
  (0.8–4.3 ms contra 0.4–1.5 ms), por debajo de lo que nota un dashboard.
  Las de logs tardaron 11–27 ms contra 15–49 ms de Loki.
- **Un solo servidor.** Hoy un solo API server evalúa alertas y guarda
  métricas.

Dos notas sobre la medición. La cifra de litek8s es un A/B: dos guests
idénticos de 2 vCPU / 2 GiB con la observabilidad prendida y apagada (el API
server en 32.0 contra 30.2 MiB). Y los 1,371 MiB de k3s son los pods del
stack (1,057 MiB) más lo que crecieron k3s-server y containerd al servirlo
(+314 MiB).

## Cuándo sigues queriendo el stack completo

Corre kube-prometheus-stack (o un Prometheus al lado de litek8s) cuando:

- tus aplicaciones exponen `/metrics` y las quieres en el mismo lugar;
- dependes de recording rules, subconsultas o los dashboards de
  kubernetes-mixin tal como vienen;
- necesitas historia en alta resolución más allá de 6 horas, o búsqueda de
  logs indexada sobre semanas;
- corres un control plane replicado y necesitas que las alertas sobrevivan a
  perder el servidor que las evalúa.

No se excluyen: `/metrics` sigue igual para un Prometheus real,
`/prometheus/federate` exporta el almacén, e instalar metrics-server toma
`metrics.k8s.io` (desinstalarlo devuelve el grupo). Y si no quieres nada de
esto, `--no-observability` lo apaga todo.

Para un home lab, una caja en el edge, un cluster de desarrollo o un único
servidor pequeño, lo que queríamos eran las métricas, logs y alertas de la
propia plataforma, sin un segundo sistema que operar al lado. Eso es esto.

## Pruébalo

```bash
brew install jairoFernandez/tap/litek8s
litek8s dev up
litek8s alerts
kubectl top pods -A --containers
```

Todo lo anterior, con cada serie y cada flag, está en
[Métricas, logs y alertas](/es/docs/observability/). Lo que todavía falta,
en [Limitaciones](/es/docs/limitations/).
