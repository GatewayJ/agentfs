use crate::{
    OperationService, RevisionService, Runtime, revisions::validate_label, runtime::increment,
};
use agentfs_model::*;
use agentfs_ports::*;
use async_trait::async_trait;
use std::sync::Arc;

pub struct WorkspaceService {
    runtime: Arc<Runtime>,
    revisions: Arc<RevisionService>,
    operations: Arc<OperationService>,
    directories: Arc<dyn DirectoryAccess>,
    mount_capability: String,
}

impl std::fmt::Debug for WorkspaceService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WorkspaceService").finish_non_exhaustive()
    }
}

impl WorkspaceService {
    pub fn new(
        runtime: Arc<Runtime>,
        revisions: Arc<RevisionService>,
        operations: Arc<OperationService>,
        directories: Arc<dyn DirectoryAccess>,
        mount_capability: String,
    ) -> Arc<Self> {
        Arc::new(Self {
            runtime,
            revisions,
            operations,
            directories,
            mount_capability,
        })
    }

    async fn create_workspace(
        &self,
        context: OperationContext,
        request: CreateWorkspace,
    ) -> Result<OperationRecord> {
        validate_label(&request.name, "workspace name")?;
        let _protection = self.runtime.objects.read().await;
        let now = self.runtime.clock.now_ns();
        let id = WorkspaceId::new();
        let initial_revision = RevisionId::new();
        let workspace = Workspace {
            id,
            name: request.name,
            owner: context.operation.principal.clone(),
            initial_revision,
            format_version: FORMAT_VERSION,
            name_policy: "portable_v1".into(),
            grants: request.grants,
            max_file_bytes: request.max_file_bytes.unwrap_or(64 * 1024 * 1024 * 1024),
            max_working_bytes: request
                .max_working_bytes
                .unwrap_or(256 * 1024 * 1024 * 1024),
            max_inodes: request.max_inodes.unwrap_or(1_000_000),
        };
        if workspace.max_inodes == 0
            || workspace.max_inodes > 1_000_000
            || workspace.max_file_bytes == 0
            || workspace.max_file_bytes > workspace.max_working_bytes
            || workspace.max_working_bytes > i64::MAX as u64
        {
            return Err(Error::invalid("invalid workspace capacity limits"));
        }
        let inode = Inode {
            id: InodeId::ROOT,
            kind: FileKind::Directory,
            mode: 0o755,
            uid: 0,
            gid: 0,
            size: 0,
            created_ns: now,
            modified_ns: now,
            content: None,
        };
        let tree = FileTree::from([(WorkspacePath::root(), inode.clone())]);
        let root = self.revisions.trees.save_tree(id, &tree).await?;
        let branch_id = BranchId::new();
        let revision = Revision {
            id: initial_revision,
            workspace: id,
            branch: Some(branch_id),
            parents: vec![],
            root: root.clone(),
            kind: RevisionKind::Commit,
            actor: workspace.owner.clone(),
            created_ns: now,
            operation: context.operation.id,
        };
        let branch = Branch {
            id: branch_id,
            workspace: id,
            name: "main".into(),
            owner: self.runtime.location,
            authority_epoch: 1,
            generation: 1,
            mutation_seq: 0,
            saved_seq: 0,
            source_generation: 1,
            formal_head: initial_revision,
            autosave_head: None,
            working_root: root,
            base_revision: initial_revision,
            next_inode: 2,
            dirty: false,
            state: BranchState::Writable,
            protected: true,
        };
        let mut operation = context.operation;
        operation.workspace = Some(id);
        operation.branch = Some(branch_id);
        operation.local_saved = true;
        operation.phase = OperationPhase::LocalSaved;
        operation.result = Some(CommandResult::Workspace {
            workspace: workspace.clone(),
        });
        let job = self
            .revisions
            .publisher
            .build_job(&branch, Some(&revision), &operation, None)
            .await?;
        self.runtime
            .state
            .commit(LocalCommit {
                new_workspaces: vec![workspace],
                new_branches: vec![branch],
                revisions: vec![revision],
                operations: vec![operation.clone()],
                sync_jobs: vec![job],
                node_changes: vec![NodeChange::Replace {
                    branch: branch_id,
                    nodes: vec![Node {
                        path: WorkspacePath::root(),
                        inode,
                        dirty: false,
                    }],
                }],
                ..Default::default()
            })
            .await?;
        Ok(operation)
    }
}

#[async_trait]
impl WorkspaceApi for WorkspaceService {
    async fn create(
        &self,
        context: RequestContext,
        request: CreateWorkspace,
    ) -> Result<OperationRecord> {
        let operation = match self
            .operations
            .start(context, "workspace_create", &request, None, None)
            .await?
        {
            OperationStart::Existing(record) => return Ok(record),
            OperationStart::New(context) => context,
        };
        match self.create_workspace(operation.clone(), request).await {
            Ok(record) => Ok(record),
            Err(error) => self.operations.fail(&operation, error).await,
        }
    }

    async fn list(&self, actor: &Principal) -> Result<Vec<Workspace>> {
        Ok(self
            .runtime
            .state
            .workspaces()
            .await?
            .into_iter()
            .filter(|workspace| workspace.authorize(actor, None, Permission::Read).is_ok())
            .collect())
    }

