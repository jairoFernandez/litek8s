---
title: "Un LLM repartido entre tres máquinas de 4 GiB: ganas memoria, no velocidad"
description: "Qwen2.5 7B necesita 4.4 GiB de pesos y ningún nodo los tenía. Repartido entre tres con el backend RPC de llama.cpp en litek8s, dio 7.5 tokens/s."
date: 2026-10-30
tags: [llm, llama.cpp, home lab, varios nodos]
docs: [llm-cluster, multi-node, observability]
lang: es
translation: one-llm-across-three-machines
---

Un modelo necesita sus pesos en memoria. Qwen2.5 7B, cuantizado a Q4_K_M,
son 4.4 GiB de pesos. Teníamos tres máquinas de 4 GiB cada una (3.9 GiB
usables), así que no cabía en ninguna. Repartido entre las tres, respondió a
**7.5 tokens por segundo**.

Ese es el post en una línea, y la mitad honesta importa tanto como la
divertida: juntar máquinas te da memoria, no velocidad. Cada token pasa por
cada worker por turno y cruza la red. Tres máquinas son más lentas que una
sola que pudiera cargar el modelo entera. La gracia es correr un modelo que,
de otra forma, no corre.

## Las piezas

[llama.cpp](https://github.com/ggml-org/llama.cpp) hace la parte difícil. Su
backend RPC permite que un `llama-server` deje capas del modelo en otras
máquinas, cada una con un pequeño `rpc-server`. Lo que no hace es encontrar
esas máquinas, reiniciarlas ni darse cuenta cuando una desaparece. Esa parte
la hace litek8s, con objetos de Kubernetes normales:

```text
 OpenAI client ──▶ Service llama (NodePort) ──▶ llama-server (Deployment, model on a PVC)
                                                    │ --rpc ipA:50052,ipB:50052,ipC:50052
                     headless Service rpc-server ┄┄┄┤ (ready pod IPs, watched)
                                                    ├──▶ rpc-server on node A ─┐
                                                    ├──▶ rpc-server on node B  ├ DaemonSet,
                                                    └──▶ rpc-server on node C ─┘ layers in RAM
```

- **`rpc-server` es un DaemonSet** en cada nodo con la etiqueta
  `llm.litek8s.dev/rpc=true`. Escucha solo en la IP de su pod, nunca en un
  puerto del host. Eso importa: el protocolo RPC no tiene autenticación, y
  entre nodos el tráfico va por el overlay WireGuard de litek8s. `--cache`
  guarda en el disco del nodo los tensores recibidos, así un worker que se
  reinicia los lee de ahí en vez de pedirlos otra vez por la red.
- **`llama-server` es un Deployment de una réplica.** Su script de arranque
  le pregunta al Service headless por los workers listos, espera a que el
  conjunto no cambie durante 5 segundos y corre
  `llama-server --rpc <workers> -ngl 999`. Cada capa va a un worker, en
  proporción a la memoria que reporta cada uno. Cuando el conjunto cambia,
  el script reinicia llama-server con el nuevo, porque el cliente RPC de
  llama.cpp no se reconecta.
- **`llama` es un Service NodePort** con la API compatible con OpenAI en el
  puerto 8080, `/health` y métricas de Prometheus en `/metrics`.

Las imágenes oficiales de llama.cpp vienen compiladas sin el backend RPC;
los tarballs de sus releases sí lo traen. Por eso los pods usan la imagen
oficial por sus librerías, y un init container desempaca el tarball del mismo
build, verificado contra un sha256 fijo. Nodos arm64 y amd64 se mezclan sin
problema y no se compila nada.

## Los números

La misma petición cada vez (46 tokens de prompt, 128 generados, temperatura
0), tres corridas después de un calentamiento, leídas de los tiempos que
reporta el propio llama-server con
[`bench.sh`](https://github.com/jairoFernandez/litek8s/blob/main/examples/llm-rpc/bench.sh).
El cluster: tres VMs Ubuntu 24.04 (4 vCPU y 4 GiB cada una) sobre qemu/KVM
en un Ryzen 7 5800X, en una LAN virtual, unidas con `litek8s agent --join`.
"Local" es `llama-bench` dentro del pod del servidor, sin RPC.

| modelo | dónde | generación tok/s | prompt tok/s | memoria del modelo por worker |
|---|---|---:|---:|---|
| Qwen2.5 1.5B Q4_K_M (1.1 GB) | local, sin RPC | 41.1 | 173 | – |
| | 1 worker (mismo nodo) | 33.0 | 92 | 935 MiB |
| | 3 workers en 3 nodos | 26.5 | 88 | 276 / 265 / 394 MiB |
| Qwen2.5 7B Q4_K_M (4.7 GB) | 3 workers en 3 nodos | 7.5 | 19.7 | 1336 / 1302 / 1530 MiB |

No hay fila "local" para el 7B, y justamente ese es el punto: no cabe en
ninguno de estos nodos. Repartido entre tres, cada nodo usó entre 1.8 y
2.0 GiB de su memoria con todo lo demás corriendo.

Las filas del 1.5B muestran el precio. De no usar RPC (41.1 tokens/s) a un
worker en la misma máquina (33.0) y a tres workers en tres máquinas (26.5),
cada paso cuesta velocidad. En un cluster de desarrollo en un Apple M3 Max,
con un modelo de 0.5B, los mismos tres pasos fueron 200 → 104 → 75. El salto
RPC por sí solo cuesta entre una quinta parte (amd64) y la mitad (el Mac,
que es rápido) de la velocidad, y cada máquina extra un poco más.

¿A dónde se va? El último worker tiene la capa de salida y devuelve los
logits de cada token: el tamaño del vocabulario por 4 bytes, unos 600 KB por
token con los 152 mil tokens del vocabulario de Qwen (medido: 5.1 MB/s desde
ese worker durante el benchmark del 1.5B), más el grafo de cómputo que
llama-server manda en cada paso. Con gigabit por cable no hay drama. Por
Wi-Fi, el enlace pone la velocidad. Esa capa de salida es también la razón
de que los repartos de arriba sean disparejos.

Hasta la primera respuesta en el cluster del 7B: bajar el archivo de 4.4 GiB
tomó 64 s y cargarlo (mandar las capas a los workers) cerca de un minuto. Un
reinicio posterior de llama-server tomó 40 s, con los workers leyendo sus
tensores desde la caché en disco.

## Cuando una máquina se va

Esto es lo que de verdad pone a prueba un homelab. Borramos pods de workers
y apagamos un nodo en plena petición:

| qué pasó | la petición en curso | la API de vuelta |
|---|---|---|
| se borra un pod worker, 1.5B | falla en el acto | 12 s después, con los 3 workers |
| lo mismo, 7B | falla en el acto | 38 s después |
| se apaga un nodo (1.5B, periodo de gracia por defecto de 40 s) | se cuelga y falla a los 52 s | 62 s después del apagado, con los 2 workers que quedan |
| ese nodo se vuelve a prender | – | otra vez con 3 workers 25 s después de prenderlo |

llama.cpp no tiene reintentos ni failover: cuando se corta la conexión con un
worker, aborta llama-server y se lleva por delante todas las peticiones en
curso. Así que tu cliente debería reintentar. Quien reinicia el servidor es
el script de arranque, no Kubernetes, así que el pod nunca cae en
CrashLoopBackOff. Un nodo apagado se detecta cuando dejan de llegar sus
heartbeats; su worker pasa a NotReady, sale del Service headless y el script
recarga el modelo sin él, siempre que a los workers restantes les alcance la
memoria.

## Cómo verlo

Quieres saber qué máquina tiene qué. La memoria de cada worker es su parte
del modelo:

```bash
kubectl -n llm logs deploy/llama-server | grep 'rpc workers'   # which workers it uses
litek8s top -n llm                                            # memory per worker = its share
curl -s localhost:$PORT/metrics                               # llama-server's Prometheus metrics
```

Las métricas integradas también responden PromQL, por ejemplo el tráfico de
salida por pod (ver [Métricas, logs y alertas](/es/docs/observability/)):

```bash
litek8s query 'sum by (pod) (rate(container_network_transmit_bytes_total{namespace="llm"}[1m]))'
```

## Lo que hay que tener en cuenta

- llama.cpp llama a su backend RPC una prueba de concepto insegura. Deja los
  workers en IPs de pod, como hacen los manifiestos, y el cluster en una red
  de confianza.
- Más máquinas no lo van a hacer más rápido. Los números de arriba lo dicen.
- Hay un solo llama-server. Es el punto único de falla, y un worker que se va
  cuesta todas las peticiones en curso más una recarga del modelo.
- Cada worker reporta la RAM total de su nodo, no la libre (llama.cpp no ve
  los límites del cgroup). En máquinas que hacen otras cosas, fija los
  repartos con `--tensor-split` en `LLAMA_ARGS`.
- llama.cpp tiene builds de `rpc-server` para GPU; aquí no los probamos.

## Pruébalo

En una laptop, un cluster de desarrollo de 3 nodos muestra la mecánica (los
tres nodos comparten una VM o sandbox, así que no la ganancia de memoria):

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

En un Mac, desde `kubectl apply` en un cluster nuevo hasta la primera
respuesta pasaron 171 s, casi todo bajando la imagen en los tres nodos y
descargando el modelo.

Para la versión de verdad, primero une tus máquinas
([Varios nodos y HA](/es/docs/multi-node/)), después etiqueta las que van a
cargar capas y elige el modelo en `00-config.yaml`:

```bash
kubectl label node desktop laptop nas llm.litek8s.dev/rpc=true
# 00-config.yaml: MODEL_URL, MODEL_FILE, MODEL_SHA256, RPC_MIN_WORKERS: "3"
kubectl apply -f examples/llm-rpc/
```

Sirve cualquier GGUF que cargue llama.cpp. La guía completa, el ajuste fino
y el detalle de las fallas están en
[Un LLM entre varias máquinas](/es/docs/llm-cluster/). Para borrarlo todo:
`kubectl delete ns llm`.
