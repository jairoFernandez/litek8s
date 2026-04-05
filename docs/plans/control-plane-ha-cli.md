# Plan: CLI HA — `serve --ha` flags + `control-plane join --install`

## Context

Los 4 módulos del control plane Raft (`config`, `storage`, `network`, `raft`) ya compilan. Ahora falta el CLI para usarlos: flags HA en `serve` y un comando `control-plane join` que reutilice el patrón de `node add --install` (cross-compile, SCP, arranque remoto).

## Cambios

### 1. Flags HA en `serve` (`src/cli.rs`)

Agregar a `Command::Serve`:
- `--ha` (bool) — activa modo HA
- `--node-id <u64>` — ID de este nodo Raft (requerido si `--ha`)
- `--peer-addr <addr>` — dirección TCP que este nodo anuncia para Raft RPCs (ej: `0.0.0.0:4001`)
- `--peer <ID=HOST:PORT>` (repetible) — mapa de peers del cluster (debe incluirse a sí mismo)

Cuando `--ha` está activo:
1. Validar con `validate_peer_map()` existente
2. Crear `ControlPlaneNode::start(node_id, peer_addr, store)`
3. Llamar `spawn_rpc_listener(peer_addr)` para recibir RPCs
4. Inicializar cluster Raft (initialize con membership si es nodo 1)
5. El daemon sigue funcionando igual pero el Store se replica

### 2. Nuevo subcomando `ControlPlane` (`src/cli.rs`)

```
litek8s control-plane join cp2 \
  --ssh dev@192.168.0.77 \
  --install \
  --peer-port 4001
```

`ControlPlaneCommand::Join`:
- `name` — nombre lógico del peer
- `--ssh <target>` — SSH target
- `--install` — cross-compile + SCP (reutiliza `node::install_remote_agent`)
- `--peer-port <u16>` — puerto para Raft RPCs (default 4001)
- `--remote-binary` — path del binario remoto (default `~/.local/bin/litek8s`)

Flujo:
1. Si `--install`: reutilizar `node::install_remote_agent()` para compilar y copiar
2. Arrancar daemon remoto via SSH: `ssh target "nohup litek8s serve --ha --node-id N --peer-addr 0.0.0.0:PORT --peer ... &"`
3. Esperar que el peer responda (TCP connect al peer-addr)
4. Agregar el peer al cluster Raft via `raft.change_membership()`

### 3. Actualizar `ServerConfig` (`src/daemon.rs`)

Agregar campos opcionales de HA:
```rust
pub struct ServerConfig {
    // ... existentes ...
    pub ha: Option<HaConfig>,
}

pub struct HaConfig {
    pub node_id: u64,
    pub peer_addr: String,
    pub peers: BTreeMap<String, String>,
}
```

En `serve()`: si `ha` está presente, crear `ControlPlaneNode` y spawnearlo junto al daemon normal.

### 4. Protocolo: nuevo `Request::GetClusterStatus` (`src/protocol.rs`)

Para poder verificar el estado del cluster HA desde CLI:
- `Request::GetClusterStatus`
- `Response::ClusterStatus { leader_id, members, ... }`

## Archivos a modificar

| Archivo | Cambio |
|---------|--------|
| `src/cli.rs` | Flags HA en Serve, nuevo subcomando ControlPlane |
| `src/daemon.rs` | `HaConfig` en ServerConfig, inicialización Raft en serve() |
| `src/protocol.rs` | Request/Response para cluster status |
| `src/control_plane/raft.rs` | Método `initialize_cluster()` para bootstrap |

## Verificación

1. `cargo build` — compila sin errores
2. `cargo test` — tests existentes pasan (sin --ha, el daemon se comporta igual)
3. Test manual: arrancar 2 nodos con `--ha`, verificar que eligen líder
4. Test manual con `control-plane join --install` al server 192.168.0.77
