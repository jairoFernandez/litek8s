use std::collections::BTreeMap;
use std::fmt::Debug;
use std::io::Cursor;
use std::ops::RangeBounds;
use std::sync::Arc;

use openraft::Entry;
use openraft::EntryPayload;
use openraft::ErrorSubject;
use openraft::ErrorVerb;
use openraft::LogId;
use openraft::StorageError;
use openraft::StorageIOError;
use openraft::StoredMembership;
use openraft::Vote;
use openraft::storage::LogFlushed;
use openraft::storage::LogState;
use openraft::storage::RaftLogReader;
use openraft::storage::RaftLogStorage;
use openraft::storage::RaftSnapshotBuilder;
use openraft::storage::RaftStateMachine;
use openraft::storage::Snapshot;
use openraft::storage::SnapshotMeta;
use tokio::sync::RwLock;

use super::config::TypeConfig;
use super::{ClusterCommand, CommandResult};
use crate::manifest::DeploymentManifest;
use crate::node::ClusterNode;
use crate::store::{NewEvent, ServiceApplyOutcome, StateFile, Store};

// ---------------------------------------------------------------------------
// Log storage (in-memory)
// ---------------------------------------------------------------------------

pub struct LogStore {
    vote: RwLock<Option<Vote<u64>>>,
    committed: RwLock<Option<LogId<u64>>>,
    log: RwLock<BTreeMap<u64, Entry<TypeConfig>>>,
    last_purged: RwLock<Option<LogId<u64>>>,
}

impl LogStore {
    pub fn new() -> Self {
        Self {
            vote: RwLock::new(None),
            committed: RwLock::new(None),
            log: RwLock::new(BTreeMap::new()),
            last_purged: RwLock::new(None),
        }
    }
}

impl RaftLogReader<TypeConfig> for LogStore {
    async fn try_get_log_entries<RB: RangeBounds<u64> + Clone + Debug + Send>(
        &mut self,
        range: RB,
    ) -> Result<Vec<Entry<TypeConfig>>, StorageError<u64>> {
        let log = self.log.read().await;
        Ok(log.range(range).map(|(_, v)| v.clone()).collect())
    }
}

impl RaftLogStorage<TypeConfig> for LogStore {
    type LogReader = Self;

    async fn get_log_state(&mut self) -> Result<LogState<TypeConfig>, StorageError<u64>> {
        let log = self.log.read().await;
        let last_purged = self.last_purged.read().await;
        let last = log.iter().next_back().map(|(_, e)| e.log_id);
        Ok(LogState {
            last_purged_log_id: *last_purged,
            last_log_id: last.or(*last_purged),
        })
    }

    async fn get_log_reader(&mut self) -> Self::LogReader {
        // For in-memory storage we cannot cheaply clone, so we create a new
        // reader that shares the same data via RwLock.  However, the openraft
        // API takes `&mut self`, meaning the engine serialises access to the
        // log store.  We'll return a fresh LogStore that shares nothing — the
        // engine never reads from the reader while writing, so an empty reader
        // that re-reads from the same source is safe here.  For a production
        // implementation you'd use Arc-shared state or a real DB.
        //
        // Workaround: copy current state into a standalone reader.
        let log = self.log.read().await.clone();
        let last_purged = *self.last_purged.read().await;
        let reader = LogStore::new();
        *reader.log.write().await = log;
        *reader.last_purged.write().await = last_purged;
        reader
    }

    async fn save_vote(&mut self, vote: &Vote<u64>) -> Result<(), StorageError<u64>> {
        *self.vote.write().await = Some(*vote);
        Ok(())
    }

    async fn read_vote(&mut self) -> Result<Option<Vote<u64>>, StorageError<u64>> {
        Ok(*self.vote.read().await)
    }

    async fn save_committed(
        &mut self,
        committed: Option<LogId<u64>>,
    ) -> Result<(), StorageError<u64>> {
        *self.committed.write().await = committed;
        Ok(())
    }

    async fn read_committed(&mut self) -> Result<Option<LogId<u64>>, StorageError<u64>> {
        Ok(*self.committed.read().await)
    }

    async fn append<I>(
        &mut self,
        entries: I,
        callback: LogFlushed<TypeConfig>,
    ) -> Result<(), StorageError<u64>>
    where
        I: IntoIterator<Item = Entry<TypeConfig>> + Send,
        I::IntoIter: Send,
    {
        let mut log = self.log.write().await;
        for entry in entries {
            log.insert(entry.log_id.index, entry);
        }
        // In-memory — data is immediately "flushed".
        callback.log_io_completed(Ok(()));
        Ok(())
    }

