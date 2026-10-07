use crate::{
    OperationService, Runtime, TreeService,
    files::read_content,
    runtime::{check_guard, increment},
};
use agentfs_model::*;
use agentfs_ports::*;
use async_trait::async_trait;
use std::{collections::BTreeSet, sync::Arc};

pub struct RevisionService {
    pub(crate) runtime: Arc<Runtime>,
    pub(crate) trees: Arc<TreeService>,
    pub(crate) operations: Arc<OperationService>,
    pub(crate) publisher: Arc<dyn ReplicaPublisher>,
    snapshots: Arc<dyn SnapshotCapture>,
}

impl std::fmt::Debug for RevisionService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RevisionService").finish_non_exhaustive()
    }
}

impl RevisionService {
    pub fn new(
        runtime: Arc<Runtime>,
        trees: Arc<TreeService>,
        operations: Arc<OperationService>,
        publisher: Arc<dyn ReplicaPublisher>,
        snapshots: Arc<dyn SnapshotCapture>,
    ) -> Arc<Self> {
        Arc::new(Self {
            runtime,
            trees,
            operations,
            publisher,
            snapshots,
        })
    }

    pub(crate) async fn finish_durability(
        &self,
        operation: OperationRecord,
        durability: Durability,
    ) -> Result<OperationRecord> {
        if durability == Durability::Remote {
            let workspace = operation
                .workspace
                .ok_or_else(|| Error::integrity("saved operation has no workspace"))?;
            if let Err(error) = self.publisher.drain(workspace, operation.branch).await {
                let mut current = self
                    .runtime
                    .state
                    .operation(operation.id)
                    .await?
                    .ok_or_else(|| Error::integrity("saved operation is missing"))?;
                current.error = Some(error);
                self.runtime
                    .state
                    .commit(LocalCommit {
                        operations: vec![current.clone()],
                        ..Default::default()
                    })
                    .await?;
                return Ok(current);
            }
        }
        self.runtime
            .state
            .operation(operation.id)
            .await?
            .ok_or_else(|| Error::integrity("saved operation is missing"))
    }

