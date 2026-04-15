# Spec 03: VPN Pack

- Prioridad: P0
- Estado: Draft
- Depende de: opcionalmente `01-security-baseline`
- Bloquea parcialmente: `07-vpc-lite`

## Contexto

La conectividad actual entre nodos remotos depende de SSH y puertos publicados. Eso funciona para demos, pero hace más difícil tener una red privada coherente, una API segura por defecto y una experiencia sencilla de multi-máquina.

## Problema

Sin un `VPN pack`:

- el control plane y los workers se apoyan demasiado en conectividad pública o túneles ad hoc;
- health checks, `proxy` y `service` no tienen una preferencia clara por rutas privadas;
- una “VPC simple” se vuelve difícil porque ni siquiera existe una malla privada de base.

## Objetivos

- Dar a `litek8s` una capa de conectividad privada simple y segura entre nodos.
- Hacer que el control plane pueda anunciarse en IP privada del mesh.
- Permitir una UX mínima para operar nodos remotos sin abrir demasiados puertos públicos.
- Diseñar una base simple para futuras capacidades de service discovery interna.

## No objetivos

- No resolver overlay per-pod en esta fase.
- No reemplazar completamente SSH el primer día.
- No exigir BGP, VXLAN o componentes complejos de red.

## Enfoque recomendado

### Fase 1: mesh privado entre nodos

Dos caminos posibles:

- `WireGuard` gestionado por `litek8s`
- integración con `Tailscale/Headscale` como provider externo

Recomendación:

- diseñar provider abstraction desde el inicio;
- implementar primero un modo `external-mesh` con metadata y autodetección;
- luego agregar `wireguard-managed` como provider nativo si hace falta.

## UX propuesta

### CLI

```bash
litek8s vpn init --provider external-mesh
litek8s vpn status
litek8s vpn attach-node server77 --ssh server77
```

Fase posterior:

```bash
litek8s vpn init --provider wireguard --cidr 10.44.0.0/24
litek8s vpn attach-node server77 --ssh server77 --advertise 10.44.0.7
```

### Estado de nodo

Cada nodo puede ganar metadata nueva:

- `vpn.provider`
- `vpn.private_ip`
- `vpn.endpoint`
- `vpn.status`
- `vpn.lastHandshake`

## Comportamiento esperado

- `node add` puede registrar o descubrir IP privada del mesh.
- `control-plane join` debe preferir la IP privada cuando exista.
- `service` y `proxy` deben resolver backends privados antes que endpoints públicos.
- health checks de nodo deben preferir la red privada.

## Cambios técnicos esperados

Archivos probables:

- `src/node.rs`
- `src/daemon.rs`
- `src/protocol.rs`
- `src/store.rs`
- `src/cli.rs`

Cambios:

- extender metadata de nodos con networking privada
- agregar comandos `vpn`
- enseñar a `proxy/service/control-plane` a preferir la IP VPN

## Fases sugeridas

### Fase 1

- provider abstraction
- metadata de mesh por nodo
- preferencia de IP privada en rutas internas

### Fase 2

- integración operativa con provider específico
- chequeos de salud del mesh
- eventos y `describe node`

### Fase 3

- aprovisionamiento gestionado de claves y config
- rotación y re-join automático

## Seguridad

- Si se expone API TCP, debe poder bindear solo en IP privada del mesh.
- El `VPN pack` no reemplaza autenticación del API; reduce exposición, no elimina la necesidad de auth/mTLS.

## Riesgos

- Gestionar WireGuard cross-platform puede complicarse en macOS/Desktop.
- Integrarse con Tailscale/Headscale reduce complejidad inicial pero agrega dependencia externa.
- Puede haber conflicto entre IPs privadas del mesh y resolución existente por SSH/hostnames.

## Decisiones abiertas

- ¿Provider inicial: externo o WireGuard gestionado?
- ¿El control plane debe exigir mesh para join remoto o solo preferirlo?
- ¿La metadata de VPN vive dentro del recurso `Node` actual o en una estructura aparte?

## Definition of Done

- `litek8s` puede registrar una IP privada por nodo.
- `control-plane join`, health checks y forwarding interno prefieren esa IP.
- `describe node` muestra estado de mesh/VPN.
- Hay una ruta de operación documentada para cluster privado básico.

## Próximo paso recomendado

Elegir provider inicial y definir la estructura persistida de networking privada por nodo.
