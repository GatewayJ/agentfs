use agentfs_model::*;
use async_trait::async_trait;
use bytes::Bytes;
use serde::{Deserialize, Serialize};
use std::{fmt, pin::Pin};
use tokio::io::AsyncRead;

pub type ByteStream = Pin<Box<dyn AsyncRead + Send + Unpin>>;

pub struct ObjectStream {
    pub reference: ObjectRef,
    pub reader: ByteStream,
}

impl fmt::Debug for ObjectStream {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ObjectStream")
            .field("reference", &self.reference)
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Debug, Default)]
pub struct LocalCommit {
    pub guards: Vec<BranchGuard>,
    pub new_workspaces: Vec<Workspace>,
    pub workspaces: Vec<Workspace>,
    pub new_branches: Vec<Branch>,
    pub branches: Vec<Branch>,
    pub node_changes: Vec<NodeChange>,
    pub revisions: Vec<Revision>,
    pub sessions: Vec<SessionBinding>,
    pub turns: Vec<TurnRecord>,
    pub operations: Vec<OperationRecord>,
    pub mounts: Vec<MountBinding>,
    pub merges: Vec<MergeCandidate>,
    pub sync_jobs: Vec<SyncJob>,
    pub completed_jobs: Vec<OperationId>,
    pub remote_refs: Vec<BranchRef>,
    pub pin_updates: Vec<PinUpdate>,
}

#[derive(Clone, Debug)]
pub enum NodeChange {
    Put {
        branch: BranchId,
        node: Node,
    },
    Remove {
        branch: BranchId,
        path: WorkspacePath,
    },
    Replace {
        branch: BranchId,
        nodes: Vec<Node>,
    },
}

#[derive(Clone, Debug)]
pub struct PinUpdate {
    pub pin: CachePin,
    pub enabled: bool,
}

#[derive(Clone, Debug)]
pub struct ReservedOperation {
    pub record: OperationRecord,
    pub created: bool,
}

