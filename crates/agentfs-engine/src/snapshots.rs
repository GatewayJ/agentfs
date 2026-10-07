use crate::{Runtime, TreeService};
use agentfs_model::*;
use agentfs_ports::*;
use async_trait::async_trait;
use std::sync::Arc;

pub struct SnapshotService {
    runtime: Arc<Runtime>,
    trees: Arc<TreeService>,
    working: Arc<dyn WorkingTreeStore>,
}

impl std::fmt::Debug for SnapshotService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SnapshotService").finish_non_exhaustive()
    }
}

impl SnapshotService {
    pub fn new(
        runtime: Arc<Runtime>,
        trees: Arc<TreeService>,
        working: Arc<dyn WorkingTreeStore>,
    ) -> Arc<Self> {
        Arc::new(Self {
            runtime,
            trees,
            working,
        })
    }

    /// Startup runs before native mounts or command transports are exposed.
    pub async fn recover(&self) -> Result<()> {
        for workspace in self.runtime.state.workspaces().await? {
            for branch in self.runtime.state.branches(workspace.id).await? {
                let _gate = self.runtime.lock_branch(branch.id).await;
                let tree = self.trees.tree(workspace.id, &branch.working_root).await?;
                let nodes = tree
                    .into_iter()
                    .map(|(path, inode)| Node {
                        path,
                        inode,
                        dirty: false,
                    })
                    .collect();
                let mut recovered = branch.clone();
                recovered.mutation_seq = recovered.saved_seq;
                let formal = self
                    .trees
                    .revision(workspace.id, branch.formal_head)
                    .await?;
                recovered.dirty = formal.root != recovered.working_root;
                self.runtime
                    .state
                    .commit(LocalCommit {
                        guards: vec![BranchGuard::from(&branch)],
                        branches: vec![recovered],
                        node_changes: vec![NodeChange::Replace {
                            branch: branch.id,
                            nodes,
                        }],
                        ..Default::default()
                    })
                    .await?;
                self.working.discard_branch(workspace.id, branch.id).await?;
            }
            let mut commit = LocalCommit::default();
            for mut session in self.runtime.state.sessions(workspace.id).await? {
                if session.state == SessionState::Active || session.active_turn.is_some() {
                    session.state = SessionState::RecoveryRequired;
                    commit.sessions.push(session);
                }
            }
            for mut operation in self.runtime.state.operations(workspace.id).await? {
                if matches!(
                    operation.phase,
                    OperationPhase::Reserved | OperationPhase::Saving
                ) && !operation.local_saved
                {
                    operation.phase =
                        if matches!(operation.pending, Some(PendingAction::Transfer { .. })) {
                            OperationPhase::RecoveryRequired
                        } else {
                            OperationPhase::Failed
                        };
                    operation.error = Some(Error::new(
                        ErrorCode::RecoveryRequired,
                        "operation was interrupted before its durable local commit",
                    ));
                    commit.operations.push(operation);
                }
            }
            self.runtime.state.commit(commit).await?;
        }
        let mut commit = LocalCommit::default();
        for mut mount in self.runtime.state.mounts().await? {
            if mount.state != MountState::Released {
                mount.state = MountState::RecoveryRequired;
                commit.mounts.push(mount);
            }
        }
        self.runtime.state.commit(commit).await
    }
}

#[async_trait]
impl SnapshotCapture for SnapshotService {
    async fn capture(&self, branch: &Branch) -> Result<CapturedTree> {
        let mut nodes = self.runtime.state.nodes(branch.id).await?;
        let mut tree = FileTree::new();
        for node in &mut nodes {
            if node.dirty && node.inode.kind == FileKind::File {
                let object = self
                    .trees
                    .content
                    .local
                    .seal(WorkingFile {
                        workspace: branch.workspace,
                        branch: branch.id,
                        inode: node.inode.id,
                    })
                    .await?;
                if object.size != node.inode.size {
                    return Err(Error::integrity(
                        "working file size differs from committed metadata",
                    ));
                }
                node.inode.content = Some(object);
            }
            node.dirty = false;
            tree.insert(node.path.clone(), node.inode.clone());
        }
        let root = self.trees.save_tree(branch.workspace, &tree).await?;
        Ok(CapturedTree {
            branch: branch.clone(),
            root,
            nodes,
            captured_seq: branch.mutation_seq,
        })
    }
}
