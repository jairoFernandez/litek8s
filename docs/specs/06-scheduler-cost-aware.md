# Spec 06: Scheduler Cost-Aware

- Prioridad: P1
- Estado: Draft
- Depende de: nada estrictamente, pero se beneficia de `04-observability`
- Se beneficia de: `05-sandbox-isolation`

## Contexto

El scheduler actual elige nodos de forma muy simple: respeta `nodeName`, `nodeSelector`, nodos cordoned y elige una opción schedulable. Eso es suficiente para demos, pero no para aprovechar mejor un fleet de máquinas baratas y heterogéneas.

## Problema

Sin capacidad y costo como señales de scheduling:

- no hay forma de exprimir nodos pequeños o económicos de manera controlada;
- no existe noción de requests/allocatable;
- el sistema no diferencia workloads sensibles de cargas batch o edge.

## Objetivos

- Introducir capacidad declarada/descubierta por nodo.
- Introducir `requests` además de `limits`.
- Agregar un scoring policy simple y entendible.
- Permitir decisiones orientadas a costo, spread o binpack.

## No objetivos

- No hacer un scheduler multi-constraint de nivel Kubernetes en v1.
- No meter preemption compleja desde el principio.
- No hacer autoscaling de nodos en esta fase.

## UX propuesta

### Nodo

Extender metadata de nodo con:

- `capacity.cpu`
- `capacity.memory`
- `arch`
- `costTier`
- `storageClass`
- `supportsSandboxed`

### Deployment

Agregar:

```yaml
resources:
  requests:
    cpu: "250m"
    memory: "128Mi"
  limits:
    cpu: "500m"
    memory: "256Mi"

placementPolicy: cheapest-first
```

Valores iniciales de `placementPolicy`:

- `balanced`
- `binpack`
- `spread`
- `cheapest-first`

## Diseño de scoring inicial

### Requisitos mínimos

- respetar `nodeName`, `nodeSelector`, cordon y capacidades requeridas
- filtrar nodos sin capacidad suficiente para `requests`
- luego scorear con una función pequeña y auditable

### Score sugerido

- capacidad remanente tras requests
- peso por `costTier`
- afinidad por `arch` o capacidades especiales
- penalización por nodos degradados

## Cambios técnicos esperados

Archivos probables:

- `src/node.rs`
- `src/manifest.rs`
- `src/store.rs`
- `src/daemon.rs`
- `src/reconciler.rs`

Cambios:

- parsear `requests`
- persistir capacidad/costo del nodo
- reescribir selección simple por un pipeline `filter + score`

## Fases sugeridas

### Fase 1

- requests en manifiesto
- capacidad manual por nodo
- scheduler `balanced`

### Fase 2

- `placementPolicy`
- score por costo y consumo

### Fase 3

- capacidad descubierta automáticamente
- reserva/allocatable y mejor reporting

## Riesgos

- Si solo usamos labels manuales, el estado real del host puede divergir rápido.
- Si se implementa capacity accounting sin cuidado, los reinicios y drift del runtime pueden desalinear la contabilidad.
- Hardware pequeño y bursty puede hacer que un scoring demasiado rígido dé malos resultados.

## Decisiones abiertas

- ¿La capacidad del nodo será manual, descubierta o híbrida?
- ¿`costTier` será label libre o campo estructurado?
- ¿`placementPolicy` vive en `Deployment` o es un default global del daemon?

## Definition of Done

- El manifiesto soporta `requests`.
- Los nodos exponen capacidad útil para scheduling.
- El scheduler filtra por capacidad y scorea con una policy inicial.
- `describe node` muestra capacidad y carga aproximada.

## Próximo paso recomendado

Definir el modelo mínimo de capacidad por nodo y el subset inicial de `requests`.
