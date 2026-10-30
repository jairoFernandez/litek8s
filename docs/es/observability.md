# Observabilidad: métricas, logs y alertas, integradas

Un cluster doméstico o de edge quiere saber cuánto cuestan sus pods, encontrar
el error en los logs de anoche y recibir un mensaje cuando un disco se llena o
un pod entra en crash loop. La respuesta habitual, kube-prometheus-stack más
Loki, cuesta más que el propio litek8s (ver [las mediciones](#mediciones)).
litek8s lo trae integrado:

- **Métricas**: cada agente de nodo muestrea su máquina y cada contenedor
  (cgroup v2: CPU, memoria dividida en anónima y page cache, I/O, OOM kills,
  throttling; la red del pod en su veth; el uso de volúmenes persistentes)
  cada 10 s; el API server agrega el estado de los objetos (con los nombres de
  kube-state-metrics) y sus propias métricas de peticiones. Todo va a un
  almacén de series temporales embebido: chunks con compresión Gorilla, un
  write-ahead log, tres niveles de resolución (10 s durante 6 h, 1 min durante
  7 d, 10 min durante 90 d por defecto) y un presupuesto de memoria estricto.
- **Una API compatible con Prometheus** con un subconjunto práctico de PromQL,
  así que Grafana usa litek8s directamente como data source de Prometheus, y
  los dashboards escritos para cAdvisor, node-exporter y kube-state-metrics
  funcionan en parte sin cambios. `/metrics` sigue como estaba para un
  Prometheus real, y `/prometheus/federate` exporta todo.
- **Logs**: búsqueda y seguimiento en todos los pods y nodos (también de
  contenedores que ya no existen) a partir de los archivos de log de CRI que
  los nodos ya escriben; sin almacén de logs, sin shipper.
- **Alertas**: reglas integradas (crash loops, OOM kills, bucles de reinicio,
  nodos no listos, presión en nodos, discos y volúmenes llenos, poca memoria),
  las tuyas como objetos `AlertRule` en sintaxis de reglas de Prometheus,
  notificaciones a un webhook (el payload de Alertmanager), ntfy o Telegram,
  silences y acks.
- **`kubectl top` y `litek8s top`** leen la API de métricas de recursos
  (`metrics.k8s.io/v1beta1`) que el API server sirve desde el mismo almacén:
  sin metrics-server ([más abajo](#api-de-métricas-de-recursos-kubectl-top)).
- **`litek8s ui`** lo muestra todo en su vista Observe (9).

Todo está activado por defecto en `litek8s apiserver` y `litek8s dev`;
`--no-observability` lo desactiva.

## Inicio rápido

```bash
litek8s logs -l app=web --since 1h --grep 'ERROR|panic'   # todos los pods de app=web, todos los nodos
litek8s logs -n shop -f                                    # seguir un namespace
litek8s query 'topk(5, sum by (namespace, pod) (rate(container_cpu_usage_seconds_total[5m])))'
litek8s alerts                                             # qué está disparándose
litek8s alerts ack KubePodCrashLooping                     # silenciar hasta que se resuelva
litek8s top                                                # nodos y pods, en vivo
kubectl top pods -A --containers                           # sin necesidad de metrics-server
```

Grafana: agrega un data source **Prometheus** con la URL
`http://<api server>:6443/prometheus` (o arranca el API server con
`--prometheus-listen 0.0.0.0:9090` y usa `http://<server>:9090`).
Comprobado con Grafana 12.2: el health check del data source ("Successfully
queried the Prometheus API"), las consultas de rango (GET o POST), el
explorador de métricas y los valores de labels. Los dashboards construidos
sobre las series crudas de cAdvisor, kube-state-metrics y node-exporter
funcionan (los selectores `job`, `image!=""`, `container!=""` que usan
coinciden); los dashboards de kubernetes-mixin que trae kube-prometheus-stack
leen sus recording rules (`node_namespace_pod_container:…`), que litek8s no
tiene.

La CLI encuentra el API server como `litek8s ui`: `--context`, si no el
contexto actual de `$KUBECONFIG`, si no el cluster de desarrollo local.

## Métricas

### Qué se recolecta

Cada serie lleva las labels `job` e `instance` como lo harían los scrape jobs
habituales de Prometheus, así que los selectores de los dashboards coinciden.

| job | dónde | series (labels) |
|---|---|---|
| `kubelet`, `metrics_path="/metrics/cadvisor"` | cada agente de nodo, cada contenedor (crun: su cgroup; runtime de Docker: solo CPU y working set) | `container_cpu_usage_seconds_total`, `container_cpu_user_seconds_total`, `container_cpu_system_seconds_total`, `container_cpu_cfs_periods_total`, `container_cpu_cfs_throttled_periods_total`, `container_cpu_cfs_throttled_seconds_total`, `container_memory_working_set_bytes`, `container_memory_usage_bytes`, `container_memory_rss` (anónima), `container_memory_cache` (page cache), `container_spec_memory_limit_bytes`, `container_oom_events_total`, `container_fs_reads_bytes_total`, `container_fs_writes_bytes_total` (`namespace`, `pod`, `container`, `image`, `node`); por pod: `container_network_{receive,transmit}_{bytes,packets}_total`, `..._packets_dropped_total` (`interface="eth0"`) |
| `kubelet`, `metrics_path="/metrics"` | cada agente de nodo, cada persistent volume claim montado | `kubelet_volume_stats_used_bytes`, `_capacity_bytes`, `_available_bytes` (`namespace`, `persistentvolumeclaim`) |
| `node-exporter` | cada agente de nodo | `node_cpu_seconds_total{mode}`, `node_memory_{MemTotal,MemAvailable,MemFree,Cached,Buffers,SwapTotal,SwapFree}_bytes`, `node_load{1,5,15}`, `node_filesystem_{size,free,avail}_bytes` (el directorio de datos del nodo), `node_network_{receive,transmit}_{bytes,packets,errs}_total{device}`, `node_disk_{read,written}_bytes_total`, `node_disk_io_time_seconds_total`, `machine_cpu_cores`, `up` |
| `litek8s` | cada agente de nodo | a dónde van los recursos del nodo, tal como los explica Home en `litek8s ui`: `litek8s_node_memory_bytes{owner="litek8s|pods|cache"}`, `litek8s_node_cpu_seconds_total{owner="litek8s|pods"}`, `litek8s_node_disk_bytes{category}` (cada 15 min) |
| `kube-state-metrics` | el API server, desde su almacén de objetos | `kube_pod_info`, `kube_pod_status_phase`, `kube_pod_status_ready`, `kube_pod_status_scheduled`, `kube_pod_container_info`, `kube_pod_container_status_{restarts_total,ready,running,waiting,terminated}`, `..._waiting_reason`, `..._terminated_reason`, `..._last_terminated_reason`, `..._last_terminated_exitcode`, `kube_pod_container_resource_{requests,limits}`, `kube_node_info`, `kube_node_status_condition`, `kube_node_status_{capacity,allocatable}`, `kube_node_spec_unschedulable`, `kube_deployment_*`, `kube_statefulset_*`, `kube_daemonset_*`, `kube_job_status_*`, `kube_job_failed`, `kube_cronjob_*`, `kube_persistentvolumeclaim_*`, `kube_persistentvolume_*`, `kube_namespace_status_phase` |
| `apiserver` | el API server | su `/metrics`: `apiserver_request_total`, `apiserver_longrunning_requests` (los nombres y labels de kube-apiserver), `litek8s_apiserver_request_duration_seconds` (histograma), `litek8s_store_*`, …; `process_resident_memory_bytes`, `process_cpu_seconds_total`, `up` |
| `litek8s` | el API server | la salud del propio almacén: `litek8s_tsdb_{series,head_bytes,index_bytes,disk_bytes,wal_bytes,blocks}`, `..._series_{created,refused,removed}_total`, `..._samples_{appended,out_of_order}_total` |
| – | el evaluador de alertas | `ALERTS{alertname, alertstate="pending|firing", …}` tal como lo escribe Prometheus |

El muestreo está alineado al intervalo (10 s por defecto: la resolución del
primer nivel), así que las marcas de tiempo son regulares. Una serie que se
detiene (la de un pod borrado, la de un nodo que se desconecta) recibe un
staleness marker, como en Prometheus: sale de las consultas instantáneas de
inmediato en lugar de quedarse 5 minutos.

### El almacén

`src/obs/tsdb/`, datos en `<data dir>/obs/`:

- **Las series** son conjuntos de labels internados con un índice invertido
  (par de labels → series) en memoria; sus definiciones se agregan al final de
  `series.log`.
- **Las muestras** van primero a `wal.log` (con fsync en cada intervalo) y
  luego al **head chunk** en memoria de la serie: compresión Gorilla, marcas de
  tiempo delta-of-delta (una muestra regular cuesta un bit) y valores con XOR
  (un valor sin cambios, un bit). Cada 20 minutos (o a las 120 muestras) los
  heads se cortan al archivo de **bloque** de la ventana actual y el WAL se
  trunca.
- **Los bloques** son un archivo por nivel y ventana de tiempo (2 h para el
  nivel crudo). Un bloque abierto es de solo agregar, con un CRC por chunk; una
  escritura incompleta al final (un crash a mitad de una escritura) se recorta
  cuando se vuelve a abrir. Cuando la ventana termina, se **sella**: se
  reescribe ordenado por serie con un índice al final y luego se renombra a su
  lugar (de forma atómica).
- **Niveles y downsampling.** Sellar una ventana también la reduce
  (downsampling) al siguiente nivel: los contadores (`*_total`, `*_count`,
  `*_sum`, `*_bucket`, o con tipo counter) conservan el último valor de cada
  bucket, los gauges el promedio; los buckets se marcan con su inicio. Cada
  nivel borra los bloques más antiguos que su retención una vez que se
  reducieron. Una consulta lee el nivel más grueso cuya resolución sea como
  mucho su step (y como mucho un tercio de su ventana de `rate` más corta) entre
  aquellos cuya retención cubre su inicio, y completa la parte que el nivel más
  grueso todavía no ha recibido con la del más fino, reducida al vuelo. Las
  consultas instantáneas leen el nivel más fino que las cubre.
- **Seguridad ante crashes.** Todo lo confirmado está en el WAL; el replay
  omite lo que ya llegó a un bloque; un sellado o un downsampling interrumpido
  por un crash deja el archivo viejo o el nuevo, y las muestras duplicadas se
  descartan al leer. Tests: `survives_restart_through_wal_and_blocks`,
  `torn_wal_tail_is_ignored`, `open_append_reopen_seal_query`.
- **Presupuesto de memoria.** Solo el índice de labels y un head chunk por
  serie están en memoria; los bloques se leen por consulta. `--obs-memory`
  (24 MiB; 8 MiB en un cluster de `litek8s dev`) fija el límite de series (dos
  tercios del presupuesto a ~900 bytes por serie: 18 000 series por defecto);
  las series nuevas por encima de él se rechazan y se cuentan
  (`litek8s_tsdb_series_refused_total`, y la alerta integrada
  `Litek8sMetricsSeriesRefused`). Una consulta carga como mucho 1 millón de
  muestras (16 MB mientras corre). Las series cuyos datos ya salieron de todos
  los niveles se quitan del índice (y `series.log` se compacta).

Flags de `litek8s apiserver`:

| flag | por defecto | |
|---|---|---|
| `--no-observability` | | sin métricas, búsqueda de logs ni alertas |
| `--obs-retention` | `10s:6h,1m:7d,10m:90d` | niveles, `<resolution>:<retention>`; la primera resolución es el intervalo de muestreo; cada resolución es múltiplo de la anterior |
| `--obs-memory` | `24Mi` | memoria para series y heads |
| `--obs-log-retention` | `7d` | cuánto tiempo guarda cada nodo los logs de contenedores eliminados |
| `--obs-log-max-size` | `512Mi` | lo máximo que guarda cada nodo de ellos |
| `--no-builtin-alerts` | | solo tus propias reglas |
| `--prometheus-listen` | | sirve también la API de Prometheus en la raíz de esta dirección |

### Consultas: el subconjunto de PromQL

Soportado:

- selectores con `=`, `!=`, `=~`, `!~` (regexes ancladas, como en
  Prometheus), `{__name__=~"..."}`, selectores de rango `[5m]`, `offset`;
- `rate`, `irate`, `increase` (extrapolados como en Prometheus, con manejo de
  reinicios de contadores), `delta`, `idelta`, `changes`, `resets`, `deriv`,
  `predict_linear`, `{avg,sum,min,max,count,last,stddev,stdvar,quantile,present,absent}_over_time`;
- `sum`, `avg`, `min`, `max`, `count`, `group`, `stddev`, `stdvar`, `topk`,
  `bottomk`, `quantile`, `count_values`, con `by (…)` / `without (…)`
  antes o después;
- `+ - * / % ^ atan2`, comparaciones con y sin `bool`, `and`, `or`,
  `unless`, vector matching con `on`/`ignoring` y
  `group_left`/`group_right` (con labels adicionales), menos unario;
- `histogram_quantile`, `label_replace`, `label_join`, `abs`, `ceil`,
  `floor`, `round`, `exp`, `ln`, `log2`, `log10`, `sqrt`, `sgn`, `clamp`,
  `clamp_min`, `clamp_max`, `time`, `timestamp` (el momento de evaluación),
  `vector`, `scalar`, `absent`, `sort`, `sort_desc`, `day_of_week`,
  `day_of_month`, `days_in_month`, `hour`, `minute`, `month`, `year`.

No soportado: subconsultas (`x[1h:5m]`), el modificador `@`, histogramas
nativos, recording rules, exemplars (siempre vacíos).

### API HTTP

Bajo `/prometheus` en el puerto de la API (y en la raíz de
`--prometheus-listen`), GET o POST (form-encoded), con el formato de respuesta
de Prometheus:

| ruta | |
|---|---|
| `/api/v1/query` | `query`, `time` |
| `/api/v1/query_range` | `query`, `start`, `end`, `step` (11 000 puntos como máximo) |
| `/api/v1/series` | `match[]`, `start`, `end` |
| `/api/v1/labels`, `/api/v1/label/<name>/values` | `match[]`, `start`, `end` |
| `/api/v1/metadata` | tipos y ayuda de las métricas propias del API server |
| `/api/v1/rules`, `/api/v1/alerts` | las reglas de alerta y las alertas activas |
| `/api/v1/status/buildinfo` | informa Prometheus 2.53 (Grafana elige sus funciones según eso) |
| `/federate` | `match[]`: la última muestra de cada serie que coincide, en formato de texto |

### API de métricas de recursos (`kubectl top`)

El API server sirve `metrics.k8s.io/v1beta1`, lo que sirve metrics-server,
desde el almacén: `NodeMetrics` y `PodMetrics`, get y list, selectores de
labels y de campos, por namespace o en todos, y una tabla para
`kubectl get pods.metrics.k8s.io`. Así que `kubectl top nodes`, `kubectl top
pods -A [--containers]` y otros clientes de metrics-server funcionan sin nada
instalado (litek8s todavía no tiene controlador de HorizontalPodAutoscaler).
Las cifras son las de metrics-server:

| | CPU | memoria |
|---|---|---|
| contenedor | tasa de `container_cpu_usage_seconds_total` entre sus dos últimas muestras (la `window`, 10 s por defecto) | último `container_memory_working_set_bytes` |
| nodo | tasa de `node_cpu_usage_seconds_total` | último `node_memory_working_set_bytes` |

Las dos series de nodo llevan los nombres de `/metrics/resource` del kubelet
(`job="kubelet"`, `metrics_path="/metrics/resource"`). Un agente las muestrea
del propio cgroup del nodo cuando tiene uno (cada nodo de un cluster de
`litek8s dev` es una celda con su propio cgroup; un agente en un contenedor),
si no, de la máquina: los modos ocupados de `/proc/stat` y
`MemTotal - MemAvailable`. Leer `/proc` en la VM de dev de macOS contaría el
balloon de memoria como usado y le daría a cada nodo la VM entera. Los nodos
con agentes anteriores a estas series recurren a las de node-exporter
(`node_cpu_seconds_total` sin `idle`, `iowait`, `steal`;
`MemTotal - MemAvailable`), y un nodo sin ninguna serie de nodo (el runtime de
Docker en macOS no tiene `/proc`) informa la suma de sus contenedores.

Un pod aparece un intervalo de muestreo después de arrancar (se necesitan dos
muestras de CPU), y una serie con más de tres intervalos de antigüedad no está
vigente (un nodo que dejó de informar desaparece, `kubectl top nodes` muestra
`<unknown>`).

El grupo está integrado (litek8s no registra objetos APIService para sus
grupos integrados), y aparece en el descubrimiento (también el agregado v2)
mientras la observabilidad está activa. Para usar metrics-server en su lugar,
instálalo como siempre (su chart de Helm o `components.yaml`): su APIService
`v1beta1.metrics.k8s.io` apunta a su Service y toma el grupo, a partir de
entonces toda petición bajo `/apis/metrics.k8s.io/v1beta1` va a
metrics-server, y desinstalarlo le devuelve el grupo al almacén. (Un objeto
APIService Local, como los que kube-apiserver mantiene para sus propios
grupos, haría fallar `helm install metrics-server`: Helm no adopta objetos que
no creó.)

`litek8s top` muestra los nodos (uso frente a allocatable) y los pods del
contexto de kube actual (o de `--context`), los más ocupados primero,
actualizados cada `--interval-secs` en una terminal e impresos una sola vez
cuando se redirige la salida o con `--once`; `-n`, `-l`, `--containers` y
`--sort-by cpu|memory|name` lo acotan y ordenan. Solo lee la API de métricas,
así que también funciona contra cualquier cluster con metrics-server.

## Logs

No hay almacén de logs: una búsqueda va a cada nodo, y cada agente de nodo
lee los archivos de log de CRI que ya escribe (`<time> <stream> <F|P> <text>`,
rotados a los 10 MiB). El agente filtra (namespace, selector de labels de
pods, regex del nombre del pod, contenedor, rango de tiempo, una regex sobre
el texto), se queda con las `limit` coincidencias más recientes y las envía
en streaming; el API server las combina por tiempo. `--follow` sigue leyendo
el final (y toma los contenedores que arrancan después).

Los logs sobreviven a su contenedor: cuando el runtime crun elimina uno (su
pod se borró, o se reinició dos veces), sus archivos se mueven al archivo
histórico del nodo,
`<node dir>/logs/<namespace>/<pod>/<container>.<attempt>.<uid>.log` con las
labels del pod junto a ellos, y también se buscan, hasta que
`--obs-log-retention` (7 días) o `--obs-log-max-size` (512 MiB por nodo,
los más antiguos primero) los eliminan. `--live-only` los omite.

```bash
litek8s logs -l app=web --since 1h --grep 'ERROR|panic'
litek8s logs api- -n prod -f                      # pods cuyo nombre coincide con api-
litek8s logs --grep '(?i)timeout' --since 2026-10-19T08:00:00Z --until 30m --json
```

API: `GET /litek8s/v1/logs?namespace=&selector=&pod=&container=&since=&until=&grep=&limit=&follow=&node=&liveOnly=`
responde con líneas JSON (`ts`, `namespace`, `pod`, `container`, `node`,
`stream`, `line`); un nodo que no puede responder agrega
`{"node": …, "error": …}`. `since`/`until` son una duración hacia atrás desde
ahora (`1h`) o RFC 3339.

En el runtime de Docker (macOS sin VM de dev) el agente lee los logs a través
de Docker en lugar de archivos, y no hay archivo histórico.

## Alertas

El API server evalúa las reglas cada 30 s. Una alerta está *pending* hasta
que pasa su `for`, luego *firing*; se *resuelve* cuando la expresión deja de
devolverla (las alertas resueltas siguen en la lista 15 minutos). El estado se
guarda en `<data dir>/obs/alerts.json`: un reinicio ni olvida una alerta ni la
notifica dos veces.

### Reglas integradas

| alerta | expresión | for | severidad |
|---|---|---|---|
| `KubePodCrashLooping` | `max_over_time(kube_pod_container_status_waiting_reason{reason="CrashLoopBackOff"}[5m]) >= 1` | 1m | warning |
| `KubeContainerOOMKilled` | última terminación `OOMKilled` y un reinicio en los últimos 10 m | – | warning |
| `KubePodRestartLoop` | `increase(kube_pod_container_status_restarts_total[15m]) > 3` | – | warning |
| `KubePodNotReady` | Pending o Unknown | 15m | warning |
| `KubeNodeNotReady` | `kube_node_status_condition{condition="Ready",status="true"} == 0` | 2m | critical |
| `KubeNodePressure` | condición de presión de Disk/Memory/PID | 2m | warning |
| `NodeFilesystemAlmostFull` | < 10 % libre en el sistema de archivos del directorio de datos | 5m | warning |
| `NodeMemoryLow` | < 10 % de memoria disponible | 5m | warning |
| `KubePersistentVolumeFillingUp` | < 15 % libre en un volumen | 2m | warning |
| `Litek8sMetricsSeriesRefused` | el almacén rechazó series (presupuesto) | – | warning |

`--no-builtin-alerts` las desactiva; mejor silencia las que no quieras una por
una.

### Tus reglas: `AlertRule`

El spec es el de un `PrometheusRule`: grupos de reglas con `alert`, `expr`,
`for`, `labels`, `annotations` (`{{ $labels.x }}`, `{{ $value }}`,
`| humanize`, `| humanizePercentage`, `| humanize1024`). Los objetos
`PrometheusRule` de `monitoring.coreos.com/v1` existentes también se leen
cuando ese CRD está instalado. Los errores aparecen en `litek8s alerts` y en
`/api/v1/rules`.

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

(Las métricas de tus propias aplicaciones necesitan una fuente; litek8s
todavía no hace scraping de pods, ver Límites.)

### A dónde van las notificaciones: `AlertReceiver`

Objetos a nivel de cluster; cada receiver recibe todas las alertas con las
que coinciden sus `matchers` (todos ellos; sin matchers: todo), agrupadas por
evaluación, de nuevo cada `repeatInterval` (4h) mientras siguen disparándose,
y una vez al resolverse (`sendResolved`, por defecto true). Un receiver que
falla se reintenta con backoff (de 30 s a 10 min).

```yaml
apiVersion: observability.litek8s.dev/v1
kind: AlertReceiver
metadata: {name: phone}
spec:
  ntfy: {topic: my-cluster-alerts}            # url por defecto es https://ntfy.sh; tokenSecretRef para autenticación
  matchers: [{name: severity, value: critical}]
---
apiVersion: observability.litek8s.dev/v1
kind: AlertReceiver
metadata: {name: telegram}
spec:
  telegram:
    chatId: "123456789"
    botTokenSecretRef: {namespace: kube-system, name: telegram-bot, key: token}
---
apiVersion: observability.litek8s.dev/v1
kind: AlertReceiver
metadata: {name: hook}
spec:
  webhook: {url: "http://alert-handler.ops.svc:8080/alerts"}   # el JSON de webhook de Alertmanager (versión 4)
  repeatInterval: 1h
```

### Silences y acks

Un `Silence` silencia las alertas con las que coinciden sus matchers (`name`,
`value`, `isRegex`, `isEqual`) entre `startsAt` y `endsAt`. Un *ack* es un
silence sobre todas las labels de una alerta, marcado con
`observability.litek8s.dev/until-resolved`: el servidor lo borra cuando la
alerta se resuelve.

```bash
litek8s alerts silence alertname=KubePodCrashLooping namespace=dev --for 4h --comment deploying
litek8s alerts ack 3f2a9c1d04b7e6a1          # un fingerprint, o un nombre de alerta (todas sus alertas)
litek8s alerts silences
litek8s alerts unsilence silence-x7k2p
kubectl get alertrules -A; kubectl get alertreceivers,silences
```

## En `litek8s ui`

La vista Observe (9) muestra las alertas activas y los pods por CPU o memoria
(`s`), cada uno frente al límite del pod (o su request), con sparklines de los
últimos 30 minutos; `l` abre todos los logs del pod seleccionado (incluidos
los de contenedores que ya no existen), `L` busca por regex en los logs de
todos los pods de la última hora, `a` reconoce (ack) las alertas del pod
seleccionado, `A` todas ellas. Muestrea cada 10 s mientras está en pantalla y
nada en absoluto en caso contrario (ver [ui.md](../ui.md#ligera-para-la-máquina)).

## Con varios API servers

El almacén, su directorio (`<data dir>/obs`) y los recolectores son un módulo
propio (`src/obs`), separado del almacén de objetos, para que puedan seguir al
líder del control plane una vez que el API server esté replicado: el
recolector de estado de objetos, la evaluación de alertas y las notificaciones
deben correr en un solo servidor (leen el almacén y escriben borrados de
`Silence`), y las muestras de los agentes de nodo deben llegar a ese servidor
(un follower reenviaría los mensajes `Metrics` de los agentes conectados a
él). Hoy cada API server los ejecuta para los agentes conectados a él.

## Límites

- Todavía no se hace scraping de los endpoints `/metrics` de las aplicaciones
  (anotaciones o ServiceMonitors): las series son las de la propia plataforma.
- Sin recording rules, subconsultas ni `@`; los niveles reducidos conservan un
  valor por bucket (el último en los contadores, la media en los gauges), así
  que `max_over_time` sobre datos antiguos ve promedios.
- La búsqueda de logs lee archivos: cada archivo se biseca hasta el inicio del
  rango de tiempo, pero un rango largo sobre un cluster muy verboso se lee
  completo (todos los nodos en paralelo); no hay índice.
- Un solo API server evalúa alertas y almacena métricas (ver arriba).

## Mediciones

En un guest de 2 vCPU / 3 GiB con la misma carga pequeña, frente a k3s con
kube-prometheus-stack (Grafana desactivado), Loki y Alloy
([bench/RESULTS.md](../../bench/RESULTS.md), "Observability: built in vs
kube-prometheus-stack + Loki"):

| | k3s + kube-prometheus-stack + Loki | litek8s |
|---|---|---|
| memoria | 1.4 GiB | +1.8 MiB |
| CPU, % de una vCPU | 4.2 % | +0.03 % |
| imágenes a instalar | 2.1 GiB | – (+2.1 MiB de binario) |
| series guardadas | 69.7 k | 342 (un conjunto seleccionado, ver arriba) |
| métricas en disco | 2.0–4.1 GB para 10 días | ≈ 10 MB para 90 días (1.6 bytes/muestra) |
| logs en disco además de los archivos de CRI | ≈ 52 MiB/día | 0 |
| consultas | < 2 ms métricas, 15–50 ms logs | < 5 ms métricas, 11–27 ms logs |

El almacén por sí solo, en un cluster simulado de 3 nodos con 240 contenedores
(5,560 series, 6 horas de muestras cada 10 s; `cargo test --release --lib
tsdb_memory_for_a_home_cluster -- --ignored --nocapture`): ingiere 4 millones
de muestras/s, mantiene un índice de 1 MiB y 1.8 MiB de head chunks (RSS +6.4
MiB), y responde un select de una hora de 240 series en 6 ms. Después de todo
el corpus de compatibilidad (119 casos, 47 charts de Helm instalados y
eliminados), el almacén tenía 4,839 series en 1.4 MB de memoria y 2.5 MB en
disco.

La vista Observe de `litek8s ui` en reposo usa 0.00 % de CPU y 0.4
despertares/s mientras muestrea (cada 10 s), 4.4 MiB de RSS.
