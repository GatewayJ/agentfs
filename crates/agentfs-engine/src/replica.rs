use crate::{ContentService, OperationService, Runtime, TreeService};
use agentfs_model::*;
use agentfs_ports::*;
use async_trait::async_trait;
use std::{collections::BTreeMap, sync::Arc};
use tokio::sync::Mutex;

#[derive(Debug)]
pub struct ReplicaService {
    runtime: Arc<Runtime>,
    trees: Arc<TreeService>,
    content: Arc<ContentService>,
    operations: Arc<OperationService>,
    publication: Mutex<()>,
}

impl ReplicaService {
    pub fn new(
        runtime: Arc<Runtime>,
        trees: Arc<TreeService>,
        content: Arc<ContentService>,
        operations: Arc<OperationService>,
    ) -> Arc<Self> {
        Arc::new(Self {
            runtime,
            trees,
            content,
            operations,
            publication: Mutex::new(()),
        })
    }

    pub(crate) async fn upload_closure(
        &self,
        workspace: WorkspaceId,
        roots: Vec<ObjectRef>,
    ) -> Result<()> {
        let remote = self.content.remote.as_ref().ok_or_else(|| {
            Error::new(
                ErrorCode::RemoteRequired,
                "remote storage is not configured",
            )
        })?;
        for object in self.trees.closure(workspace, roots).await? {
            remote
                .upload(
                    workspace,
                    self.content.local.open(workspace, &object).await?,
                )
                .await?;
            self.content
                .local
                .confirm_remote(workspace, &object)
                .await?;
        }
        Ok(())
    }

    pub(crate) async fn contains_operation(
        &self,
        reference: &BranchRef,
        operation: &OperationRecord,
    ) -> Result<bool> {
        for entry in self
            .trees
            .load_index::<HistoryRecord>(reference.workspace, &reference.record_index, 1_000_000)
            .await?
        {
            if let HistoryRecord::Operation(existing) = entry.value
                && existing.id == operation.id
            {
                if existing.fingerprint != operation.fingerprint
                    || existing.principal != operation.principal
                    || existing.result != operation.result
                {
                    return Err(Error::integrity(
                        "remote operation result differs from its local durable result",
                    ));
                }
                return Ok(true);
            }
        }
        Ok(false)
    }

    async fn acknowledge(&self, job: &SyncJob, remote: BranchRef) -> Result<()> {
        let mut operation = self
            .runtime
            .state
            .operation(job.operation)
            .await?
            .ok_or_else(|| Error::integrity("sync job operation is missing"))?;
        operation.remote_confirmed = true;
        operation.error = None;
        if operation.pending.is_none() {
            operation.phase = OperationPhase::Complete;
        }
        self.runtime
            .state
            .commit(LocalCommit {
                operations: vec![operation],
                completed_jobs: vec![job.operation],
                remote_refs: vec![remote],
                ..Default::default()
            })
            .await?;
        if let Err(error) = self
            .content
            .release_remote(job.reference.workspace, job.operation)
            .await
        {
            tracing::warn!(operation = %job.operation, error = %error, "publication confirmed; retention registration remains for recovery");
        }
        Ok(())
    }

