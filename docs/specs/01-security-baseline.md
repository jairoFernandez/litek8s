# Spec 01: Security Baseline

- Prioridad: P0
- Estado: Draft
- Depende de: nada
- Bloquea: `02-secrets-and-config`, `05-sandbox-isolation`

## Contexto

Hoy `litek8s` crea contenedores Docker con una configuración bastante mínima: imagen, `entrypoint`, `cmd`, `env`, puertos y límites de CPU/memoria. Falta una postura segura por defecto para ejecución en hosts pequeños, entornos mixtos y nodos remotos no “enterprise”.

## Problema

Sin un baseline de seguridad:

- cada workload hereda defaults inseguros del runtime;
- los futuros secrets quedarían más expuestos;
- los nodos edge o baratos tienen mayor riesgo si un contenedor escapa o abusa de privilegios;
- no existe una forma declarativa de pedir más aislamiento o endurecimiento.

## Objetivos

- Introducir defaults seguros para todos los despliegues nuevos.
- Exponer presets simples de seguridad sin hacer el manifiesto inmanejable.
- Permitir overrides explícitos y auditables cuando un workload realmente necesite más privilegios.
- Hacer visible la postura de seguridad en `describe`, `events` y eventualmente `top/tree`.

## No objetivos

- No resolver sandboxing fuerte multi-runtime en esta fase.
- No implementar políticas multi-tenant completas tipo PodSecurity admission.
- No cubrir SELinux/AppArmor por distro de forma perfecta en v1.

## UX propuesta

### Manifiesto

Agregar en `spec.template.spec`:

```yaml
securityProfile: standard
```

Valores iniciales:

- `standard` — default seguro y compatible.
- `isolated` — más estricto, pensado para servicios expuestos o workloads no confiables.
- `privileged` — opt-in explícito y ruidoso; solo para casos excepcionales.

Agregar un subset de overrides avanzados opcionales:

```yaml
security:
  runAsNonRoot: true
  readOnlyRootFilesystem: true
  allowPrivilegeEscalation: false
  dropCapabilities: ["ALL"]
  addCapabilities: []
  noNewPrivileges: true
  pidsLimit: 256
  tmpfs:
    - /tmp
```

### CLI

- `describe deployment` debe mostrar el perfil efectivo y overrides aplicados.
- `events` debe registrar cuándo un workload pide `privileged` o rompe defaults seguros.

## Semántica de perfiles

### `standard`

- `runAsNonRoot=true` cuando la imagen lo soporte.
- `allowPrivilegeEscalation=false`
- `noNewPrivileges=true`
- `dropCapabilities=["ALL"]`
- `readOnlyRootFilesystem=false`
- `pidsLimit=512`

### `isolated`

- Todo lo de `standard`
- `readOnlyRootFilesystem=true`
- `tmpfs=["/tmp"]`
- `pidsLimit=256`
- `network egress` todavía sin cambios en esta fase

### `privileged`

- Sin endurecimiento agresivo.
- Requiere confirmación declarativa explícita.
- Debe generar `Warning` event por defecto.

## Cambios técnicos esperados

### Manifiesto

Archivos probables:

- `src/manifest.rs`

Cambios:

- agregar parsing/validación de `securityProfile`
- agregar estructura `SecurityOverrides`
- definir defaults estables

### Runtime Docker

Archivos probables:

- `src/runtime/docker.rs`

Cambios:

- mapear perfiles a `HostConfig` y `Config`
- configurar `cap_drop`, `cap_add`, `readonly_rootfs`, `security_opt`, `tmpfs`, `pids_limit`
- documentar qué ocurre si el host no soporta alguna opción

### Estado y descripción

Archivos probables:

- `src/protocol.rs`
- `src/daemon.rs`

Cambios:

- exponer perfil efectivo en descripción y vistas
- generar condiciones o warnings legibles

## Fases sugeridas

### Fase 1

- `securityProfile` con presets
- `standard` como default
- `describe` y `events`

### Fase 2

- overrides controlados
- validación de combinaciones incompatibles

### Fase 3

- capability discovery por nodo
- warnings si un nodo no soporta un ajuste solicitado

## Riesgos

- Algunas imágenes oficiales fallan si forzamos non-root sin más contexto.
- `readOnlyRootFilesystem` puede romper aplicaciones que escriben en `/tmp` o rutas de trabajo.
- El soporte de opciones concretas de Docker puede variar entre Linux, Docker Desktop y OrbStack.

## Decisiones abiertas

- ¿El manifiesto debe adoptar un estilo propio (`securityProfile`) o alinearse más con `securityContext` de Kubernetes?
- ¿`standard` debe forzar non-root o solo recomendarlo cuando la imagen lo permita?
- ¿`privileged` se permitirá en cualquier nodo o solo en nodos etiquetados explícitamente?

## Definition of Done

- Existe `securityProfile` en el manifiesto.
- Todos los despliegues sin configuración explícita reciben un baseline seguro.
- `describe deployment` muestra el perfil efectivo.
- Se generan eventos claros cuando se usa `privileged`.
- Hay tests unitarios o de integración para parsing y mapeo a Docker.

## Próximo paso recomendado

Definir el shape exacto del manifiesto y cerrar la discusión entre `securityProfile` vs `securityContext` subset.