    /// The branch lock and object retention lock are owned by the caller.
    pub(crate) async fn save_locked(
        &self,
        context: OperationContext,
        request: SaveRevision,
    ) -> Result<OperationRecord> {
        let mut branch = self.runtime.branch(request.guard.branch).await?;
        check_guard(&branch, &request.guard)?;
        let permission = if request
            .parents
            .as_ref()
            .is_some_and(|parents| parents.len() > 1)
        {
            Permission::Merge
        } else {
            Permission::Write
        };
        self.runtime
            .authorize(
                &context.operation.principal,
                branch.workspace,
                Some(branch.id),
                permission,
            )
            .await?;
        if branch.owner != self.runtime.location || branch.state != BranchState::Writable {
            return Err(Error::new(
                ErrorCode::StaleAuthority,
                "branch is not writable at this location",
            ));
        }
        if branch.protected && permission != Permission::Merge {
            return Err(Error::new(
                ErrorCode::ReadOnly,
                "protected branch requires a validated merge",
            ));
        }
        let replacing = request.replace_root.is_some();
        let captured = if let Some(root) = request.replace_root {
            let tree = self.trees.tree(branch.workspace, &root).await?;
            CapturedTree {
                branch: branch.clone(),
                root,
                captured_seq: increment(branch.mutation_seq)?,
                nodes: tree
                    .into_iter()
                    .map(|(path, inode)| Node {
                        path,
                        inode,
                        dirty: false,
                    })
                    .collect(),
            }
        } else {
            self.snapshots.capture(&branch).await?
        };
        let formal = self
            .trees
            .revision(branch.workspace, branch.formal_head)
            .await?;
        let reuse = request.parents.is_none() && captured.root == formal.root;
        let reuse_autosave = request.parents.is_none()
            && request.kind != RevisionKind::Commit
            && captured.root == branch.working_root
            && branch.autosave_head.is_some();
        let revision_id = if reuse {
            branch.formal_head
        } else if reuse_autosave {
            branch
                .autosave_head
                .ok_or_else(|| Error::integrity("autosave reference is missing"))?
        } else {
            RevisionId::new()
        };
        let revision = if reuse || reuse_autosave {
            None
        } else {
            let parents = request
                .parents
                .unwrap_or_else(|| vec![branch.autosave_head.unwrap_or(branch.formal_head)]);
            if parents.is_empty() || parents.len() > 2 {
                return Err(Error::invalid("revision must have one or two parents"));
            }
            for parent in &parents {
                self.trees.revision(branch.workspace, *parent).await?;
            }
            Some(Revision {
                id: revision_id,
                workspace: branch.workspace,
                branch: Some(branch.id),
                parents,
                root: captured.root.clone(),
                kind: request.kind,
                actor: context.operation.principal.clone(),
                created_ns: self.runtime.clock.now_ns(),
                operation: context.operation.id,
            })
        };
        if replacing {
            branch.generation = increment(branch.generation)?;
        }
        branch.working_root = captured.root;
        branch.saved_seq = captured.captured_seq;
        branch.mutation_seq = captured.captured_seq;
        branch.source_generation = increment(branch.source_generation)?;
        branch.next_inode = branch.next_inode.max(increment(
            captured
                .nodes
                .iter()
                .map(|node| node.inode.id.0)
                .max()
                .unwrap_or(1),
        )?);
        if request.kind == RevisionKind::Commit {
            branch.formal_head = revision_id;
            branch.autosave_head = None;
            branch.dirty = false;
        } else {
            branch.autosave_head = if reuse { None } else { Some(revision_id) };
            branch.dirty = branch.working_root != formal.root;
        }
        let mut turn = request.turn;
        if let Some(turn) = &mut turn {
            turn.result_revision = Some(revision_id);
        }
        let mut operation = context.operation;
        operation.workspace = Some(branch.workspace);
        operation.branch = Some(branch.id);
        operation.local_saved = true;
        operation.phase = if operation.pending.is_some() {
            OperationPhase::CommittedMountPending
        } else {
            OperationPhase::LocalSaved
        };
        operation.result = Some(match request.result {
            SaveResult::Turn => CommandResult::Turn {
                record: turn
                    .clone()
                    .ok_or_else(|| Error::integrity("turn result is missing"))?,
            },
            SaveResult::Session => CommandResult::Session {
                binding: request
                    .session
                    .clone()
                    .ok_or_else(|| Error::integrity("session result is missing"))?,
            },
            SaveResult::Revision => CommandResult::Revision {
                revision: revision_id,
                branch: branch.id,
            },
        });
        let mut merges = Vec::new();
        if let Some(PendingAction::ApplyRevision {
            merge: Some(id), ..
        }) = &operation.pending
        {
            let mut candidate = self
                .runtime
                .state
                .merge(*id)
                .await?
                .ok_or_else(|| Error::integrity("merge candidate is missing"))?;
            candidate.applied_revision = Some(revision_id);
            merges.push(candidate);
        }
        let job = self
            .publisher
            .build_job(&branch, revision.as_ref(), &operation, turn.as_ref())
            .await?;
        self.runtime
            .state
            .commit(LocalCommit {
                guards: vec![request.guard],
                branches: vec![branch.clone()],
                revisions: revision.into_iter().collect(),
                node_changes: vec![NodeChange::Replace {
                    branch: branch.id,
                    nodes: captured.nodes,
                }],
                operations: vec![operation.clone()],
                turns: turn.into_iter().collect(),
                sessions: request.session.into_iter().collect(),
                merges,
                sync_jobs: vec![job],
                ..Default::default()
            })
            .await?;
        self.runtime.clear_dirty(branch.id, branch.saved_seq);
        Ok(operation)
    }

