# Un LLM repartido entre varias máquinas

Un modelo necesita sus pesos en memoria. Cuando ninguna de tus máquinas tiene
suficiente por sí sola, litek8s puede juntar varias: un desktop, una laptop, un
NAS, una VM barata en la nube. El backend RPC de
[llama.cpp](https://github.com/ggml-org/llama.cpp) hace el reparto; litek8s
corre las piezas, las encuentra, las reinicia y muestra lo que consumen. Los
manifiestos están en [`examples/llm-rpc/`](../../examples/llm-rpc/README.md).

Qué esperar, según las mediciones de abajo: un modelo que no cabe en ningún
nodo corre en tres (Qwen2.5 7B, 4.4 GiB de pesos, en tres máquinas de 4 GiB), y
responde más lento que una sola máquina que pudiera contenerlo entero. Cada
token pasa por cada worker por turno y cruza la red, así que juntar máquinas
compra memoria, no velocidad.

## Cómo funciona

```text
 OpenAI client ──▶ Service llama (NodePort) ──▶ llama-server (Deployment, model on a PVC)
                                                    │ --rpc ipA:50052,ipB:50052,ipC:50052
                     headless Service rpc-server ┄┄┄┤ (ready pod IPs, watched)
                                                    ├──▶ rpc-server on node A ─┐
                                                    ├──▶ rpc-server on node B  ├ DaemonSet,
                                                    └──▶ rpc-server on node C ─┘ layers in RAM
```

- **`rpc-server`**, un DaemonSet en cada nodo con la etiqueta
  `llm.litek8s.dev/rpc=true` (y que tolera el taint
  `llm.litek8s.dev/dedicated`, para nodos reservados para este trabajo). Cada
  uno ofrece la CPU y la memoria de su nodo (o su GPU, con un build de
  llama.cpp para GPU) en la IP de su pod, puerto 50052. Sin host network, sin
  host port: entre nodos el tráfico va por el overlay WireGuard de litek8s, lo
  cual importa porque el protocolo RPC no tiene autenticación. `--cache` guarda
  en el disco de su nodo los tensores que recibió un worker
  (`/var/cache/litek8s-llm-rpc`), así que un worker reiniciado los lee
  localmente en lugar de por la red.
- **`llama-server`**, un Deployment de una réplica. Unos init containers
  descargan llama.cpp y el modelo (una sola vez, en un PersistentVolumeClaim;
  la clase `local-path` de litek8s lo mantiene en el nodo donde cae el servidor
  la primera vez). Su script de arranque le pregunta al Service headless
  `rpc-server` por los workers listos, espera a que haya `RPC_MIN_WORKERS` y a
  que el conjunto se mantenga sin cambios durante 5 s, y ejecuta
  `llama-server --rpc <ip:port,...> -ngl 999`: cada capa va a un worker, en
  proporción a la memoria que reporta cada uno, y los token embeddings se
  quedan en el nodo del servidor. El script sigue vigilando la respuesta:
  cuando el conjunto cambia, detiene llama-server (SIGKILL después de 10 s, ya
  que un worker cuyo nodo se quedó sin energía nunca responde) y lo vuelve a
  arrancar con el nuevo conjunto. El cliente RPC de llama.cpp no se reconecta,
  y un worker nuevo solo recibe capas cuando el modelo se vuelve a cargar.
- **`llama`**, un Service NodePort en el puerto 8080: la API compatible con
  OpenAI (`/v1/chat/completions`, `/v1/completions`, `/v1/models`), `/health`
  y métricas de Prometheus en `/metrics`. En un cluster de desarrollo, el
  NodePort también queda abierto en el `localhost` de tu máquina.

Las imágenes de contenedor oficiales de llama.cpp se compilan sin el backend
RPC (`GGML_RPC` está desactivado en su Dockerfile); sus tarballs de release sí
lo traen. Por eso ambos pods corren la imagen oficial
`ghcr.io/ggml-org/llama.cpp:server-b11669` (llama.cpp v0.7.0), por sus
bibliotecas, y un init container desempaqueta el tarball del mismo build para
la CPU del nodo, verificado contra un sha256 fijado. Los nodos arm64 y amd64 se
mezclan libremente; no se compila nada.

## Ponlo a correr

En un cluster de desarrollo (macOS o Linux):

```bash
litek8s dev up --name llm --nodes 3
kubectl config use-context litek8s-llm
kubectl label node --all llm.litek8s.dev/rpc=true
kubectl apply -f examples/llm-rpc/
kubectl -n llm rollout status deploy/llama-server --timeout=10m

PORT=$(kubectl -n llm get svc llama -o jsonpath='{.spec.ports[0].nodePort}')
curl -s localhost:$PORT/v1/chat/completions -H 'Content-Type: application/json' \
  -d '{"messages":[{"role":"user","content":"In one sentence, what is a Kubernetes DaemonSet?"}],"max_tokens":60}'
```

Los tres nodos de un cluster de desarrollo comparten una VM o sandbox, así que
esto muestra la mecánica, no la ganancia de memoria. En máquinas reales, únelas
primero ([Multi-node](../multi-node.md)), luego etiqueta las que deben
contener capas y define el modelo en
[`00-config.yaml`](../../examples/llm-rpc/00-config.yaml):

```bash
kubectl label node desktop laptop nas llm.litek8s.dev/rpc=true
# 00-config.yaml: MODEL_URL, MODEL_FILE, MODEL_SHA256, RPC_MIN_WORKERS: "3"
kubectl apply -f examples/llm-rpc/
```

Funciona cualquier GGUF que cargue llama.cpp; `MODEL_SHA256` es el sha256 del
archivo según su página de Hugging Face (vacío omite la verificación). Para
cambiar el modelo después, edita el ConfigMap y reinicia el servidor:
`kubectl -n llm rollout restart deploy/llama-server`. Elimina todo con
`kubectl delete ns llm`.

### Ajustes

- **Porciones.** Cada worker reporta la RAM total de su nodo (el dispositivo
  CPU de llama.cpp no ve un límite de cgroup), y llama.cpp le asigna capas en
  proporción. Por eso el DaemonSet no define límite de memoria: un límite por
  debajo de la porción haría que el worker muriera por OOM en lugar de recibir
  menos capas. Para fijar las porciones, agrega `--tensor-split 2,1,1` a
  `LLAMA_ARGS`, en el orden de la línea de log `rpc workers:` (ordenada por IP
  de pod).
- **Contexto.** `LLAMA_ARGS: "-c 4096"`; la caché KV va a parar a los workers
  junto con sus capas.
- **Ver el reparto.** `-lv 4` en `LLAMA_ARGS` hace que llama-server registre
  los dispositivos y cuántos MiB del modelo recibió cada uno (también registra
  los detalles de cada petición).
- **GPUs.** Un nodo con GPU corre un build de `rpc-server` para GPU (los
  tarballs CUDA/Vulkan/ROCm de llama.cpp y las imágenes
  `server-cuda`/`server-vulkan`); el worker ofrece entonces la GPU en lugar de
  la CPU. No se probó aquí.

## Mediciones

La misma petición cada vez (46 tokens de prompt, 128 generados, temperatura
0), tres ejecuciones después de un calentamiento, con
[`bench.sh`](../../examples/llm-rpc/bench.sh), que lee los tiempos que reporta
el propio llama-server. "Local" es `llama-bench` en el pod del servidor sin
RPC en absoluto. La generación son tokens/s de salida; el prompt son tokens/s
de procesamiento del prompt.

**Cluster multinodo real, amd64**: tres VMs Ubuntu 24.04 (4 vCPU, 4 GiB cada
una) sobre qemu/KVM en un Ryzen 7 5800X, en una LAN virtual, unidas con
`litek8s agent --join` (bench/cluster.sh), litek8s 0.1.2, crun 1.14.1.

| modelo | dónde | generación | prompt | memoria del modelo por worker |
|---|---|---:|---:|---|
| Qwen2.5 1.5B Q4_K_M (1.1 GB) | local, sin RPC | 41.1 | 173 | – |
| | 1 worker (mismo nodo) | 33.0 | 92 | 935 MiB |
| | 3 workers en 3 nodos | 26.5 | 88 | 276 / 265 / 394 MiB |
| Qwen2.5 7B Q4_K_M (4.7 GB) | 3 workers en 3 nodos | 7.5 | 19.7 | 1336 / 1302 / 1530 MiB |

Los 4.4 GiB de pesos del modelo 7B no caben en ninguno de estos nodos de
3.9 GiB; repartidos entre tres, cada nodo usó 1.8–2.0 GiB de su memoria con
todo lo demás corriendo (los working sets de los workers fueron 1.5, 1.5 y
1.8 GB en `litek8s top`). El nodo del servidor se queda con los token
embeddings (125 MiB para 1.5B, 292 MiB para 7B, mapeados en memoria desde el
archivo del modelo). El primer arranque descargó el archivo de 4.4 GiB en
64 s, y cargarlo (las capas enviadas a los workers) tardó alrededor de un
minuto; un reinicio posterior de llama-server tardó 40 s, con los workers
leyendo sus tensores en caché desde el disco.

**Cluster de desarrollo en una Mac** (Apple M3 Max, `litek8s dev up --name llm
--nodes 3`, litek8s 0.1.2; los tres nodos comparten una VM con 7 vCPUs):

| modelo | dónde | generación | prompt | memoria del modelo por worker |
|---|---|---:|---:|---|
| Qwen2.5 0.5B Q4_K_M (491 MB) | local, sin RPC | 200 | 454 | – |
| | 1 worker | 104 | 228 | 374 MiB |
| | 3 workers | 75 | 220 | 90 / 79 / 205 MiB |

Desde `kubectl apply` en un cluster nuevo hasta la primera respuesta: 171 s
(la imagen en tres nodos y la descarga del modelo).

Así que el salto RPC por sí solo cuesta entre una quinta parte (amd64) y la
mitad (la Mac rápida) de la velocidad, y cada máquina adicional un poco más.
El último worker contiene la capa de salida y devuelve los logits de cada
token: el tamaño del vocabulario por 4 bytes, unos 600 KB por token para los
152k tokens de Qwen (medido: 5.1 MB/s desde ese worker durante el minuto del
benchmark de 1.5B), más el grafo de cómputo que llama-server envía en cada
paso. Gigabit cableado está bien; con Wi-Fi, el enlace marca la velocidad. Las
porciones desiguales, como las de arriba, se deben a esa capa de salida.

## Cuando un worker desaparece

| qué pasó | la petición en curso | la API otra vez disponible |
|---|---|---|
| se borró un pod worker (el DaemonSet lo recrea), Linux, 1.5B | falla de inmediato: `curl: (52) Empty reply from server` | 12 s después, los 3 workers |
| lo mismo, 7B | falla de inmediato | 38 s después |
| lo mismo, cluster de desarrollo en Mac, 0.5B | falla de inmediato | 18 s después |
| un nodo apagado (Linux, 1.5B, grace period de nodo por defecto de 40 s) | se queda colgada y luego falla a los 52 s | 62 s después del apagado, con los 2 workers restantes |
| ese nodo encendido de nuevo | – | de vuelta con 3 workers 25 s después del encendido, ~12 s de ellos arrancando la VM |

llama.cpp aborta llama-server cuando se rompe la conexión con un worker
(`Remote RPC server crashed or returned malformed response`), lo que mata las
peticiones en curso; no tiene reintentos ni failover, así que un cliente
debería reintentar. Lo reinicia el script de arranque, no Kubernetes, así que
el pod nunca entra en CrashLoopBackOff. Un nodo que se queda sin energía se
detecta cuando dejan de llegar sus heartbeats (`--node-monitor-grace-period`,
40 s): sus pods pasan a NotReady, salen del Service headless y el script
vuelve a cargar el modelo sin él, siempre que a los workers restantes les
alcance la memoria. Hasta entonces, las peticiones que necesitan el worker
caído se quedan esperándolo.

## Observarlo

- **Qué workers, qué porción**: `kubectl -n llm logs deploy/llama-server
  | grep 'rpc workers'` lista los workers en uso y cada cambio; con `-lv 4`,
  las líneas `load_tensors: RPC0[ip:port] model buffer size` dan la porción de
  cada worker.
- **CPU y memoria por worker**: `litek8s top -n llm` (o `kubectl top
  pods -n llm`), y la vista Observe de `litek8s ui` (9), que lista los pods por
  CPU o memoria con sparklines de 30 minutos. La memoria de un worker es su
  porción del modelo. PromQL sobre las métricas integradas
  ([observability.md](../observability.md)):

  ```bash
  litek8s query 'sum by (pod, node) (container_memory_working_set_bytes{namespace="llm"})'
  litek8s query 'sum by (pod) (rate(container_cpu_usage_seconds_total{namespace="llm"}[1m]))'
  litek8s query 'sum by (pod) (rate(container_network_transmit_bytes_total{namespace="llm"}[1m]))'
  ```

- **Tokens/s y cola**: el propio `/metrics` de llama-server
  (`llamacpp:predicted_tokens_seconds`, `llamacpp:prompt_tokens_seconds`,
  `llamacpp:requests_processing`, `llamacpp:requests_deferred`, ...). litek8s
  todavía no hace scrape de los endpoints `/metrics` de las aplicaciones: léelo
  con `curl localhost:$PORT/metrics`, o apunta un Prometheus al Service
  `llama`.
- **Logs**: `litek8s logs` busca en los logs de todos los pods, incluidos los
  de contenedores reiniciados o borrados, p. ej. `litek8s logs -n llm
  -g 'RPC server crashed'`.

## Límites

- llama.cpp describe su backend RPC como una prueba de concepto e inseguro:
  mantén los workers en IPs de pod (como aquí) y el cluster en una red de
  confianza.
- Más máquinas no aceleran nada, como se midió arriba; el objetivo es que
  quepa un modelo que de otro modo no cabe.
- Un solo llama-server; es el punto único de falla, y un worker que
  desaparece cuesta todas las peticiones en curso más una recarga del modelo.
- Las porciones de memoria siguen la RAM total, no la RAM libre ni los
  límites; fíjalas con `--tensor-split` en máquinas que hacen otro trabajo.
