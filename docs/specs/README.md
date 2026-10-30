# Specs: roadmap de infraestructura

## Propósito

Este directorio reúne los specs de trabajo para llevar `litek8s` desde un orquestador demo funcional hacia una plataforma más útil para clusters pequeños, nodos heterogéneos y hardware de costo contenido.

La idea es que cada sesión futura pueda arrancar desde aquí sin re-descubrir el contexto.

## Estado actual resumido

- `litek8s` ya resuelve despliegues simples sobre Docker, local o remoto por SSH.
- Tiene reconciler, backoff, readiness básica, `service`, `proxy`, `events`, `top`, `tree`, `cordon` y `drain`.
- El control plane ya usa Raft, pero todavía no cubre todos los requisitos de una operación de producción.
- El manifiesto sigue siendo deliberadamente estrecho: una réplica, un contenedor, sin overlay network, sin secrets, sin volumes, sin rollouts complejos.
- El foco inmediato es orquestar varias máquinas virtuales o nodos modestos con bajo consumo; la malla de inferencia IA queda documentada como extensión futura.

## Cómo usar estos specs

1. Abrir este archivo y `docs/specs/SESSION-TRACKER.md`.
2. Elegir un spec siguiendo la prioridad y dependencias.
3. Trabajar solo el alcance de la fase actual.
4. Al cerrar una sesión, actualizar:
   - `Estado`
   - `Decisiones tomadas`
   - `Siguiente paso recomendado`
5. Si una sesión produce cambios relevantes, enlazar commits o archivos tocados en el tracker.

## Orden recomendado

### P0 — base de seguridad y operación

1. `01-security-baseline.md`
2. `02-secrets-and-config.md`
3. `03-vpn-pack.md`
4. `04-observability.md`

### P1 — aislamiento y mejor aprovechamiento de nodos

5. `05-sandbox-isolation.md`
6. `06-scheduler-cost-aware.md`

### P2 — red interna más completa

7. `07-vpc-lite.md`

### P1 — cargas de agentes (Agent Substrate)

9. `09-pod-certificates.md` — PodCertificateRequest, ClusterTrustBundle y sus proyecciones en pods.

### P3 — extensiones futuras

8. `08-ai-inference-mesh.md` — diferido hasta tener multi-VM liviano, scheduling por capacidad y conectividad privada.

## Dependencias entre specs

- `02-secrets-and-config.md` depende parcialmente de `01-security-baseline.md`.
- `03-vpn-pack.md` mejora y simplifica varios caminos de `control-plane join`, `service` y health checks.
- `04-observability.md` puede iniciarse en paralelo, pero conviene después de `01` para exponer métricas de seguridad y networking desde el comienzo.
- `05-sandbox-isolation.md` depende de `01-security-baseline.md` y se beneficia de `06-scheduler-cost-aware.md`.
- `07-vpc-lite.md` depende de `03-vpn-pack.md`.
- `08-ai-inference-mesh.md` depende de `03-vpn-pack`, `04-observability` y `06-scheduler-cost-aware`; por ahora no debe bloquear el trabajo de multi-VM liviano.

## Criterios de diseño globales

- Mantener el proyecto pequeño y entendible.
- Preferir defaults seguros antes que knobs infinitos.
- Diseñar para nodos baratos, ARM/x86 mezclados, edge y VPS modestos.
- Evitar exigir infraestructura enterprise para el camino feliz.
- Introducir complejidad en fases, sin intentar “hacer Kubernetes completo”.

## Convenciones sugeridas para futuras sesiones

- Si una capacidad requiere manifiesto nuevo, intentar mantener compatibilidad hacia atrás.
- Si una capacidad afecta seguridad, incluir una ruta `default-safe` y una ruta `override-explicit`.
- Si una capacidad necesita estado persistido, documentar migración del `state.json`.
- Si una capacidad agrega un listener nuevo, documentar bind address, auth y cifrado.
- Cada spec debería implementarse en vertical slices pequeñas, no en un gran merge.

## Próxima sesión recomendada

Arrancar por el camino de multi-VM liviano: `docs/specs/03-vpn-pack.md` y `docs/specs/06-scheduler-cost-aware.md`.

Razón:

- La prioridad práctica actual es coordinar varias VMs con pocos recursos.
- `03-vpn-pack` reduce dependencia de SSH y puertos públicos.
- `06-scheduler-cost-aware` permite decidir placement según capacidad real y costo.
- `01-security-baseline.md` sigue siendo P0 y debe entrar antes de exponer APIs o workloads menos confiables.

## Archivos de referencia

- `docs/specs/SESSION-TRACKER.md`
- `docs/specs/01-security-baseline.md`
- `docs/specs/02-secrets-and-config.md`
- `docs/specs/03-vpn-pack.md`
- `docs/specs/04-observability.md`
- `docs/specs/05-sandbox-isolation.md`
- `docs/specs/06-scheduler-cost-aware.md`
- `docs/specs/07-vpc-lite.md`
- `docs/specs/08-ai-inference-mesh.md`
- `docs/specs/09-pod-certificates.md`
