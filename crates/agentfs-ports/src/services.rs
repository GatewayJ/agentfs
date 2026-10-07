use crate::{ObjectStream, RequestContext};
use agentfs_model::*;
use async_trait::async_trait;
use bytes::Bytes;

#[derive(Clone, Debug)]
pub struct OperationContext {
    pub operation: OperationRecord,
}

#[derive(Clone, Debug)]
pub enum OperationStart {
    New(OperationContext),
    Existing(OperationRecord),
}

#[async_trait]
pub trait OperationCoordinator: Send + Sync {
    async fn begin(
        &self,
        context: RequestContext,
        name: &str,
        fingerprint: ObjectId,
        workspace: Option<WorkspaceId>,
        branch: Option<BranchId>,
    ) -> Result<OperationStart>;
    async fn fail(&self, context: &OperationContext, error: Error) -> Result<OperationRecord>;
}

#[async_trait]
pub trait Authorization: Send + Sync {
    async fn authorize(
        &self,
        actor: &Principal,
        workspace: WorkspaceId,
        branch: Option<BranchId>,
        permission: Permission,
    ) -> Result<Workspace>;
}

#[async_trait]
pub trait ContentResolver: Send + Sync {
    async fn open(&self, workspace: WorkspaceId, reference: &ObjectRef) -> Result<ObjectStream>;
    async fn read_range(
        &self,
        workspace: WorkspaceId,
        reference: &ObjectRef,
        offset: u64,
        size: u32,
    ) -> Result<Bytes>;
    async fn metadata(&self, workspace: WorkspaceId, reference: &ObjectRef) -> Result<Bytes>;
    async fn protect_remote(
        &self,
        workspace: WorkspaceId,
        operation: OperationId,
        roots: Vec<ObjectRef>,
    ) -> Result<()>;
    async fn release_remote(&self, workspace: WorkspaceId, operation: OperationId) -> Result<()>;
}

#[async_trait]
pub trait RevisionReader: Send + Sync {
    async fn revision(&self, workspace: WorkspaceId, revision: RevisionId) -> Result<Revision>;
    async fn tree(&self, workspace: WorkspaceId, root: &ObjectRef) -> Result<FileTree>;
    async fn save_tree(&self, workspace: WorkspaceId, tree: &FileTree) -> Result<ObjectRef>;
}

#[derive(Clone, Debug)]
pub struct CapturedTree {
    pub branch: Branch,
    pub root: ObjectRef,
    pub nodes: Vec<Node>,
    pub captured_seq: u64,
}

/// The caller owns the branch mutation barrier until its local commit completes.
#[async_trait]
pub trait SnapshotCapture: Send + Sync {
    async fn capture(&self, branch: &Branch) -> Result<CapturedTree>;
}

#[derive(Clone, Debug)]
pub struct SaveRevision {
    pub guard: BranchGuard,
    pub kind: RevisionKind,
    pub parents: Option<Vec<RevisionId>>,
    pub replace_root: Option<ObjectRef>,
    pub turn: Option<TurnRecord>,
    pub session: Option<SessionBinding>,
    pub durability: Durability,
    pub result: SaveResult,
}

#[derive(Clone, Copy, Debug)]
pub enum SaveResult {
    Revision,
    Session,
    Turn,
}

#[async_trait]
pub trait RevisionWriter: Send + Sync {
    async fn save(
        &self,
        context: OperationContext,
        request: SaveRevision,
    ) -> Result<OperationRecord>;
}

#[async_trait]
pub trait RevisionTransition: Send + Sync {
    async fn replace_revision(
        &self,
        context: OperationContext,
        target: BranchGuard,
        revision: RevisionId,
        merge: Option<MergeId>,
        durability: Durability,
    ) -> Result<OperationRecord>;
}

#[async_trait]
pub trait ReplicaPublisher: Send + Sync {
    async fn publish_workspace(&self, workspace: WorkspaceId) -> Result<()>;
    async fn drain(&self, workspace: WorkspaceId, branch: Option<BranchId>) -> Result<()>;
    async fn build_job(
        &self,
        branch: &Branch,
        new_revision: Option<&Revision>,
        operation: &OperationRecord,
        turn: Option<&TurnRecord>,
    ) -> Result<SyncJob>;
    async fn discover(&self, workspace: WorkspaceId) -> Result<Vec<BranchRef>>;
}

#[async_trait]
pub trait DurabilityService: Send + Sync {
    async fn flush(
        &self,
        branch: BranchId,
        actor: Principal,
        durability: Durability,
    ) -> Result<crate::DurabilityReceipt>;
}

#[async_trait]
pub trait WorkingTree: Send + Sync {
    async fn replace(&self, branch: BranchId, root: &ObjectRef) -> Result<Vec<Node>>;
    fn open_handles(&self, mount: MountId) -> usize;
    fn branch_dirty_time(&self, branch: BranchId) -> Option<(u64, u64)>;
    fn clear_dirty_time(&self, branch: BranchId, through_seq: u64);
}
