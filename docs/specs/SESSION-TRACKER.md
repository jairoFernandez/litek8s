# Session Tracker

## Estado global

- Proyecto: `litek8s`
- Objetivo de esta línea de trabajo: orquestar varias máquinas virtuales o nodos modestos con pocos recursos, sumando seguridad, conectividad privada, observabilidad, aislamiento y scheduling orientado a capacidad/costo.
- Nota: la malla de inferencia IA queda documentada para más adelante; no forma parte del foco inmediato.
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
| `docs/specs/08-ai-inference-mesh.md` | P3 | Deferred | Reabrir después de multi-VM liviano, scheduler por capacidad y conectividad privada |
| `docs/specs/09-pod-certificates.md` | P1 | Implementado | `hostPath` type y `mountPropagation` en crun, luego instalar Substrate (`docs/substrate.md`) |

## Sesión actual recomendada

- Trabajar el camino de multi-VM liviano con `docs/specs/03-vpn-pack.md` y `docs/specs/06-scheduler-cost-aware.md`
- Entregable esperado: modelo mínimo de conectividad privada entre VMs, capacidad por nodo y scheduling por requests/costo

## Registro de decisiones

### Pendientes

- Decidir si el manifiesto usará un perfil propio (`securityProfile`) o un subset k8s-like de `securityContext`.
- Decidir si el VPN pack será gestionado por `litek8s` o si primero se soportará un mesh externo con autodetección.
- Decidir si `Secret` y `ConfigMap` salen juntos o por fases.
- Decidir el modelo mínimo de capacidad por nodo para varias VMs pequeñas.

### Notas para futuras sesiones

- Actualizar este archivo primero cuando cambie la prioridad.
- Añadir rutas de archivos tocados para cada spec cuando haya implementación.
- Mantener una sola línea de “próximo paso” por spec para facilitar retomar contexto rápido.