    async fn publish_job(&self, job: &SyncJob) -> Result<()> {
        let refs = self.content.refs.as_ref().ok_or_else(|| {
            Error::new(
                ErrorCode::RemoteRequired,
                "remote storage is not configured",
            )
        })?;
        let reference = &job.reference;
        let operation = self
            .runtime
            .state
            .operation(job.operation)
            .await?
            .ok_or_else(|| Error::integrity("sync job operation is missing"))?;
        let key = RefKey::Branch {
            workspace: reference.workspace,
            branch: reference.branch,
        };
        self.content
            .register_publication(
                reference.workspace,
                job.operation,
                vec![
                    reference.working_root.clone(),
                    reference.record_index.clone(),
                ],
            )
            .await?;
        let mut uploaded = false;
        for _ in 0..8 {
            let current = refs.get(&key).await?;
            if let Some(current) = &current {
                let remote =
                    decode_branch_ref(reference.workspace, reference.branch, &current.bytes)?;
                if remote == *reference || self.contains_operation(&remote, &operation).await? {
                    return self.acknowledge(job, remote).await;
                }
                if remote.owner != reference.owner
                    || remote.authority_epoch != reference.authority_epoch
                {
                    return Err(Error::new(
                        ErrorCode::StaleAuthority,
                        "remote branch authority has changed",
                    ));
                }
                if remote.source_generation >= reference.source_generation {
                    return Err(Error::integrity(
                        "remote publication is ahead without the expected operation record",
                    ));
                }
            } else if reference.authority_epoch != 1 {
                return Err(Error::new(
                    ErrorCode::StaleAuthority,
                    "transferred branch is missing its remote authority record",
                ));
            }
            if !uploaded {
                self.upload_closure(
                    reference.workspace,
                    vec![
                        reference.working_root.clone(),
                        reference.record_index.clone(),
                    ],
                )
                .await?;
                uploaded = true;
            }
            let update = RefUpdate {
                key: key.clone(),
                expected_version: current.map(|value| value.version),
                bytes: encode(reference)?.into(),
            };
            match refs.compare_exchange(update).await? {
                CasOutcome::Applied { .. } => {
                    return self.acknowledge(job, reference.clone()).await;
                }
                CasOutcome::Conflict | CasOutcome::Unknown => continue,
            }
        }
        Err(Error::new(
            ErrorCode::RemoteUnknown,
            "remote publication outcome remains unconfirmed",
        )
        .retryable())
    }

    async fn import_history(&self, reference: &BranchRef) -> Result<()> {
        let mut commit = LocalCommit::default();
        for entry in self
            .trees
            .load_index::<HistoryRecord>(reference.workspace, &reference.record_index, 1_000_000)
            .await?
        {
            match entry.value {
                HistoryRecord::Revision(revision) => {
                    if revision.workspace != reference.workspace {
                        return Err(Error::integrity("revision belongs to another workspace"));
                    }
                    self.content
                        .metadata(reference.workspace, &revision.root)
                        .await?;
                    commit.revisions.push(revision);
                }
                HistoryRecord::Turn(turn) => {
                    if turn.session.workspace != reference.workspace {
                        return Err(Error::integrity("turn belongs to another workspace"));
                    }
                    let existing = self.runtime.state.turns(reference.workspace).await?;
                    if !existing
                        .iter()
                        .any(|old| old.session == turn.session && old.turn_id == turn.turn_id)
                    {
                        commit.turns.push(turn);
                    }
                }
                HistoryRecord::Operation(operation) => {
                    if operation.workspace != Some(reference.workspace) {
                        return Err(Error::integrity("operation belongs to another workspace"));
                    }
                    if self.runtime.state.operation(operation.id).await?.is_none() {
                        commit.operations.push(*operation);
                    }
                }
            }
        }
        if !commit
            .revisions
            .iter()
            .any(|revision| revision.id == reference.formal_head)
            && self
                .runtime
                .state
                .revision(reference.formal_head)
                .await?
                .is_none()
        {
            return Err(Error::integrity(
                "remote formal head is absent from retained history",
            ));
        }
        commit.remote_refs.push(reference.clone());
        self.runtime.state.commit(commit).await
    }
}

pub(crate) fn decode_branch_ref(
    workspace: WorkspaceId,
    branch: BranchId,
    bytes: &[u8],
) -> Result<BranchRef> {
    if bytes.len() > MAX_METADATA_BYTES {
        return Err(Error::integrity("branch reference exceeds format limit"));
    }
    let reference: BranchRef = decode(bytes)?;
    if reference.workspace != workspace
        || reference.branch != branch
        || reference.authority_epoch == 0
        || reference.record_index.kind != ObjectKind::RecordIndex
        || reference.working_root.kind != ObjectKind::Tree
    {
        return Err(Error::integrity("invalid branch reference identity"));
    }
    Ok(reference)
}

