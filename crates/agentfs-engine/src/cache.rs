use crate::{OperationService, Runtime, TreeService};
use agentfs_model::*;
use agentfs_ports::*;
use async_trait::async_trait;
use std::{collections::BTreeSet, sync::Arc};

#[derive(Debug)]
pub struct CacheService {
    runtime: Arc<Runtime>,
    trees: Arc<TreeService>,
    operations: Arc<OperationService>,
    config: CacheConfig,
}

impl CacheService {
    pub fn new(
        runtime: Arc<Runtime>,
        trees: Arc<TreeService>,
        operations: Arc<OperationService>,
        config: CacheConfig,
    ) -> Result<Arc<Self>> {
        config.validate()?;
        Ok(Arc::new(Self {
            runtime,
            trees,
            operations,
            config,
        }))
    }

    async fn selection(
        &self,
        range: &RevisionRange,
    ) -> Result<(Vec<WorkspacePath>, Vec<ObjectRef>)> {
        let revision = self.trees.revision(range.workspace, range.revision).await?;
        let tree = self.trees.tree(range.workspace, &revision.root).await?;
        let mut paths = range
            .paths
            .clone()
            .unwrap_or_else(|| vec![WorkspacePath::root()]);
        if paths.is_empty() {
            return Err(Error::invalid("revision paths must not be empty"));
        }
        for path in &mut paths {
            *path = tree
                .keys()
                .find(|entry| entry.lookup_key() == path.lookup_key())
                .cloned()
                .ok_or_else(|| Error::new(ErrorCode::NotFound, "revision path does not exist"))?;
        }
        paths.sort();
        paths.dedup();
        let mut objects: Vec<_> = self
            .trees
            .closure(range.workspace, vec![revision.root])
            .await?
            .into_iter()
            .filter(|object| object.kind != ObjectKind::File)
            .collect();
        objects.extend(
            tree.iter()
                .filter(|(path, _)| paths.iter().any(|selected| selected.contains(path)))
                .filter_map(|(_, inode)| inode.content.clone()),
        );
        objects.sort();
        objects.dedup();
        Ok((paths, objects))
    }

    async fn inspect(
        &self,
        range: &RevisionRange,
        paths: Vec<WorkspacePath>,
        objects: &[ObjectRef],
    ) -> Result<CacheStatus> {
        let pins = self.runtime.state.cache_pins(range.workspace).await?;
        let pinned: BTreeSet<_> = pins
            .iter()
            .flat_map(|pin| pin.objects.iter().map(|object| object.id.clone()))
            .collect();
        let mut missing = Vec::new();
        let mut completed_bytes = 0u64;
        for object in objects {
            if self
                .trees
                .content
                .local
                .contains(range.workspace, object)
                .await?
            {
                completed_bytes = completed_bytes
                    .checked_add(object.size)
                    .ok_or_else(|| Error::integrity("cache byte count overflow"))?;
            } else {
                missing.push(object.id.clone());
            }
        }
        Ok(CacheStatus {
            revision: range.revision,
            paths,
            complete: missing.is_empty(),
            offline_ready: missing.is_empty()
                && objects.iter().all(|object| pinned.contains(&object.id)),
            completed_bytes,
            missing,
        })
    }

    async fn fetch(&self, range: &RevisionRange, objects: &[ObjectRef]) -> Result<()> {
        for object in objects {
            let _reader = self.trees.content.open(range.workspace, object).await?;
        }
        Ok(())
    }
}

#[async_trait]
impl CacheApi for CacheService {
    async fn prefetch(
        &self,
        context: RequestContext,
        request: RevisionRange,
    ) -> Result<OperationRecord> {
        self.runtime
            .authorize(
                &context.principal,
                request.workspace,
                None,
                Permission::Read,
            )
            .await?;
        let operation = match self
            .operations
            .start(
                context,
                "revision_prefetch",
                &request,
                Some(request.workspace),
                None,
            )
            .await?
        {
            OperationStart::Existing(record) => return Ok(record),
            OperationStart::New(context) => context,
        };
        let result = async {
            let _protection = self.runtime.objects.read().await;
            let revision = self
                .trees
                .revision(request.workspace, request.revision)
                .await?;
            let (paths, objects) = self.selection(&request).await?;
            let cached = self.inspect(&request, paths.clone(), &objects).await?;
            let protected = self.trees.content.refs.is_some() && !cached.complete;
            if protected {
                self.trees
                    .content
                    .protect_remote(
                        request.workspace,
                        operation.operation.id,
                        vec![revision.root],
                    )
                    .await?;
            }
            self.fetch(&request, &objects).await?;
            let status = self.inspect(&request, paths, &objects).await?;
            let record = self
                .operations
                .finish(
                    operation.clone(),
                    CommandResult::Prefetch { status },
                    LocalCommit::default(),
                )
                .await?;
            if protected {
                self.trees
                    .content
                    .release_remote(request.workspace, operation.operation.id)
                    .await?;
            }
            Ok(record)
        }
        .await;
        match result {
            Ok(record) => Ok(record),
            Err(error) => self.operations.fail(&operation, error).await,
        }
    }

