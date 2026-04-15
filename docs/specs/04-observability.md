# Spec 04: Observability

- Prioridad: P0
- Estado: Draft
- Depende de: nada estrictamente, pero se beneficia de `01-security-baseline` y `03-vpn-pack`

## Contexto

`litek8s` ya tiene buena observabilidad humana para una demo: `top`, `events`, `tree`, `logs` y descripciones. Lo que falta es observabilidad consumible por tooling: métricas scrapeables, mejor retención, correlación entre nodos y estados más fáciles de alertar.

## Problema

Sin observabilidad exportable:

- no se puede integrar fácil con Prometheus/Grafana;
- no hay base para alertas simples;
- la retención de eventos es muy corta para troubleshooting real;
- el estado de nodo, control plane, servicios y seguridad no sale como señales operativas estables.

## Objetivos

- Exponer métricas Prometheus sencillas y útiles.
- Aumentar la utilidad operativa de eventos y estado.
- Mantener `top/tree/events` como UX humana, no reemplazarlos.
- Diseñar una base simple de alertas y health.

## No objetivos

- No implementar un stack completo de logging centralizado en v1.
- No introducir OpenTelemetry completo desde el primer paso.
- No hacer dashboards obligatorios dentro del producto.

## Métricas mínimas propuestas

### Control plane

- `litek8s_control_plane_is_leader`
- `litek8s_control_plane_members`
- `litek8s_reconcile_duration_seconds`
- `litek8s_reconcile_errors_total`
- `litek8s_events_total`

### Deployments

- `litek8s_deployment_ready`
- `litek8s_deployment_restarts_total`
- `litek8s_deployment_next_retry_seconds`
- `litek8s_deployment_cpu_percent`
- `litek8s_deployment_memory_usage_bytes`

### Nodes

- `litek8s_node_ready`
- `litek8s_node_unschedulable`
- `litek8s_node_deployments`
- `litek8s_node_remote_query_failures_total`

### Services

- `litek8s_service_status`
- `litek8s_service_backend_ready`
- `litek8s_service_forward_errors_total`

## UX propuesta

### CLI / daemon

Agregar un listener HTTP de métricas:

```bash
litek8s serve --metrics-addr 127.0.0.1:9464
```

Opcionalmente, en una fase posterior:

```bash
litek8s metrics
litek8s metrics --format prometheus
```

## Cambios técnicos esperados

Archivos probables:

- `src/daemon.rs`
- `src/protocol.rs`
- `src/store.rs`
- `src/service.rs`
- `src/reconciler.rs`

Cambios:

- recolección de counters/gauges
- listener HTTP simple de solo lectura
- aumento de retención de eventos o export de eventos recientes

## Logging y eventos

### Fase 1

- retención configurable de eventos
- reason codes estables
- timestamps más útiles para scraping/export

### Fase 2

- logs estructurados del daemon
- etiquetas de nodo/deployment en errores importantes

### Fase 3

- export de auditoría para cambios de seguridad, secrets y join de nodos

## Riesgos

- Exponer métricas en TCP sin auth puede abrir una superficie innecesaria si no se bindea bien.
- Duplicar lógica entre `top` y `/metrics` puede introducir inconsistencias.
- Recolectar demasiado puede castigar nodos modestos.

## Decisiones abiertas

- ¿El endpoint de métricas debe ir siempre por HTTP o puede salir por Unix socket también?
- ¿La retención de eventos se queda en JSON local o se separa en un journal más durable?
- ¿Conviene sumar tracing luego o mantenerlo fuera del core?

## Definition of Done

- Existe un endpoint Prometheus scrapeable.
- Se exportan métricas mínimas de control plane, deployment, node y service.
- La retención de eventos deja de ser un cuello de botella para troubleshooting básico.
- La documentación muestra una integración mínima con Prometheus/Grafana.

## Próximo paso recomendado

Definir el conjunto mínimo de métricas y el listener `--metrics-addr`.
