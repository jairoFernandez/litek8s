# Spec 05: Sandbox Isolation

- Prioridad: P1
- Estado: Draft
- Depende de: `01-security-baseline`
- Se beneficia de: `06-scheduler-cost-aware`

## Contexto

Hoy `litek8s` solo tiene runtime Docker. Eso es suficiente para arrancar, pero no permite ofrecer distintos niveles de aislamiento según el tipo de workload o el tipo de nodo.

## Problema

Sin aislamiento configurable:

- todos los workloads comparten el mismo modelo de riesgo;
- no hay ruta para ejecutar código menos confiable en nodos baratos o edge;
- el usuario no puede pedir “sandbox fuerte” sin salir del producto.

## Objetivos

- Introducir una abstracción de runtime/sandbox.
- Permitir perfiles de aislamiento simples y legibles.
- Mantener una ruta compatible con Docker clásico.
- Preparar el terreno para gVisor/Kata/MicroVM sin implementarlo todo de golpe.

## No objetivos

- No implementar Firecracker completo en la primera fase.
- No soportar todos los runtimes del ecosistema.
- No ocultar las limitaciones de cada nodo; deben hacerse visibles.

## UX propuesta

### Manifiesto

Agregar en `spec.template.spec`:

```yaml
sandboxClass: process
```

Valores sugeridos:

- `process` — runtime normal endurecido
- `sandboxed` — aislamiento reforzado, por ejemplo gVisor/Kata cuando exista soporte
- `microvm` — experimental, solo en nodos con capacidad explícita

### Comportamiento

- Si el nodo no soporta la clase pedida, el scheduler no debe asignar allí.
- `describe deployment` y `describe node` deben mostrar la clase solicitada y la capacidad real.

## Diseño técnico

### Fase 1

- convertir `src/runtime` en una abstracción real
- capability discovery por nodo
- `sandboxClass=process` solamente, pero ya con plomería completa

### Fase 2

- soporte para un runtime más aislado si está disponible en el host
- validaciones de compatibilidad y eventos claros

### Fase 3

- microVM experimental
- afinidad/scheduling por clase de sandbox

## Cambios técnicos esperados

Archivos probables:

- `src/runtime/mod.rs`
- `src/runtime/docker.rs`
- `src/node.rs`
- `src/manifest.rs`
- `src/daemon.rs`

Cambios:

- trait común de runtime
- discovery de capacidades por nodo
- mapeo de `sandboxClass` a runtime efectivo

## Integración con scheduler

Esta spec se vuelve mucho más útil si los nodos pueden anunciar:

- `supportsSandboxed=true`
- `supportsMicrovm=true`
- `virtType`, `arch`, `memoryClass`, `costTier`

## Riesgos

- El soporte real de runtimes aislados cambia mucho entre Linux/macOS.
- Complejidad alta si se intenta meter múltiples runtimes demasiado pronto.
- Puede romper la simplicidad del proyecto si no se limita bien el alcance.

## Decisiones abiertas

- ¿El nombre será `sandboxClass`, `runtimeClassName` o algo más alineado a Kubernetes?
- ¿Cuál será el primer runtime alternativo soportado realmente?
- ¿La capability discovery será activa o manual por labels de nodo?

## Definition of Done

- Existe una abstracción de runtime preparada para más de un backend.
- El manifiesto puede pedir una clase de sandbox.
- El sistema informa cuándo una clase pedida no está disponible.
- El scheduler evita nodos incompatibles.

## Próximo paso recomendado

Diseñar el trait de runtime y el mecanismo de capability discovery por nodo.