#[async_trait]
impl ReplicaPublisher for ReplicaService {
    async fn publish_workspace(&self, workspace: WorkspaceId) -> Result<()> {
        let refs = self.content.refs.as_ref().ok_or_else(|| {
            Error::new(
                ErrorCode::RemoteRequired,
                "remote storage is not configured",
            )
        })?;
        let workspace = self
            .runtime
            .state
            .workspace(workspace)
            .await?
            .ok_or_else(|| Error::new(ErrorCode::NotFound, "workspace does not exist"))?;
        let revision = self
            .trees
            .revision(workspace.id, workspace.initial_revision)
            .await?;
        let key = RefKey::Workspace(workspace.id);
        for _ in 0..8 {
            if let Some(existing) = refs.get(&key).await? {
                let control: WorkspaceControl = decode(&existing.bytes)?;
                if control.workspace != workspace || control.initial_revision != revision {
                    return Err(Error::integrity(
                        "remote workspace identity or policy differs",
                    ));
                }
                return Ok(());
            }
            self.upload_closure(workspace.id, vec![revision.root.clone()])
                .await?;
            let control = WorkspaceControl {
                workspace: workspace.clone(),
                initial_revision: revision.clone(),
                gc_epoch: 0,
                phase: GcPhase::Open,
                active_operations: BTreeMap::new(),
                history_pins: BTreeMap::new(),
            };
            match refs
                .compare_exchange(RefUpdate {
                    key: key.clone(),
                    expected_version: None,
                    bytes: encode(&control)?.into(),
                })
                .await?
            {
                CasOutcome::Applied { .. } => return Ok(()),
                CasOutcome::Conflict | CasOutcome::Unknown => continue,
            }
        }
        Err(Error::new(
            ErrorCode::RemoteUnknown,
            "workspace publication remains unconfirmed",
        )
        .retryable())
    }

    async fn drain(&self, workspace: WorkspaceId, branch: Option<BranchId>) -> Result<()> {
        let _publication = self.publication.lock().await;
        self.publish_workspace(workspace).await?;
        let mut blocked = std::collections::BTreeSet::new();
        let mut first_error = None;
        for mut job in self.runtime.state.sync_jobs(workspace).await? {
            if branch.is_some_and(|branch| branch != job.reference.branch)
                || blocked.contains(&job.reference.branch)
            {
                continue;
            }
            if let Err(error) = self.publish_job(&job).await {
                job.attempts = job.attempts.saturating_add(1);
                job.error = Some(error.clone());
                let mut operation = self
                    .runtime
                    .state
                    .operation(job.operation)
                    .await?
                    .ok_or_else(|| Error::integrity("sync operation is missing"))?;
                operation.error = Some(error.clone());
                if operation.pending.is_none() {
                    operation.phase = OperationPhase::LocalSaved;
                }
                let mut commit = LocalCommit {
                    sync_jobs: vec![job.clone()],
                    operations: vec![operation],
                    ..Default::default()
                };
                if error.code == ErrorCode::StaleAuthority {
                    let _gate = self.runtime.lock_branch(job.reference.branch).await;
                    if let Some(mut branch) =
                        self.runtime.state.branch(job.reference.branch).await?
                    {
                        branch.state = BranchState::Stopped;
                        commit.branches.push(branch);
                    }
                    self.runtime.state.commit(commit).await?;
                } else {
                    self.runtime.state.commit(commit).await?;
                }
                blocked.insert(job.reference.branch);
                first_error.get_or_insert(error);
            }
        }
        first_error.map_or(Ok(()), Err)
    }

