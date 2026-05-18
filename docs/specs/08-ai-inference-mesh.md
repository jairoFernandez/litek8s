# Spec 08: AI Inference Mesh

- Prioridad: P3
- Estado: Deferred
- Depende de: `03-vpn-pack`, `04-observability`, `06-scheduler-cost-aware`
- Se beneficia de: `05-sandbox-isolation`, `07-vpc-lite`

## Contexto

Hyperspace muestra una dirección interesante: usar varias máquinas como un pool distribuido para servir modelos de IA, con discovery de nodos, capacidades anunciadas, un gateway compatible con APIs conocidas y, en su versión más ambiciosa, paralelización de inferencia entre máquinas.

Para `litek8s`, esta idea es útil como extensión futura, pero no debe desplazar el foco actual del proyecto: orquestar varias máquinas virtuales o nodos modestos con pocos recursos, manteniendo una experiencia pequeña y entendible.

## Veredicto

El concepto es aplicable si se aterriza en fases.

Alta factibilidad:

- pool de nodos para inferencia local;
- scheduling por CPU, RAM, GPU, VRAM, arquitectura y costo;
- gateway compatible con OpenAI API;
- enrutamiento hacia runtimes existentes como Ollama, llama.cpp server o vLLM;
- reporting de modelos disponibles por nodo.

Factibilidad media:

- discovery dinámico de nodos y modelos;
- cache o distribución de pesos entre máquinas;
- mesh privado para llamadas internas entre gateway y workers;
- failover simple entre nodos con el mismo modelo.

Factibilidad baja para el core de `litek8s`:

- partir un único modelo grande entre máquinas heterogéneas;
- implementar tensor parallelism o pipeline parallelism propio;
- coordinar inferencia distribuida token a token dentro del orquestador.

La recomendación es que `litek8s` orqueste runtimes de inferencia existentes, no que implemente el motor de inferencia.

## Objetivos

- Permitir que `litek8s` modele nodos con capacidades útiles para IA.
- Ejecutar y reconciliar runtimes de inferencia como deployments normales.
- Ofrecer un gateway compatible con OpenAI API para enrutar solicitudes al nodo correcto.
- Mantener bajo el consumo de control plane y workers.
- Reutilizar el futuro mesh privado para no exponer backends de inferencia en redes públicas.

## No objetivos

- No competir con Kubernetes completo, Ray, vLLM distributed serving o Slurm.
- No implementar sharding de modelos dentro de `litek8s`.
- No introducir economía, marketplace, reputación, tokens o proof-of-work.
- No requerir GPU para el camino feliz; debe seguir funcionando con CPU en nodos modestos.
- No hacer discovery público P2P en la primera versión.

## UX propuesta

### Nodo con capacidades IA

```bash
litek8s node add vm-a --ssh vm-a --label role=ai --label arch=amd64
litek8s node annotate-capacity vm-a \
  --cpu 4 \
  --memory 8Gi \
  --gpu none \
  --cost-tier cheap
```

Fase posterior con GPU:

```bash
litek8s node annotate-capacity gpu-1 \
  --cpu 16 \
  --memory 64Gi \
  --gpu nvidia \
  --vram 24Gi \
  --runtime ollama \
  --runtime vllm
```

### Deployment de runtime

```yaml
apiVersion: litek8s.dev/v1alpha1
kind: Deployment
metadata:
  name: llama-runtime
  labels:
    app: ai-runtime
    model: llama-3.1-8b
spec:
  replicas: 1
  placementPolicy: cheapest-first
  template:
    spec:
      nodeSelector:
        role: ai
      containers:
        - name: ollama
          image: ollama/ollama:latest
          ports:
            - containerPort: 11434
              hostPort: 11434
          resources:
            requests:
              cpu: "2000m"
              memory: "6Gi"
            limits:
              cpu: "4000m"
              memory: "8Gi"
```

### Recurso lógico de modelo

```yaml
apiVersion: litek8s.dev/v1alpha1
kind: AIModel
metadata:
  name: llama-3.1-8b
spec:
  runtime: ollama
  modelRef: llama3.1:8b
  selector:
    app: ai-runtime
    model: llama-3.1-8b
```

### Gateway compatible con OpenAI

```bash
litek8s ai gateway --listen 127.0.0.1:8080
curl http://127.0.0.1:8080/v1/models
curl http://127.0.0.1:8080/v1/chat/completions
```

## Diseño técnico sugerido

### Fase 0: dejarlo documentado

- Mantener esta spec como `Deferred`.
- No cambiar el runtime ni el scheduler hasta cerrar primero el camino multi-VM liviano.

### Fase 1: capacidad de nodo

- Extender `ClusterNode` con capacidad estructurada o metadata equivalente.
- Soportar CPU, memoria, arquitectura, costo y runtime disponible.
- Dejar GPU/VRAM como campos opcionales.
- Mostrar capacidad en `describe node`.

Esta fase se solapa con `06-scheduler-cost-aware` y debería implementarse allí primero.