    pub(crate) async fn preserve_working_locked(
        &self,
        branch: &Branch,
        actor: &Principal,
    ) -> Result<Branch> {
        if branch.mutation_seq == branch.saved_seq {
            return Ok(branch.clone());
        }
        let request = SaveRevision {
            guard: BranchGuard::from(branch),
            kind: RevisionKind::Autosave,
            parents: None,
            replace_root: None,
            turn: None,
            session: None,
            result: SaveResult::Revision,
            durability: Durability::Local,
        };
        let start = self
            .operations
            .start(
                RequestContext {
                    principal: actor.clone(),
                    request_id: format!("checkpoint-{}", OperationId::new()),
                },
                "working_checkpoint",
                &request.guard,
                Some(branch.workspace),
                Some(branch.id),
            )
            .await?;
        let OperationStart::New(context) = start else {
            return Err(Error::integrity(
                "generated checkpoint operation already exists",
            ));
        };
        match self.save_locked(context.clone(), request).await {
            Ok(_) => self.runtime.branch(branch.id).await,
            Err(error) => {
                self.operations.fail(&context, error.clone()).await?;
                Err(error)
            }
        }
    }

    pub(crate) async fn fork_locked(
        &self,
        context: OperationContext,
        workspace: WorkspaceId,
        source: RevisionId,
        name: String,
        session: Option<SessionBinding>,
    ) -> Result<OperationRecord> {
        validate_label(&name, "branch name")?;
        let revision = self.trees.revision(workspace, source).await?;
        let tree = self.trees.tree(workspace, &revision.root).await?;
        let id = session
            .as_ref()
            .map(|binding| binding.branch)
            .unwrap_or_default();
        let next_inode = increment(tree.values().map(|inode| inode.id.0).max().unwrap_or(1))?;
        let branch = Branch {
            id,
            workspace,
            name,
            owner: self.runtime.location,
            authority_epoch: 1,
            generation: 1,
            mutation_seq: 0,
            saved_seq: 0,
            source_generation: 1,
            formal_head: source,
            autosave_head: None,
            working_root: revision.root.clone(),
            base_revision: source,
            next_inode,
            dirty: false,
            state: BranchState::Writable,
            protected: false,
        };
        let mut operation = context.operation;
        operation.workspace = Some(workspace);
        operation.branch = Some(id);
        operation.local_saved = true;
        operation.phase = if operation.pending.is_some() {
            OperationPhase::CommittedMountPending
        } else {
            OperationPhase::LocalSaved
        };
        operation.result = Some(if let Some(binding) = &session {
            CommandResult::Session {
                binding: binding.clone(),
            }
        } else {
            CommandResult::Branch {
                branch: branch.clone(),
            }
        });
        let job = self
            .publisher
            .build_job(&branch, None, &operation, None)
            .await?;
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
                operations: vec![operation.clone()],
                sessions: session.into_iter().collect(),
                sync_jobs: vec![job],
                ..Default::default()
            })
            .await?;
        Ok(operation)
    }
}

pub(crate) fn validate_label(value: &str, field: &str) -> Result<()> {
    if value.is_empty() || value.len() > 256 || value.chars().any(char::is_control) {
        return Err(Error::invalid(format!("invalid {field}")));
    }
    Ok(())
}

#[async_trait]
impl RevisionWriter for RevisionService {
    async fn save(
        &self,
        context: OperationContext,
        request: SaveRevision,
    ) -> Result<OperationRecord> {
        let durability = request.durability;
        let operation = {
            let _gate = self.runtime.lock_branch(request.guard.branch).await;
            let _protection = self.runtime.objects.read().await;
            self.save_locked(context, request).await?
        };
        self.finish_durability(operation, durability).await
    }
}

