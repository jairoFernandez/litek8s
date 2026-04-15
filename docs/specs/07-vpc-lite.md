# Spec 07: VPC-lite

- Prioridad: P2
- Estado: Draft
- Depende de: `03-vpn-pack`

## Contexto

Una de las fricciones habituales en este tipo de infraestructura es que resulta difícil tener una “VPC simple”. En realidad, una VPC útil requiere varias capas: conectividad privada, addressing, discovery y políticas. Por eso esta spec se apoya en el `VPN pack` como base.

## Problema

Aunque exista VPN entre nodos, todavía faltaría una red privada operativa para servicios:

- naming interno consistente;
- exposición interna sin `hostPort` público;
- reglas simples de acceso;
- experiencia de red privada para apps pequeñas.

## Objetivos

- Ofrecer una capa de red privada sencilla entre nodos y servicios.
- Mantener el alcance pequeño y compatible con el enfoque minimalista de `litek8s`.
- Dar un camino usable para startups o labs sin pedir overlay per-pod completo.

## No objetivos

- No implementar kube-proxy completo.
- No asignar IP por pod en la primera versión.
- No resolver ingress L7 completo ni service mesh.

## Alcance mínimo viable

### Componentes

- addressing privado a nivel nodo
- service discovery interno
- listeners de service sobre IP privada
- políticas simples de acceso entre grupos de nodos/servicios

### Resultado esperado

Un servicio puede exponerse como interno, anunciarse en DNS simple y resolverse por la red privada del cluster, aunque por debajo siga usando forwarding a nivel nodo.

## UX propuesta

### Service interno

```yaml
apiVersion: litek8s.dev/v1alpha1
kind: Service
metadata:
  name: payments
spec:
  selector:
    app: payments
  ports:
    - port: 8080
  exposure: internal
```

### Resolución prevista

- `payments.internal`
- `payments.service.internal`

El naming exacto puede decidirse después, pero debe ser corto y predecible.

## Diseño técnico

### Fase 1

- servicios internos resueltos a nivel nodo
- DNS mínimo o tabla de resolución simple
- exposición privada sin bind público por defecto

### Fase 2

- ACLs básicas por etiqueta o grupo
- egress policy mínima

### Fase 3

- endpoint selection multi-backend
- balanceo interno simple

## Integración con el estado actual

La arquitectura actual de `service` ya hace forwarding a un único deployment. `VPC-lite` debe reutilizar esa idea, pero moviéndola a una semántica privada del cluster y no solo a `127.0.0.1` del daemon local.

## Cambios técnicos esperados

Archivos probables:

- `src/service.rs`
- `src/daemon.rs`
- `src/protocol.rs`
- `src/store.rs`
- `src/node.rs`

Cambios:

- estado de exposición `internal/public`
- resolución por IP privada del nodo
- naming interno y metadata de endpoints

## Riesgos

- Es fácil que esta spec crezca demasiado y termine intentando reinventar un CNI completo.
- Si el DNS interno se diseña mal, la UX empeora en vez de simplificarse.
- La política interna de acceso puede mezclarse demasiado con `security baseline` y `vpn pack` si no se separan bien responsabilidades.

## Decisiones abiertas

- ¿El DNS interno será embebido, por archivo hosts, o por integración externa opcional?
- ¿La exposición `internal` reemplaza o convive con el modelo actual de listeners locales?
- ¿Las ACLs se expresan por `Service`, por `Node`, o por ambos?

## Definition of Done

- Un `Service` puede declararse como interno.
- El backend se resuelve y alcanza por la red privada del cluster.
- La semántica no requiere `hostPort` público para el camino feliz interno.
- Existe una historia clara y pequeña de naming interno.

## Próximo paso recomendado

Cerrar el alcance mínimo viable para evitar convertir esta spec en un rediseño completo de networking.
