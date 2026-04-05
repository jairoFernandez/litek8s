# Plan: Raft siempre activo — serve simple + join dinámico

## Context

Eliminar `--ha` y toda la configuración manual. `litek8s serve` siempre arranca con Raft single-node. Agregar peers es dinámico via `control-plane join`.

## UX final

```bash
litek8s serve                                                      # arranca con Raft, single-node leader
litek8s control-plane join cp2 --ssh dev@192.168.0.77 --install    # agrega control plane
litek8s control-plane status                                       # verifica cluster
litek8s node add server77 --ssh server77 --install                 # agrega worker node
```

## Cambios

### 1. `serve` siempre con Raft (`src/cli.rs` + `src/daemon.rs`)

- Eliminar `--ha`, `--node-id`, `--peer`, `--peers` flags
- Mantener `--peer-addr` (default `0.0.0.0:4001`) y `--peer-id` (default `1`) como overrides opcionales
- `HaConfig` ya no es `Option` — siempre presente
- `serve()` siempre crea `ControlPlaneNode`, llama `initialize_cluster()` con solo el nodo propio
- Agregar `--join <LEADER_ADDR>` flag oculto/interno — usado por `control-plane join` cuando arranca el remote. Cuando presente, NO inicializa cluster (espera ser agregado)

### 2. `JoinCluster` request (`src/protocol.rs`)

```rust
Request::JoinCluster { node_id: u64, addr: String }
```

### 3. `control-plane join` automático (`src/cli.rs`)

Flow:
1. `--install` → `node::install_remote_agent()` (cross-compile + SCP)
2. `GetClusterStatus` al daemon local → obtiene max node_id, calcula next_id
3. Detecta IP del remote via SSH (`hostname -I | awk '{print $1}'`)
4. Arranca daemon remoto: `ssh target "litek8s serve --peer-id <N> --peer-addr 0.0.0.0:<port> --join <local_ip>:<local_port> &"`
5. Espera que el remote esté listening (TCP probe)
6. Envía `JoinCluster { node_id: N, addr: remote_ip:port }` al daemon local
7. Leader ejecuta `raft.change_membership()` → remote se une

### 4. `add_member()` en raft.rs (`src/control_plane/raft.rs`)

```rust
pub async fn add_member(&self, node_id: u64, addr: String) -> Result<()>
```

### 5. Handler JoinCluster en daemon (`src/daemon.rs`)

Llama `ha_node.add_member()`.

### 6. Limpiar `control_plane/mod.rs`

Eliminar `validate_peer_map` y `parse_peer_spec` — ya no se necesitan.

## Archivos

| Archivo | Cambio |
|---------|--------|
| `src/cli.rs` | Simplificar Serve flags, join automático |
| `src/daemon.rs` | Raft siempre activo, handler JoinCluster |
| `src/protocol.rs` | `Request::JoinCluster` |
| `src/control_plane/raft.rs` | `add_member()`, skip init con `--join` |
| `src/control_plane/mod.rs` | Limpiar validaciones obsoletas |
| `tests/integration.rs` | Actualizar ServerConfig (sin Option) |