#[async_trait]
impl DurabilityService for RevisionService {
    async fn flush(
        &self,
        branch: BranchId,
        actor: Principal,
        durability: Durability,
    ) -> Result<DurabilityReceipt> {
        let (operation, sequence) = {
            let _gate = self.runtime.lock_branch(branch).await;
            let _protection = self.runtime.objects.read().await;
            let branch = self.runtime.branch(branch).await?;
            let request = SaveRevision {
                guard: BranchGuard::from(&branch),
                kind: RevisionKind::Autosave,
                parents: None,
                replace_root: None,
                turn: None,
                session: None,
                result: SaveResult::Revision,
                durability,
            };
            let context = RequestContext {
                principal: actor,
                request_id: format!("fsync-{}", OperationId::new()),
            };
            let start = self
                .operations
                .start(
                    context,
                    "fsync",
                    &request.guard,
                    Some(branch.workspace),
                    Some(branch.id),
                )
                .await?;
            let OperationStart::New(context) = start else {
                return Err(Error::integrity("generated fsync operation already exists"));
            };
            match self.save_locked(context.clone(), request).await {
                Ok(record) => (record, branch.mutation_seq),
                Err(error) => {
                    self.operations.fail(&context, error.clone()).await?;
                    return Err(error);
                }
            }
        };
        let operation = self.finish_durability(operation, durability).await?;
        if durability == Durability::Remote && !operation.remote_confirmed {
            return Err(operation.error.unwrap_or_else(|| {
                Error::new(
                    ErrorCode::RemoteUnknown,
                    "remote durability is not confirmed",
                )
                .retryable()
            }));
        }
        Ok(DurabilityReceipt {
            branch,
            mutation_seq: sequence,
            local_saved: true,
            remote_confirmed: operation.remote_confirmed,
            operation: operation.id,
        })
    }
}

pub struct VersionService {
    revisions: Arc<RevisionService>,
    transitions: Arc<dyn RevisionTransition>,
}

impl std::fmt::Debug for VersionService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VersionService").finish_non_exhaustive()
    }
}

impl VersionService {
    pub fn new(
        revisions: Arc<RevisionService>,
        transitions: Arc<dyn RevisionTransition>,
    ) -> Arc<Self> {
        Arc::new(Self {
            revisions,
            transitions,
        })
    }
}

#[async_trait]
impl RevisionApi for VersionService {
    async fn commit(
        &self,
        context: RequestContext,
        request: CommitRevision,
    ) -> Result<OperationRecord> {
        if !matches!(
            request.kind,
            RevisionKind::Commit | RevisionKind::Checkpoint
        ) {
            return Err(Error::invalid(
                "explicit save kind must be commit or checkpoint",
            ));
        }
        let branch = self.revisions.runtime.branch(request.guard.branch).await?;
        self.revisions
            .runtime
            .authorize(
                &context.principal,
                branch.workspace,
                Some(branch.id),
                Permission::Write,
            )
            .await?;
        let operation = match self
            .revisions
            .operations
            .start(
                context,
                "revision_commit",
                &request,
                Some(branch.workspace),
                Some(branch.id),
            )
            .await?
        {
            OperationStart::Existing(record) => return Ok(record),
            OperationStart::New(context) => context,
        };
        let save = SaveRevision {
            guard: request.guard,
            kind: request.kind,
            parents: None,
            replace_root: None,
            turn: None,
            session: None,
            result: SaveResult::Revision,
            durability: request.durability,
        };
        match self.revisions.save(operation.clone(), save).await {
            Ok(record) => Ok(record),
            Err(error) => self.revisions.operations.fail(&operation, error).await,
        }
    }

    async fn fork(&self, context: RequestContext, request: ForkBranch) -> Result<OperationRecord> {
        self.revisions
            .runtime
            .authorize(
                &context.principal,
                request.workspace,
                None,
                Permission::Write,
            )
            .await?;
        let operation = match self
            .revisions
            .operations
            .start(
                context,
                "branch_fork",
                &request,
                Some(request.workspace),
                None,
            )
            .await?
        {
            OperationStart::Existing(record) => return Ok(record),
            OperationStart::New(context) => context,
        };
        let result = {
            let _protection = self.revisions.runtime.objects.read().await;
            self.revisions
                .fork_locked(
                    operation.clone(),
                    request.workspace,
                    request.source,
                    request.name,
                    None,
                )
                .await
        };
        match result {
            Ok(record) => Ok(record),
            Err(error) => self.revisions.operations.fail(&operation, error).await,
        }
    }