    async fn build_job(
        &self,
        branch: &Branch,
        new_revision: Option<&Revision>,
        operation: &OperationRecord,
        turn: Option<&TurnRecord>,
    ) -> Result<SyncJob> {
        let mut records = BTreeMap::new();
        for revision in self.runtime.state.revisions(branch.workspace).await? {
            records.insert(
                format!("revision/{}", revision.id),
                HistoryRecord::Revision(revision),
            );
        }
        if let Some(revision) = new_revision {
            records.insert(
                format!("revision/{}", revision.id),
                HistoryRecord::Revision(revision.clone()),
            );
        }
        for record in self.runtime.state.turns(branch.workspace).await? {
            let key = object_ref(
                ObjectKind::RecordIndex,
                &encode(&(&record.session, &record.turn_id))?,
            )
            .id;
            records.insert(format!("turn/{key}"), HistoryRecord::Turn(record));
        }
        if let Some(record) = turn {
            let key = object_ref(
                ObjectKind::RecordIndex,
                &encode(&(&record.session, &record.turn_id))?,
            )
            .id;
            records.insert(format!("turn/{key}"), HistoryRecord::Turn(record.clone()));
        }
        for record in self.runtime.state.operations(branch.workspace).await? {
            if record.branch == Some(branch.id) && record.local_saved {
                records.insert(
                    format!("operation/{}", record.id),
                    HistoryRecord::Operation(Box::new(record)),
                );
            }
        }
        records.insert(
            format!("operation/{}", operation.id),
            HistoryRecord::Operation(Box::new(operation.clone())),
        );
        let record_index = self
            .trees
            .save_index(
                branch.workspace,
                ObjectKind::RecordIndex,
                records
                    .into_iter()
                    .map(|(key, value)| IndexEntry { key, value })
                    .collect(),
            )
            .await?;
        Ok(SyncJob {
            operation: operation.id,
            reference: BranchRef {
                workspace: branch.workspace,
                branch: branch.id,
                owner: branch.owner,
                authority_epoch: branch.authority_epoch,
                generation: branch.generation,
                source_generation: branch.source_generation,
                durable_seq: branch.saved_seq,
                formal_head: branch.formal_head,
                autosave_head: branch.autosave_head,
                working_root: branch.working_root.clone(),
                record_index,
                deleted: branch.state == BranchState::Deleted,
                write_stopped: branch.state == BranchState::Stopped,
            },
            attempts: 0,
            error: None,
        })
    }

    async fn discover(&self, workspace: WorkspaceId) -> Result<Vec<BranchRef>> {
        let refs = self.content.refs.as_ref().ok_or_else(|| {
            Error::new(
                ErrorCode::RemoteRequired,
                "remote storage is not configured",
            )
        })?;
        let _protection = self.runtime.objects.read().await;
        let mut result = Vec::new();
        for id in refs.list_branches(workspace).await? {
            let Some(value) = refs
                .get(&RefKey::Branch {
                    workspace,
                    branch: id,
                })
                .await?
            else {
                continue;
            };
            let reference = decode_branch_ref(workspace, id, &value.bytes)?;
            let pin = OperationId::new();
            self.content
                .protect_remote(
                    workspace,
                    pin,
                    vec![
                        reference.working_root.clone(),
                        reference.record_index.clone(),
                    ],
                )
                .await?;
            self.import_history(&reference).await?;
            self.content
                .metadata(workspace, &reference.working_root)
                .await?;
            if reference.owner == self.runtime.location
                && !reference.deleted
                && self.runtime.state.branch(id).await?.is_none()
            {
                let tree = self.trees.tree(workspace, &reference.working_root).await?;
                let next_inode = crate::runtime::increment(
                    tree.values().map(|inode| inode.id.0).max().unwrap_or(1),
                )?;
                let formal = self
                    .trees
                    .revision(workspace, reference.formal_head)
                    .await?;
                let branch = Branch {
                    id,
                    workspace,
                    name: format!("adopted-{id}"),
                    owner: reference.owner,
                    authority_epoch: reference.authority_epoch,
                    generation: reference.generation,
                    mutation_seq: reference.durable_seq,
                    saved_seq: reference.durable_seq,
                    source_generation: reference.source_generation,
                    formal_head: reference.formal_head,
                    autosave_head: reference.autosave_head,
                    working_root: reference.working_root.clone(),
                    base_revision: reference.formal_head,
                    next_inode,
                    dirty: formal.root != reference.working_root,
                    state: if reference.write_stopped {
                        BranchState::Stopped
                    } else {
                        BranchState::Writable
                    },
                    protected: false,
                };
                self.runtime
                    .state
                    .commit(LocalCommit {
                        new_branches: vec![branch],
                        node_changes: vec![NodeChange::Replace {
                            branch: id,
                            nodes: tree
                                .into_iter()
                                .map(|(path, inode)| Node {
                                    path,
                                    inode,
                                    dirty: false,
                                })
                                .collect(),
                        }],
                        ..Default::default()
                    })
                    .await?;
            }
            self.content.release_remote(workspace, pin).await?;
            result.push(reference);
        }
        Ok(result)
    }
}

