# Spec 03: VPN Pack

- Prioridad: P0
- Estado: Draft (concretizado)
- Depende de: opcionalmente `01-security-baseline`
- Bloquea: `07-vpc-lite`

## Contexto

La conectividad actual entre nodos remotos depende de SSH y puertos publicados. Para clusters cross-cloud (AWS + Civo + máquinas detrás de NAT) eso no escala: cada nodo vive en una red distinta, sin IP coherente, y obliga a exponer puertos en cada cloud por separado.

## Problema

Sin `VPN pack`:

- nodos en VPCs distintos no se ven entre sí (AWS EC2 ↔ Civo VM ↔ máquina en casa);
- una máquina detrás de NAT no puede recibir conexiones del cluster;
- `service` y `proxy` no tienen IP estable a la que apuntar;
- el API del daemon queda expuesto en `0.0.0.0` o forzado a SSH tunneling;
- `07-vpc-lite` no puede existir porque no hay red privada base.

## Objetivos

- Una red privada `10.42.0.0/24` cruzando clouds y NAT.
- Cada nodo recibe IP estable (`10.42.0.N`) usable por control plane, services y health checks.
- Bootstrap operable en <5 min para 3 nodos (1 hub + 2 spokes).
- API del daemon rebindea a IP VPN automático cuando mesh esté arriba.

## No objetivos

- No overlay per-pod.
- No reemplazo total de SSH (SSH sigue para `agent install` + bootstrap inicial).
- No full mesh peer-to-peer en MVP (queda para Fase 3).
- No soporte multi-hub / HA del hub en MVP.

## Decisiones cerradas

| # | Decisión | Razón |
|---|---|---|
| 1 | Provider: **WireGuard managed por litek8s** | cero dep externa, alineado con "single binary" |
| 2 | Backend WireGuard: **boringtun (userspace)** via crate `boringtun` | sin root ni módulo kernel; portable macOS/Linux; perf suficiente para demo |
| 3 | Topología: **hub-and-spoke** | spokes detrás NAT funcionan con keepalive saliente; full mesh requiere inbound reachable |
| 4 | CIDR: `10.42.0.0/24` | predecible, fuera de rangos AWS (`10.0.x.x`) y Civo defaults |
| 5 | Assignación de IP: hub `10.42.0.1`, spokes secuencial `10.42.0.2+` desde raft store | sin colisiones, determinístico |
| 6 | Keys: cada nodo genera privkey local; pubkey publicada via raft | privkey nunca cruza red |
| 7 | Bootstrap spoke: **SSH al hub con token** → hub registra pubkey en raft → reparte peer list | reusa transport SSH existente; un solo paso para el operador |
| 8 | API del daemon: **auto-rebind a `10.42.0.N` cuando VPN sube** | reduce exposición sin requerir config explícita |
| 9 | Tailscale/Headscale: **Fase 3 opcional** | evita dep externa en MVP |

## UX

### CLI

```bash
# Hub (típicamente EC2 con EIP estable)
litek8s vpn init --cidr 10.42.0.0/24 --public-endpoint 54.123.45.67:51820
# imprime token: litek8s-vpn-<base64>

# Spoke (Civo VM, o máquina local detrás NAT)
litek8s join --vpn-token litek8s-vpn-<base64> --ssh-hub ec2-user@54.123.45.67
# 1. SSH al hub con token → hub valida + asigna 10.42.0.2 + devuelve peer list
# 2. spoke genera privkey local, escribe wg config
# 3. boringtun levanta interfaz `lk-wg0`
# 4. keepalive saliente al hub abre NAT

litek8s vpn status
# muestra: peers, lastHandshake, latency
```

### Comando `vpn` (subcomandos)

```
litek8s vpn init        # convierte daemon en hub
litek8s vpn status      # peers + handshakes
litek8s vpn peers       # tabla detallada
litek8s vpn rotate-key  # regenera privkey local (Fase 2)
litek8s vpn down        # baja interfaz (debug)
```

### Metadata por nodo

`Node` resource gana campos:

```rust
pub struct VpnMembership {
    pub role: VpnRole,          // Hub | Spoke
    pub ip: IpAddr,             // 10.42.0.N
    pub pubkey: String,         // base64
    pub endpoint: Option<String>, // solo Hub: ip:port público
    pub last_handshake: Option<u64>, // unix ts
    pub status: VpnStatus,      // Up | Degraded | Down
}
```

## Diseño técnico

### Bootstrap secuencia (spoke join)

```
spoke                          hub (via SSH)              raft store
  |                                 |                          |
  |--- ssh <token> agent vpn-attach -->                        |
  |                                 |                          |
  |                                 |--- propose Peer{...} --->|
  |                                 |<-- committed ------------|
  |                                 |                          |
  |<-- VpnConfig{ip, peers, cidr} --|                          |
  |                                 |                          |
[genera privkey, escribe wg conf, boringtun up]                |
  |                                                            |
  |--- handshake WireGuard al hub (UDP 51820) ---------------->|
```