    async fn restore(
        &self,
        context: RequestContext,
        request: RestoreBranch,
    ) -> Result<OperationRecord> {
        let branch = self.revisions.runtime.branch(request.guard.branch).await?;
        self.revisions
            .runtime
            .authorize(
                &context.principal,
                branch.workspace,
                Some(branch.id),
                Permission::Write,
            )
            .await?;
        let operation = match self
            .revisions
            .operations
            .start(
                context,
                "revision_restore",
                &request,
                Some(branch.workspace),
                Some(branch.id),
            )
            .await?
        {
            OperationStart::Existing(record) => return Ok(record),
            OperationStart::New(context) => context,
        };
        let result = self
            .transitions
            .replace_revision(
                operation.clone(),
                request.guard,
                request.revision,
                None,
                request.durability,
            )
            .await;
        match result {
            Ok(record) => Ok(record),
            Err(error) => self.revisions.operations.fail(&operation, error).await,
        }
    }

    async fn list(
        &self,
        actor: &Principal,
        workspace: WorkspaceId,
        branch: Option<BranchId>,
    ) -> Result<Vec<Revision>> {
        self.revisions
            .runtime
            .authorize(actor, workspace, branch, Permission::Read)
            .await?;
        let mut revisions: Vec<_> = self
            .revisions
            .runtime
            .state
            .revisions(workspace)
            .await?
            .into_iter()
            .filter(|revision| branch.is_none() || revision.branch == branch)
            .collect();
        revisions.sort_by_key(|revision| (revision.created_ns, revision.id));
        Ok(revisions)
    }

    async fn diff(
        &self,
        actor: &Principal,
        workspace: WorkspaceId,
        before: RevisionId,
        after: RevisionId,
    ) -> Result<Vec<FileDifference>> {
        self.revisions
            .runtime
            .authorize(actor, workspace, None, Permission::Read)
            .await?;
        let before = self.revisions.trees.revision(workspace, before).await?;
        let after = self.revisions.trees.revision(workspace, after).await?;
        let before = self.revisions.trees.tree(workspace, &before.root).await?;
        let after = self.revisions.trees.tree(workspace, &after.root).await?;
        let paths: BTreeSet<_> = before.keys().chain(after.keys()).cloned().collect();
        Ok(paths
            .into_iter()
            .filter_map(|path| {
                let left = before.get(&path);
                let right = after.get(&path);
                (!same_entry(left, right)).then(|| FileDifference {
                    path,
                    before: left.cloned(),
                    after: right.cloned(),
                })
            })
            .collect())
    }

    async fn read_file(
        &self,
        actor: &Principal,
        workspace: WorkspaceId,
        revision: RevisionId,
        path: WorkspacePath,
        offset: u64,
        size: u32,
    ) -> Result<bytes::Bytes> {
        self.revisions
            .runtime
            .authorize(actor, workspace, None, Permission::Read)
            .await?;
        let revision = self.revisions.trees.revision(workspace, revision).await?;
        let tree = self.revisions.trees.tree(workspace, &revision.root).await?;
        let inode = tree
            .iter()
            .find(|(entry, _)| entry.lookup_key() == path.lookup_key())
            .map(|(_, inode)| inode)
            .ok_or_else(|| Error::new(ErrorCode::NotFound, "file does not exist in revision"))?;
        if inode.kind != FileKind::File {
            return Err(Error::new(
                ErrorCode::IsDirectory,
                "revision path is a directory",
            ));
        }
        read_content(
            self.revisions.trees.content.as_ref(),
            workspace,
            inode
                .content
                .as_ref()
                .ok_or_else(|| Error::integrity("file content is missing"))?,
            offset,
            size,
        )
        .await
    }
}

pub(crate) fn same_entry(left: Option<&Inode>, right: Option<&Inode>) -> bool {
    match (left, right) {
        (None, None) => true,
        (Some(left), Some(right)) => {
            left.kind == right.kind
                && left.mode == right.mode
                && left.content == right.content
                && left.size == right.size
        }
        _ => false,
    }
}