#[async_trait]
pub trait LocalStateStore: Send + Sync {
    async fn location(&self) -> Result<LocationId>;
    async fn workspace(&self, id: WorkspaceId) -> Result<Option<Workspace>>;
    async fn workspaces(&self) -> Result<Vec<Workspace>>;
    async fn branch(&self, id: BranchId) -> Result<Option<Branch>>;
    async fn branches(&self, workspace: WorkspaceId) -> Result<Vec<Branch>>;
    async fn nodes(&self, branch: BranchId) -> Result<Vec<Node>>;
    async fn node(&self, branch: BranchId, inode: InodeId) -> Result<Option<Node>>;
    async fn node_at(&self, branch: BranchId, path: &WorkspacePath) -> Result<Option<Node>>;
    async fn revision(&self, id: RevisionId) -> Result<Option<Revision>>;
    async fn revisions(&self, workspace: WorkspaceId) -> Result<Vec<Revision>>;
    async fn session(&self, key: &SessionKey) -> Result<Option<SessionBinding>>;
    async fn sessions(&self, workspace: WorkspaceId) -> Result<Vec<SessionBinding>>;
    async fn turns(&self, workspace: WorkspaceId) -> Result<Vec<TurnRecord>>;
    async fn operation(&self, id: OperationId) -> Result<Option<OperationRecord>>;
    async fn operation_by_request(
        &self,
        actor: &Principal,
        request_id: &str,
    ) -> Result<Option<OperationRecord>>;
    async fn operations(&self, workspace: WorkspaceId) -> Result<Vec<OperationRecord>>;
    async fn reserve_operation(&self, record: OperationRecord) -> Result<ReservedOperation>;
    async fn mount(&self, id: MountId) -> Result<Option<MountBinding>>;
    async fn mounts(&self) -> Result<Vec<MountBinding>>;
    async fn merge(&self, id: MergeId) -> Result<Option<MergeCandidate>>;
    async fn merges(&self, workspace: WorkspaceId) -> Result<Vec<MergeCandidate>>;
    async fn sync_jobs(&self, workspace: WorkspaceId) -> Result<Vec<SyncJob>>;
    async fn remote_refs(&self, workspace: WorkspaceId) -> Result<Vec<BranchRef>>;
    async fn cache_pins(&self, workspace: WorkspaceId) -> Result<Vec<CachePin>>;
    async fn commit(&self, commit: LocalCommit) -> Result<()>;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct WorkingFile {
    pub workspace: WorkspaceId,
    pub branch: BranchId,
    pub inode: InodeId,
}

#[async_trait]
pub trait WorkingTreeStore: Send + Sync {
    async fn create(&self, file: WorkingFile, source: Option<ObjectStream>) -> Result<()>;
    async fn read(&self, file: WorkingFile, offset: u64, size: u32) -> Result<Bytes>;
    async fn write(&self, file: WorkingFile, offset: u64, data: Bytes) -> Result<u32>;
    async fn truncate(&self, file: WorkingFile, size: u64) -> Result<()>;
    async fn remove(&self, file: WorkingFile) -> Result<()>;
    async fn discard_branch(&self, workspace: WorkspaceId, branch: BranchId) -> Result<()>;
}

#[derive(Clone, Debug)]
pub struct CachedObject {
    pub workspace: WorkspaceId,
    pub reference: ObjectRef,
    pub remote_confirmed: bool,
    pub last_access_ns: i64,
}

#[async_trait]
pub trait LocalObjectStore: Send + Sync {
    async fn ingest(
        &self,
        workspace: WorkspaceId,
        kind: ObjectKind,
        reader: ByteStream,
    ) -> Result<ObjectRef>;
    async fn put(&self, workspace: WorkspaceId, kind: ObjectKind, data: Bytes)
    -> Result<ObjectRef>;
    async fn seal(&self, file: WorkingFile) -> Result<ObjectRef>;
    async fn install(&self, workspace: WorkspaceId, stream: ObjectStream) -> Result<()>;
    async fn open(&self, workspace: WorkspaceId, object: &ObjectRef) -> Result<ObjectStream>;
    async fn read_range(
        &self,
        workspace: WorkspaceId,
        object: &ObjectRef,
        offset: u64,
        size: u32,
    ) -> Result<Bytes>;
    async fn contains(&self, workspace: WorkspaceId, object: &ObjectRef) -> Result<bool>;
    async fn confirm_remote(&self, workspace: WorkspaceId, object: &ObjectRef) -> Result<()>;
    async fn objects(&self, workspace: WorkspaceId) -> Result<Vec<CachedObject>>;
    async fn evict(&self, workspace: WorkspaceId, object: &ObjectRef) -> Result<bool>;
    async fn stats(&self) -> Result<StorageStats>;
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum RefKey {
    Workspace(WorkspaceId),
    Branch {
        workspace: WorkspaceId,
        branch: BranchId,
    },
}

#[derive(Clone, Debug)]
pub struct VersionedRef {
    pub version: String,
    pub bytes: Bytes,
}

#[derive(Clone, Debug)]
pub struct RefUpdate {
    pub key: RefKey,
    pub expected_version: Option<String>,
    pub bytes: Bytes,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CasOutcome {
    Applied { version: String },
    Conflict,
    Unknown,
}

#[async_trait]
pub trait RemoteObjectStore: Send + Sync {
    async fn upload(&self, workspace: WorkspaceId, source: ObjectStream) -> Result<()>;
    async fn download(&self, workspace: WorkspaceId, object: &ObjectRef) -> Result<ObjectStream>;
    async fn contains(&self, workspace: WorkspaceId, object: &ObjectRef) -> Result<bool>;
}

#[async_trait]
pub trait RemoteRefStore: Send + Sync {
    async fn get(&self, key: &RefKey) -> Result<Option<VersionedRef>>;
    async fn compare_exchange(&self, update: RefUpdate) -> Result<CasOutcome>;
    async fn list_branches(&self, workspace: WorkspaceId) -> Result<Vec<BranchId>>;
}
