use crate::*;
use agentfs_model::*;
use agentfs_ports::*;
use std::{collections::BTreeMap, sync::Arc};

pub struct Adapters {
    pub state: Arc<dyn LocalStateStore>,
    pub objects: Arc<dyn LocalObjectStore>,
    pub working: Arc<dyn WorkingTreeStore>,
    pub remote_objects: Option<Arc<dyn RemoteObjectStore>>,
    pub remote_refs: Option<Arc<dyn RemoteRefStore>>,
    pub clock: Arc<dyn Clock>,
    pub mounts: Arc<dyn MountDriver>,
    pub directories: Arc<dyn DirectoryAccess>,
    pub validation: Arc<dyn ValidationRunner>,
}

impl std::fmt::Debug for Adapters {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Adapters")
            .field("mounts", &self.mounts.capability())
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Debug)]
pub struct EngineConfig {
    pub cache: CacheConfig,
    pub identity: LocalIdentity,
    pub validation: BTreeMap<String, ValidationConfig>,
}

#[derive(Debug)]
pub struct Engine {
    pub services: Services,
    pub runtime: Arc<Runtime>,
    revisions: Arc<RevisionService>,
    publisher: Arc<ReplicaService>,
    remote_configured: bool,
}

impl Engine {
    pub async fn open(adapters: Adapters, config: EngineConfig) -> Result<Arc<Self>> {
        if adapters.remote_objects.is_some() != adapters.remote_refs.is_some() {
            return Err(Error::invalid(
                "remote object and reference stores must be configured together",
            ));
        }
        let remote_configured = adapters.remote_objects.is_some();
        let runtime = Runtime::new(adapters.state, adapters.clock).await?;
        let operations = OperationService::new(runtime.clone());
        let content = ContentService::new(
            adapters.objects,
            adapters.remote_objects,
            adapters.remote_refs,
        );
        let trees = TreeService::new(runtime.clone(), content.clone());
        let snapshots =
            SnapshotService::new(runtime.clone(), trees.clone(), adapters.working.clone());
        snapshots.recover().await?;
        let publisher =
            ReplicaService::new(runtime.clone(), trees.clone(), content, operations.clone());
        let revisions = RevisionService::new(
            runtime.clone(),
            trees.clone(),
            operations.clone(),
            publisher.clone(),
            snapshots,
        );
        let files = FileService::new(
            runtime.clone(),
            trees.clone(),
            adapters.working,
            revisions.clone(),
        );
        let mount_capability = adapters.mounts.capability().to_owned();
        let mounts = MountService::new(
            runtime.clone(),
            revisions.clone(),
            operations.clone(),
            files.clone(),
            adapters.mounts,
            config.identity,
        );
        let versions = VersionService::new(revisions.clone(), mounts.clone());
        let sessions = SessionService::new(
            runtime.clone(),
            revisions.clone(),
            operations.clone(),
            mounts.clone(),
        );
        let cache = CacheService::new(runtime.clone(), trees, operations.clone(), config.cache)?;
        let merge = MergeService::new(
            runtime.clone(),
            revisions.clone(),
            operations.clone(),
            mounts.clone(),
            adapters.validation,
            config.validation,
        );
        let workspace = WorkspaceService::new(
            runtime.clone(),
            revisions.clone(),
            operations.clone(),
            adapters.directories,
            mount_capability,
        );
        let ownership = OwnershipService::new(
            runtime.clone(),
            revisions.clone(),
            publisher.clone(),
            operations.clone(),
        );
        let services = Services {
            workspace,
            revisions: versions,
            sessions,
            replica: publisher.clone(),
            cache,
            ownership,
            merge,
            mounts,
            operations,
            files,
        };
        Ok(Arc::new(Self {
            services,
            runtime,
            revisions,
            publisher,
            remote_configured,
        }))
    }

    /// Run independently of uploads so network delays cannot delay local autosaves.
    pub async fn autosave_due(&self) -> Result<usize> {
        let now = self.runtime.clock.monotonic_ms();
        let due: Vec<_> = self
            .runtime
            .dirty
            .lock()
            .iter()
            .filter(|(_, time)| {
                now.saturating_sub(time.last) >= 5_000 || now.saturating_sub(time.first) >= 30_000
            })
            .map(|(id, _)| *id)
            .collect();
        let mut saved = 0;
        for id in due {
            let branch = self.runtime.branch(id).await?;
            if branch.owner == self.runtime.location
                && branch.state == BranchState::Writable
                && !branch.protected
            {
                let workspace = self
                    .runtime
                    .state
                    .workspace(branch.workspace)
                    .await?
                    .ok_or_else(|| Error::integrity("workspace is missing"))?;
                self.revisions
                    .flush(id, workspace.owner, Durability::Local)
                    .await?;
                saved += 1;
            }
        }
        Ok(saved)
    }

    pub async fn sync_pending(&self) -> Result<()> {
        if !self.remote_configured {
            return Ok(());
        }
        let mut first_error = None;
        for workspace in self.runtime.state.workspaces().await? {
            if !self.runtime.state.sync_jobs(workspace.id).await?.is_empty()
                && let Err(error) = self.publisher.drain(workspace.id, None).await
            {
                first_error.get_or_insert(error);
            }
        }
        first_error.map_or(Ok(()), Err)
    }

    pub async fn flush_local(&self) -> Result<()> {
        for workspace in self.runtime.state.workspaces().await? {
            for branch in self.runtime.state.branches(workspace.id).await? {
                if branch.owner == self.runtime.location
                    && branch.state == BranchState::Writable
                    && branch.mutation_seq != branch.saved_seq
                {
                    self.revisions
                        .flush(branch.id, workspace.owner.clone(), Durability::Local)
                        .await?;
                }
            }
        }
        Ok(())
    }
}
