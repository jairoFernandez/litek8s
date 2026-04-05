# Plan: Control Plane HA con OpenRaft

## Context

litek8s tiene un daemon single-node que persiste estado como JSON en disco. El módulo `control_plane/mod.rs` ya define los tipos base (`ClusterCommand`, `CommandResult`, `ControlPlanePeer`, peer validation) y declara 4 submódulos que aún no existen: `config`, `network`, `raft`, `storage`. La dependencia `openraft = "=0.9.21"` ya está en Cargo.toml. El objetivo es implementar estos 4 módulos para que el estado del cluster se replique via Raft entre múltiples instancias del daemon.

## Approach

Implementar los 4 submódulos del control plane usando openraft 0.9.21. El estado replicado es el `StateFile` (deployments, services, nodes, events). Las mutaciones pasan por el log de Raft como `ClusterCommand` y se aplican en la state machine.

### Módulo 1: `config.rs` — Configuración de tipos Raft

- Usar `declare_raft_types!` para definir `TypeConfig`
- `D` = `ClusterCommand` (ya definido en mod.rs)
- `R` = `CommandResult` (ya definido en mod.rs)
- `NodeId` = `u64`
- `Node` = `BasicNode`
- `Entry` = `openraft::Entry<TypeConfig>`
- `SnapshotData` = `Cursor<Vec<u8>>`
- `AsyncRuntime` = `TokioRuntime`

**Archivo:** `src/control_plane/config.rs`

### Módulo 2: `storage.rs` — Log storage + State machine

Dos structs principales:

**`LogStore`** — implementa `RaftLogStorage<TypeConfig>`:
- Almacena log entries en memoria (`BTreeMap<u64, Entry>`)
- Persiste `Vote` en memoria (suficiente para v1, disco después)
- Implementa `RaftLogReader` para replicación

**`StateMachineStore`** — implementa `RaftStateMachine<TypeConfig>`:
- Wrappea el `Store` existente de litek8s
- `apply()` recibe `ClusterCommand` entries y las traduce a mutaciones del Store:
  - `ApplyDeployment` → `store.upsert()`
  - `DeleteDeployment` → `store.remove()`
  - `ApplyService` → `store.upsert_service()`
  - `DeleteService` → `store.remove_service()`
  - `AddWorkerNode` → `store.upsert_node()`
  - `SetWorkerNodeScheduling` → `store.set_node_unschedulable()`
  - `SetDeploymentStatus` → `store.set_status()`
  - `SetDeploymentAssignment` → `store.set_assigned_node()`
  - `AppendEvent` → `store.record_event()`
- Snapshots: serializa `StateFile` completo a JSON bytes
- `install_snapshot()`: deserializa JSON y reemplaza estado del Store

**Archivo:** `src/control_plane/storage.rs`

### Módulo 3: `network.rs` — Comunicación inter-nodo

**`NetworkFactory`** — implementa `RaftNetworkFactory<TypeConfig>`:
- `new_client()` crea un `NetworkClient` para cada peer

**`NetworkClient`** — implementa `RaftNetwork<TypeConfig>`:
- Usa TCP con JSON serialization (mismo patrón que el protocolo existente)
- `append_entries()`, `vote()`, `install_snapshot()` → serialize request, send over TCP, deserialize response
- Cada peer escucha en su `advertise_peer_addr` (del ControlPlanePeer)

**Archivo:** `src/control_plane/network.rs`

### Módulo 4: `raft.rs` — Inicialización y API del nodo Raft

**`ControlPlaneNode`** struct:
- `raft: openraft::Raft<TypeConfig>` — instancia del motor
- `store: Arc<Store>` — referencia al store de litek8s

**Funciones:**
- `start_raft_node()` — crea Config, LogStore, StateMachineStore, NetworkFactory, instancia Raft
- `propose_command(cmd: ClusterCommand)` — wrapper sobre `raft.client_write()` para proponer mutaciones
- `handle_raft_rpc()` — dispatcher para RPCs entrantes de otros nodos (append_entries, vote, install_snapshot)
- `spawn_raft_listener()` — TCP listener para RPCs de peers

**Archivo:** `src/control_plane/raft.rs`

## Files to create/modify

| File | Action |
|------|--------|
| `src/control_plane/config.rs` | Create — type config |
| `src/control_plane/storage.rs` | Create — log store + state machine |
| `src/control_plane/network.rs` | Create — peer networking |
| `src/control_plane/raft.rs` | Create — node init + API |

## Key reuse points

- `ClusterCommand` y `CommandResult` ya están en `control_plane/mod.rs`
- `Store` en `src/store.rs` — todas las mutaciones ya existen como métodos
- `ControlPlanePeer`, `parse_peer_spec()`, `validate_peer_map()` ya en mod.rs
- Patrón de serialización JSON sobre TCP ya existe en `src/protocol.rs` (`read_message`/`send_message`)

## Verification

1. `cargo build` — debe compilar sin errores (los 4 módulos declarados en mod.rs ahora existen)
2. `cargo test` — los tests de integración existentes deben seguir pasando (el modo HA no se activa sin flags)
3. Verificar que los traits de openraft están correctamente implementados revisando que no hay warnings de unused
