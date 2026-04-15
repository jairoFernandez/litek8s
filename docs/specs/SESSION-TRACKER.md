# Session Tracker

## Estado global

- Proyecto: `litek8s`
- Objetivo de esta línea de trabajo: seguridad, secretos, conectividad privada, observabilidad, aislamiento y scheduling orientado a nodos económicos.
- Dueño: por definir

## Estado por spec

| Spec | Prioridad | Estado | Próximo paso |
|------|-----------|--------|--------------|
| `docs/specs/01-security-baseline.md` | P0 | Draft | Definir shape final de `securityProfile` y mapeo a Docker HostConfig/Config |
| `docs/specs/02-secrets-and-config.md` | P0 | Draft | Definir modelo de cifrado en reposo y soporte inicial de `Secret` |
| `docs/specs/03-vpn-pack.md` | P0 | Draft | Elegir provider inicial: WireGuard nativo o integración Tailscale/Headscale |
| `docs/specs/04-observability.md` | P0 | Draft | Acordar endpoint Prometheus, formato de métricas y retención de eventos |
| `docs/specs/05-sandbox-isolation.md` | P1 | Draft | Diseñar abstracción de runtime y capability discovery por nodo |
| `docs/specs/06-scheduler-cost-aware.md` | P1 | Draft | Definir `requests`, capacidad por nodo y scoring policy inicial |
| `docs/specs/07-vpc-lite.md` | P2 | Draft | Delimitar el alcance mínimo viable de red privada de servicios |

## Sesión actual recomendada

- Trabajar `docs/specs/01-security-baseline.md`
- Entregable esperado: diseño aprobado de presets y campos de manifiesto

## Registro de decisiones

### Pendientes

- Decidir si el manifiesto usará un perfil propio (`securityProfile`) o un subset k8s-like de `securityContext`.
- Decidir si el VPN pack será gestionado por `litek8s` o si primero se soportará un mesh externo con autodetección.
- Decidir si `Secret` y `ConfigMap` salen juntos o por fases.

### Notas para futuras sesiones

- Actualizar este archivo primero cuando cambie la prioridad.
- Añadir rutas de archivos tocados para cada spec cuando haya implementación.
- Mantener una sola línea de “próximo paso” por spec para facilitar retomar contexto rápido.