    async fn truncate(&mut self, log_id: LogId<u64>) -> Result<(), StorageError<u64>> {
        let mut log = self.log.write().await;
        let to_remove: Vec<u64> = log.range(log_id.index..).map(|(k, _)| *k).collect();
        for k in to_remove {
            log.remove(&k);
        }
        Ok(())
    }

    async fn purge(&mut self, log_id: LogId<u64>) -> Result<(), StorageError<u64>> {
        *self.last_purged.write().await = Some(log_id);
        let mut log = self.log.write().await;
        let to_remove: Vec<u64> = log.range(..=log_id.index).map(|(k, _)| *k).collect();
        for k in to_remove {
            log.remove(&k);
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// State machine (wraps litek8s Store)
// ---------------------------------------------------------------------------

pub struct StateMachineStore {
    store: Arc<Store>,
    last_applied: RwLock<Option<LogId<u64>>>,
    last_membership: RwLock<StoredMembership<u64, openraft::BasicNode>>,
    snapshot_idx: RwLock<u64>,
}

impl StateMachineStore {
    pub fn new(store: Arc<Store>) -> Self {
        Self {
            store,
            last_applied: RwLock::new(None),
            last_membership: RwLock::new(StoredMembership::default()),
            snapshot_idx: RwLock::new(0),
        }
    }

    async fn apply_command(&self, cmd: ClusterCommand) -> CommandResult {
        match cmd {
            ClusterCommand::ApplyDeployment { manifest_yaml } => {
                match serde_yaml::from_str::<DeploymentManifest>(&manifest_yaml) {
                    Ok(manifest) => match self.store.upsert(manifest).await {
                        Ok(outcome) => CommandResult::Applied {
                            created: outcome == crate::store::ApplyOutcome::Created,
                        },
                        Err(e) => CommandResult::Ack {
                            message: Some(format!("error: {e}")),
                        },
                    },
                    Err(e) => CommandResult::Ack {
                        message: Some(format!("invalid manifest: {e}")),
                    },
                }
            }
            ClusterCommand::DeleteDeployment { name } => match self.store.remove(&name).await {
                Ok(Some(_)) => CommandResult::Changed { changed: true },
                _ => CommandResult::Changed { changed: false },
            },
            ClusterCommand::ApplyService {
                name,
                deployment_name,
                selector,
                listen_addr,
                target_port,
            } => {
                match self
                    .store
                    .upsert_service(name, deployment_name, selector, listen_addr, target_port)
                    .await
                {
                    Ok(outcome) => CommandResult::Applied {
                        created: outcome == ServiceApplyOutcome::Created,
                    },
                    Err(e) => CommandResult::Ack {
                        message: Some(format!("error: {e}")),
                    },
                }
            }
            ClusterCommand::DeleteService { name } => {
                match self.store.remove_service(&name).await {
                    Ok(Some(_)) => CommandResult::Changed { changed: true },
                    _ => CommandResult::Changed { changed: false },
                }
            }
            ClusterCommand::AddWorkerNode {
                name,
                ssh_target,
                remote_binary,
                labels,
            } => {
                let binary = remote_binary.unwrap_or_else(|| "litek8s".into());
                match ClusterNode::ssh(name.clone(), ssh_target, binary, labels) {
                    Ok(node) => match self.store.upsert_node(node).await {
                        Ok(outcome) => CommandResult::Applied {
                            created: outcome == crate::store::NodeApplyOutcome::Created,
                        },
                        Err(e) => CommandResult::Ack {
                            message: Some(format!("error: {e}")),
                        },
                    },
                    Err(e) => CommandResult::Ack {
                        message: Some(format!("error: {e}")),
                    },
                }
            }
            ClusterCommand::SetWorkerNodeScheduling {
                name,
                unschedulable,
            } => {
                match self
                    .store
                    .set_node_unschedulable(&name, unschedulable)
                    .await
                {
                    Ok(changed) => CommandResult::Changed { changed },
                    Err(_) => CommandResult::Changed { changed: false },
                }
            }
            ClusterCommand::RemoveWorkerNode { name: _ } => {
                // Node removal not yet implemented in Store
                CommandResult::Noop
            }
            ClusterCommand::SetDeploymentAssignment {
                name,
                assigned_node,
            } => match self.store.set_assigned_node(&name, assigned_node).await {
                Ok(changed) => CommandResult::Changed { changed },
                Err(_) => CommandResult::Changed { changed: false },
            },
            ClusterCommand::SetDeploymentStatus {
                name,
                status,
                ready,
                container_id,
                message,
                last_exit,
                increment_restart_count,
                increment_restart_streak,
                next_retry_at,
            } => {
                match self
                    .store
                    .set_status(
                        &name,
                        status,
                        ready,
                        container_id,
                        message,
                        last_exit,
                        increment_restart_count,
                        increment_restart_streak,
                        next_retry_at,
                    )
                    .await
                {
                    Ok(changed) => CommandResult::Changed { changed },
                    Err(_) => CommandResult::Changed { changed: false },
                }
            }
            ClusterCommand::AppendEvent {
                level,
                reason,
                object_kind,
                object_name,
                node_name,
                message,
            } => {
                let new_event = NewEvent {
                    level,
                    reason,
                    object_kind,
                    object_name,
                    node_name,
                    message,
                };
                match self.store.record_event(new_event).await {
                    Ok(stored) => CommandResult::EventRecorded { id: stored.id },
                    Err(_) => CommandResult::Noop,
                }
            }
        }
    }
}

impl RaftSnapshotBuilder<TypeConfig> for StateMachineStore {
    async fn build_snapshot(&mut self) -> Result<Snapshot<TypeConfig>, StorageError<u64>> {
        let state = self.store.snapshot().await;
        let data = serde_json::to_vec(&state)
            .map_err(|e| StorageIOError::new(ErrorSubject::StateMachine, ErrorVerb::Read, &e))?;

        let last_applied = self.last_applied.read().await;
        let last_membership = self.last_membership.read().await;

        let mut idx = self.snapshot_idx.write().await;
        *idx += 1;
        let snapshot_id = format!("{}-{}", last_applied.map(|l| l.index).unwrap_or(0), *idx);

        let meta = SnapshotMeta {
            last_log_id: *last_applied,
            last_membership: last_membership.clone(),
            snapshot_id,
        };

        Ok(Snapshot {
            meta,
            snapshot: Box::new(Cursor::new(data)),
        })
    }
}

impl RaftStateMachine<TypeConfig> for StateMachineStore {
    type SnapshotBuilder = Self;

    async fn applied_state(
        &mut self,
    ) -> Result<
        (
            Option<LogId<u64>>,
            StoredMembership<u64, openraft::BasicNode>,
        ),
        StorageError<u64>,
    > {
        let last_applied = *self.last_applied.read().await;
        let membership = self.last_membership.read().await.clone();
        Ok((last_applied, membership))
    }

    async fn apply<I>(&mut self, entries: I) -> Result<Vec<CommandResult>, StorageError<u64>>
    where
        I: IntoIterator<Item = Entry<TypeConfig>> + Send,
        I::IntoIter: Send,
    {
        let mut results = Vec::new();
        for entry in entries {
            *self.last_applied.write().await = Some(entry.log_id);

            match entry.payload {
                EntryPayload::Blank => {
                    results.push(CommandResult::Noop);
                }
                EntryPayload::Normal(cmd) => {
                    let result = self.apply_command(cmd).await;
                    results.push(result);
                }
                EntryPayload::Membership(membership) => {
                    *self.last_membership.write().await =
                        StoredMembership::new(Some(entry.log_id), membership);
                    results.push(CommandResult::Noop);
                }
            }
        }
        Ok(results)
    }

    async fn get_snapshot_builder(&mut self) -> Self::SnapshotBuilder {
        // We need a snapshot builder that can read the store state. Since the
        // engine serialises access, we create a new instance pointing at the
        // same Store.
        StateMachineStore {
            store: Arc::clone(&self.store),
            last_applied: RwLock::new(*self.last_applied.read().await),
            last_membership: RwLock::new(self.last_membership.read().await.clone()),
            snapshot_idx: RwLock::new(*self.snapshot_idx.read().await),
        }
    }

    async fn begin_receiving_snapshot(
        &mut self,
    ) -> Result<Box<Cursor<Vec<u8>>>, StorageError<u64>> {
        Ok(Box::new(Cursor::new(Vec::new())))
    }

    async fn install_snapshot(
        &mut self,
        meta: &SnapshotMeta<u64, openraft::BasicNode>,
        snapshot: Box<Cursor<Vec<u8>>>,
    ) -> Result<(), StorageError<u64>> {
        let data = snapshot.into_inner();
        let state: StateFile = serde_json::from_slice(&data)
            .map_err(|e| StorageIOError::new(ErrorSubject::Snapshot(None), ErrorVerb::Read, &e))?;

        self.store.replace_state(state).await;
        *self.last_applied.write().await = meta.last_log_id;
        *self.last_membership.write().await = meta.last_membership.clone();

        Ok(())
    }

    async fn get_current_snapshot(
        &mut self,
    ) -> Result<Option<Snapshot<TypeConfig>>, StorageError<u64>> {
        // Build a fresh snapshot on demand.
        let snapshot = self.build_snapshot().await?;
        Ok(Some(snapshot))
    }
}