Token incluye: hub pubkey + endpoint + nonce firmado (corto-vivido, 10 min).

### Componentes en código

- **`src/vpn/mod.rs`** — orquestación: gen keys, escribir config, levantar boringtun, query status.
- **`src/vpn/config.rs`** — serialización de `wg0.conf`-like config, validación CIDR.
- **`src/vpn/tunnel.rs`** — wrapper sobre `boringtun::device`. Manejo del lifecycle del túnel.
- **`src/vpn/bootstrap.rs`** — flujo `vpn init` + `join --vpn-token`.
- **`src/node.rs`** — extiende `ClusterNode` con `vpn: Option<VpnMembership>`.
- **`src/cli.rs`** — subcomandos `vpn` + flag `--vpn-token` en `join`.
- **`src/daemon.rs`** — al subir, si nodo tiene `vpn` config, levanta interfaz; rebinde API.
- **`src/control_plane/raft.rs`** — comandos `RegisterVpnPeer`, `UpdateVpnStatus`.

### Persistencia

Peer table en raft store (no en archivo local plano), para que cualquier nodo pueda regenerar su `wg.conf` desde estado replicado:

```rust
ClusterCommand::RegisterVpnPeer {
    node_name: String,
    pubkey: String,
    vpn_ip: IpAddr,
    endpoint: Option<String>,
}
```

### Rebinde del API daemon

Hoy: `Unix socket` local + (futuro) TCP API. Cuando VPN está arriba:

- `Unix socket` sigue para CLI local.
- TCP API (cuando exista) bindea a `vpn.ip:port`, nunca `0.0.0.0`.
- Health checks entre nodos usan `10.42.0.N` antes que la IP cloud.

## Fases

### Fase 1 (MVP) — esta spec

- WireGuard via boringtun.
- Hub-and-spoke, bootstrap por SSH + token.
- Peer table en raft.
- `vpn init` / `vpn status` / `join --vpn-token`.
- Auto-rebind API a IP VPN.

### Fase 2

- `vpn rotate-key` con re-distribución.
- Reconexión automática post-restart.
- Métricas de mesh (handshake age, bytes tx/rx) expuestas a observabilidad.
- Eventos en `describe node` (Up/Degraded/Down).

### Fase 3

- Provider abstraction real → `tailscale` / `headscale` como backend alternativo.
- Multi-hub (full mesh entre hubs + spokes asignados al hub más cercano).
- NAT traversal directo entre spokes via STUN (cuando posible, fallback hub).

## Seguridad

- Privkey nunca sale del nodo. Generada con `boringtun::x25519`.
- Token de join firmado con la privkey del hub, expira en 10 min.
- WireGuard provee cifrado + autenticación de peers nativo.
- Si el hub se compromete, atacante puede unir nodos falsos → `vpn rotate-key` + revoke list (Fase 2).
- API daemon NO confía solo en estar en la VPN: spec `01-security-baseline` sigue exigiendo auth/mTLS.

## Riesgos

- **boringtun perf** menor que kernel WireGuard. Para >100 Mbps puede no bastar. Demo OK.
- **macOS** requiere `utun` device — boringtun soporta pero permisos pueden molestar.
- **UDP 51820 bloqueado** en algunas redes (cafeterías, corp). Sin fallback TCP en MVP → spoke no conecta. Documentar.
- **Hub SPOF**. Hub caído = todo el cluster pierde conectividad mesh. Aceptable para demo.
- **Token replay** si nonce no se invalida bien. Mitigar: hub mantiene set de nonces usados con TTL.

## Decisiones aún abiertas

- Puerto del hub: ¿fijo `51820` (default WireGuard) o configurable? → Recomiendo configurable, default `51820`.
- `lk-wg0` vs `wg0` para nombre interfaz → `lk-wg0` para no colisionar con WireGuard del usuario.
- Si SSH bootstrap falla (firewall corp), ¿alternativa manual con `vpn add-peer --pubkey ... --ip ...` en hub? → Sí, agregar como escape hatch.

## Definition of Done

- `litek8s vpn init` en EC2 imprime token.
- `litek8s join --vpn-token X --ssh-hub Y` en Civo y en máquina local detrás NAT levanta túnel.
- `ping 10.42.0.1` desde spoke al hub responde.
- `ping 10.42.0.2` entre spokes (a través del hub) responde.
- `litek8s vpn status` muestra los 3 peers con `lastHandshake` reciente.
- `litek8s get nodes` muestra columna `VPN-IP` con `10.42.0.N`.
- Daemon API rebinde automáticamente a `10.42.0.N` (verificable con `lsof -i` o `ss -tlnp`).

## Próximo paso recomendado

Crear scaffolding `src/vpn/` con stub de `VpnConfig`, `VpnMembership` en `Node`, y comando `vpn init` mínimo (genera keys + imprime token, sin túnel todavía). Iterar incremental: keys → config → boringtun up → bootstrap SSH → peer replication.