    async fn status(&self, actor: &Principal, workspace: WorkspaceId) -> Result<WorkspaceStatus> {
        let workspace = self
            .runtime
            .authorize(actor, workspace, None, Permission::Read)
            .await?;
        Ok(WorkspaceStatus {
            location: self.runtime.location,
            branches: self.runtime.state.branches(workspace.id).await?,
            remote_branches: self.runtime.state.remote_refs(workspace.id).await?,
            mounts: self
                .runtime
                .state
                .mounts()
                .await?
                .into_iter()
                .filter(|mount| mount.workspace == workspace.id)
                .collect(),
            pending_jobs: self.runtime.state.sync_jobs(workspace.id).await?.len(),
            capabilities: Capabilities {
                format_version: FORMAT_VERSION,
                platform_mount: self.mount_capability.clone(),
                file_operations: [
                    "lookup", "getattr", "readdir", "create", "mkdir", "read", "write", "truncate",
                    "chmod", "rename", "unlink", "rmdir", "fsync",
                ]
                .map(str::to_owned)
                .to_vec(),
                remote_configured: self.revisions.trees.content.remote.is_some(),
                automatic_text_merge: false,
                physical_remote_gc: false,
                single_location_session: false,
            },
            storage: self.revisions.trees.content.local.stats().await?,
            workspace,
        })
    }

    async fn import_directory(
        &self,
        context: RequestContext,
        request: ImportDirectory,
    ) -> Result<OperationRecord> {
        let workspace = self
            .runtime
            .authorize(
                &context.principal,
                request.workspace,
                None,
                Permission::Write,
            )
            .await?;
        let operation = match self
            .operations
            .start(
                context,
                "workspace_import",
                &request,
                Some(workspace.id),
                None,
            )
            .await?
        {
            OperationStart::Existing(record) => return Ok(record),
            OperationStart::New(context) => context,
        };
        let result = async {
            let _protection = self.runtime.objects.read().await;
            let tree = self
                .directories
                .import_tree(
                    &workspace,
                    &request.source,
                    self.revisions.trees.content.local.clone(),
                )
                .await?;
            let root = self.revisions.trees.save_tree(workspace.id, &tree).await?;
            let revision_id = RevisionId::new();
            let branch_id = BranchId::new();
            let revision = Revision {
                id: revision_id,
                workspace: workspace.id,
                branch: Some(branch_id),
                parents: vec![workspace.initial_revision],
                root: root.clone(),
                kind: RevisionKind::Commit,
                actor: operation.operation.principal.clone(),
                created_ns: self.runtime.clock.now_ns(),
                operation: operation.operation.id,
            };
            let branch = Branch {
                id: branch_id,
                workspace: workspace.id,
                name: format!("import-{branch_id}"),
                owner: self.runtime.location,
                authority_epoch: 1,
                generation: 1,
                mutation_seq: 0,
                saved_seq: 0,
                source_generation: 1,
                formal_head: revision_id,
                autosave_head: None,
                working_root: root,
                base_revision: workspace.initial_revision,
                next_inode: increment(tree.values().map(|inode| inode.id.0).max().unwrap_or(1))?,
                dirty: false,
                state: BranchState::Writable,
                protected: false,
            };
            let mut record = operation.operation.clone();
            record.branch = Some(branch_id);
            record.phase = OperationPhase::LocalSaved;
            record.local_saved = true;
            record.result = Some(CommandResult::Branch {
                branch: branch.clone(),
            });
            let job = self
                .revisions
                .publisher
                .build_job(&branch, Some(&revision), &record, None)
                .await?;
            self.runtime
                .state
                .commit(LocalCommit {
                    new_branches: vec![branch],
                    revisions: vec![revision],
                    operations: vec![record.clone()],
                    sync_jobs: vec![job],
                    node_changes: vec![NodeChange::Replace {
                        branch: branch_id,
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
            Ok(record)
        }
        .await;
        match result {
            Ok(record) => Ok(record),
            Err(error) => self.operations.fail(&operation, error).await,
        }
    }

    async fn export_revision(
        &self,
        context: RequestContext,
        request: ExportRevision,
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
                "revision_export",
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
                .revisions
                .trees
                .revision(request.workspace, request.revision)
                .await?;
            let mut tree = self
                .revisions
                .trees
                .tree(request.workspace, &revision.root)
                .await?;
            if let Some(paths) = request.paths {
                if paths.is_empty() {
                    return Err(Error::invalid("export paths must not be empty"));
                }
                for path in &paths {
                    if !tree
                        .keys()
                        .any(|existing| existing.lookup_key() == path.lookup_key())
                    {
                        return Err(Error::new(
                            ErrorCode::NotFound,
                            "export path does not exist",
                        ));
                    }
                }
                tree.retain(|path, _| {
                    paths
                        .iter()
                        .any(|selected| selected.contains(path) || path.contains(selected))
                });
            }
            self.directories
                .export_tree(
                    request.workspace,
                    tree,
                    &request.destination,
                    self.revisions.trees.content.clone(),
                )
                .await?;
            self.operations
                .finish(
                    operation.clone(),
                    CommandResult::Updated,
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