    async fn pin(&self, context: RequestContext, request: PinRequest) -> Result<OperationRecord> {
        self.runtime
            .authorize(
                &context.principal,
                request.range.workspace,
                None,
                Permission::Read,
            )
            .await?;
        let operation = match self
            .operations
            .start(
                context,
                "cache_pin",
                &request,
                Some(request.range.workspace),
                None,
            )
            .await?
        {
            OperationStart::Existing(record) => return Ok(record),
            OperationStart::New(context) => context,
        };
        let result = async {
            let _protection = self.runtime.objects.read().await;
            let (paths, objects) = self.selection(&request.range).await?;
            if request.enabled {
                self.fetch(&request.range, &objects).await?;
            }
            let pin = CachePin {
                workspace: request.range.workspace,
                revision: request.range.revision,
                paths,
                objects,
            };
            self.operations
                .finish(
                    operation.clone(),
                    CommandResult::Updated,
                    LocalCommit {
                        pin_updates: vec![PinUpdate {
                            pin,
                            enabled: request.enabled,
                        }],
                        ..Default::default()
                    },
                )
                .await
        }
        .await;
        match result {
            Ok(record) => Ok(record),
            Err(error) => self.operations.fail(&operation, error).await,
        }
    }

    async fn status(&self, actor: &Principal, request: RevisionRange) -> Result<CacheStatus> {
        self.runtime
            .authorize(actor, request.workspace, None, Permission::Read)
            .await?;
        let _protection = self.runtime.objects.read().await;
        let (paths, objects) = self.selection(&request).await?;
        self.inspect(&request, paths, &objects).await
    }

    async fn collect(&self) -> Result<u64> {
        let _exclusive = self.runtime.objects.write().await;
        let mut entries = Vec::new();
        let mut protected = BTreeSet::new();
        for workspace in self.runtime.state.workspaces().await? {
            for pin in self.runtime.state.cache_pins(workspace.id).await? {
                protected.extend(
                    pin.objects
                        .into_iter()
                        .map(|object| (workspace.id, object.id)),
                );
            }
            let mut roots = Vec::new();
            for branch in self.runtime.state.branches(workspace.id).await? {
                if branch.state != BranchState::Deleted {
                    roots.push(branch.working_root);
                }
            }
            for job in self.runtime.state.sync_jobs(workspace.id).await? {
                roots.extend([job.reference.working_root, job.reference.record_index]);
            }
            for candidate in self.runtime.state.merges(workspace.id).await? {
                if candidate.applied_revision.is_none() {
                    roots.push(
                        self.trees
                            .revision(workspace.id, candidate.revision)
                            .await?
                            .root,
                    );
                }
            }
            protected.extend(
                self.trees
                    .closure(workspace.id, roots)
                    .await?
                    .into_iter()
                    .map(|object| (workspace.id, object.id)),
            );
            for revision in self.runtime.state.revisions(workspace.id).await? {
                protected.insert((workspace.id, revision.root.id));
            }
            entries.extend(self.trees.content.local.objects(workspace.id).await?);
        }
        let mut total = entries
            .iter()
            .try_fold(0u64, |sum, entry| sum.checked_add(entry.reference.size))
            .ok_or_else(|| Error::integrity("cache size overflow"))?;
        let initial = self.trees.content.local.stats().await?;
        let mut free = initial.available_bytes;
        if total <= self.config.max_bytes && free >= self.config.min_free_bytes {
            return Ok(0);
        }
        entries.sort_by_key(|entry| entry.last_access_ns);
        let mut freed = 0u64;
        for entry in entries {
            if total <= self.config.target_bytes && free >= self.config.min_free_bytes {
                break;
            }
            if entry.remote_confirmed
                && !protected.contains(&(entry.workspace, entry.reference.id.clone()))
                && self
                    .trees
                    .content
                    .local
                    .evict(entry.workspace, &entry.reference)
                    .await?
            {
                total = total.saturating_sub(entry.reference.size);
                free = free.saturating_add(entry.reference.size);
                freed = freed.saturating_add(entry.reference.size);
            }
        }
        Ok(freed)
    }
}