#[async_trait]
impl ReplicaApi for ReplicaService {
    async fn sync(&self, context: RequestContext, request: SyncRequest) -> Result<OperationRecord> {
        if self
            .runtime
            .state
            .workspace(request.workspace)
            .await?
            .is_none()
        {
            let refs = self.content.refs.as_ref().ok_or_else(|| {
                Error::new(
                    ErrorCode::RemoteRequired,
                    "remote storage is not configured",
                )
            })?;
            let value = refs
                .get(&RefKey::Workspace(request.workspace))
                .await?
                .ok_or_else(|| {
                    Error::new(ErrorCode::NotFound, "remote workspace does not exist")
                })?;
            if value.bytes.len() > MAX_METADATA_BYTES {
                return Err(Error::integrity("workspace control exceeds format limit"));
            }
            let control: WorkspaceControl = decode(&value.bytes)?;
            if control.workspace.id != request.workspace
                || control.workspace.format_version != FORMAT_VERSION
                || control.initial_revision.id != control.workspace.initial_revision
                || control.initial_revision.workspace != request.workspace
            {
                return Err(Error::integrity("invalid remote workspace"));
            }
            control
                .workspace
                .authorize(&context.principal, None, Permission::Read)?;
            let _protection = self.runtime.objects.read().await;
            self.content
                .metadata(request.workspace, &control.initial_revision.root)
                .await?;
            match self
                .runtime
                .state
                .commit(LocalCommit {
                    new_workspaces: vec![control.workspace],
                    revisions: vec![control.initial_revision],
                    ..Default::default()
                })
                .await
            {
                Ok(()) => (),
                Err(error)
                    if error.code == ErrorCode::Conflict
                        && self
                            .runtime
                            .state
                            .workspace(request.workspace)
                            .await?
                            .is_some() => {}
                Err(error) => return Err(error),
            }
        }
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
                "workspace_sync",
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
            let mut branches = Vec::new();
            if matches!(request.direction, SyncDirection::Push | SyncDirection::Both) {
                self.runtime
                    .authorize(
                        &operation.operation.principal,
                        request.workspace,
                        None,
                        Permission::Write,
                    )
                    .await?;
                if let Some(selected) = &request.branches {
                    for id in selected {
                        self.drain(request.workspace, Some(*id)).await?;
                        branches.push(*id);
                    }
                } else {
                    self.drain(request.workspace, None).await?;
                    branches.extend(
                        self.runtime
                            .state
                            .branches(request.workspace)
                            .await?
                            .into_iter()
                            .map(|branch| branch.id),
                    );
                }
            }
            if matches!(request.direction, SyncDirection::Pull | SyncDirection::Both) {
                branches.extend(
                    self.discover(request.workspace)
                        .await?
                        .into_iter()
                        .filter(|reference| {
                            request
                                .branches
                                .as_ref()
                                .is_none_or(|ids| ids.contains(&reference.branch))
                        })
                        .map(|reference| reference.branch),
                );
            }
            branches.sort();
            branches.dedup();
            self.operations
                .finish(
                    operation.clone(),
                    CommandResult::Synced { branches },
                    LocalCommit::default(),
                )
                .await
        }
        .await;
        match result {
            Ok(record) => Ok(record),
            Err(error) => self.operations.fail(&operation, error).await,
        }
    }
}