### Fase 2: scheduler AI-aware mínimo

- Agregar `resources.requests`.
- Filtrar nodos por capacidad suficiente.
- Soportar `placementPolicy` simple: `balanced`, `binpack`, `spread`, `cheapest-first`.
- Permitir selección por labels como `role=ai`, `runtime=ollama`, `gpu=nvidia`.

### Fase 3: inventario de modelos

- Descubrir modelos disponibles por nodo mediante el agent remoto.
- Persistir un snapshot de inventario:
  - modelo;
  - runtime;
  - endpoint local;
  - tamaño aproximado;
  - estado: `Available`, `Loading`, `Missing`, `Error`.
- Exponer `litek8s get models` y `litek8s describe model`.

### Fase 4: gateway de inferencia

- Agregar un gateway HTTP compatible con un subset de OpenAI API.
- Endpoints mínimos:
  - `/v1/models`
  - `/v1/chat/completions`
  - `/v1/embeddings` si existe backend compatible.
- Seleccionar backend por `model`.
- Enrutar por service/proxy existente, prefiriendo IP privada del mesh cuando `03-vpn-pack` exista.

### Fase 5: distribución de pesos

- Agregar una estrategia simple de precarga:
  - pull por runtime;
  - cache local por nodo;
  - eventos de progreso;
  - evitar scheduling a nodos donde el modelo no cabe.
- Considerar distribución P2P solo si el pull centralizado se vuelve cuello de botella.

### Fase 6: inferencia distribuida real

Solo considerar esta fase si hay una necesidad concreta.

La implementación recomendada sería delegar en un runtime externo que ya soporte inferencia distribuida. `litek8s` debería limitarse a:

- desplegar los procesos;
- reservar nodos;
- configurar endpoints;
- observar salud;
- reiniciar componentes caídos.

## Integración con specs existentes

- `03-vpn-pack`: necesario para rutas privadas entre gateway y workers.
- `04-observability`: necesario para latencia, tokens/segundo, errores y uso de recursos.
- `05-sandbox-isolation`: útil si se ejecutan modelos o prompts de menor confianza.
- `06-scheduler-cost-aware`: dependencia principal para capacidad, requests y placement.
- `07-vpc-lite`: útil para naming interno de backends de inferencia.

## Cambios técnicos esperados

Archivos probables:

- `src/node.rs`
- `src/manifest.rs`
- `src/store.rs`
- `src/protocol.rs`
- `src/daemon.rs`
- `src/service.rs`
- `src/reconciler.rs`
- `src/cli.rs`

Nuevos módulos posibles:

- `src/ai/mod.rs`
- `src/ai/gateway.rs`
- `src/ai/models.rs`
- `src/ai/runtime.rs`

## Riesgos

- GPU discovery y métricas varían mucho por host, driver y runtime.
- Los modelos grandes pueden romper el objetivo de bajo consumo si se convierten en camino feliz.
- Un gateway sin autenticación no debe exponerse fuera de localhost o del mesh privado.
- La compatibilidad OpenAI API puede crecer demasiado si no se limita el subset.
- El sharding real de modelos puede absorber el proyecto si se intenta antes de tener scheduler, mesh y observabilidad sólidos.

## Decisiones abiertas

- ¿El recurso `AIModel` debe existir desde el inicio o basta con labels sobre deployments?
- ¿Gateway como comando del daemon principal o como deployment administrado por `litek8s`?
- ¿Provider inicial de runtime: Ollama, llama.cpp server, vLLM o una abstracción genérica?
- ¿El inventario de modelos será declarado, descubierto o híbrido?
- ¿Autenticación del gateway: token estático, mTLS, o solo bind local en la primera fase?

## Definition of Done

Para considerar esta spec activa:

- el proyecto ya debe tener una historia usable de multi-VM liviano;
- los nodos deben exponer capacidad útil para scheduling;
- debe existir conectividad privada o una ruta segura equivalente;
- debe haber métricas suficientes para depurar latencia y saturación.

Para un primer MVP de IA:

- un deployment de runtime de inferencia corre en un nodo seleccionado por capacidad;
- el gateway lista modelos y enruta una solicitud de chat al backend correcto;
- `describe node` muestra capacidad relevante;
- `events` explica decisiones de scheduling y fallos de backend;
- no se requiere sharding ni GPU obligatoria.

## Próximo paso recomendado

No implementar todavía.

Primero cerrar el camino de varias VMs con pocos recursos: conectividad privada básica, capacidad por nodo, requests, scheduler simple y observabilidad mínima. Después reabrir esta spec como extensión natural del scheduler.

## Referencias

- Hyperspace: https://www.hyperspace.sh/
- Hyperspace Docs: https://www.hyperspace.sh/docs
- Hyperspace Node: https://github.com/hyperspaceai/hyperspace-node
- Hyperspace AGI: https://github.com/hyperspaceai/agi
- Hyperspace Protocol: https://protocol.hyper.space/
