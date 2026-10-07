use agentfs_model::*;
use async_trait::async_trait;
use bytes::Bytes;
use serde::{Deserialize, Serialize};
use std::{path::PathBuf, sync::Arc};

#[derive(Clone, Debug)]
pub struct RequestContext {
    pub principal: Principal,
    pub request_id: String,
}

impl RequestContext {
    pub fn validate(&self) -> Result<()> {
        if self.request_id.is_empty()
            || self.request_id.len() > 256
            || self.request_id.chars().any(char::is_control)
        {
            return Err(Error::invalid(
                "request_id must be a nonempty identifier of at most 256 bytes",
            ));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CreateWorkspace {
    pub name: String,
    #[serde(default)]
    pub grants: Vec<Grant>,
    pub max_file_bytes: Option<u64>,
    pub max_working_bytes: Option<u64>,
    pub max_inodes: Option<u64>,
}

#[derive(Clone, Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub struct Capabilities {
    pub format_version: u32,
    pub platform_mount: String,
    pub file_operations: Vec<String>,
    pub remote_configured: bool,
    pub automatic_text_merge: bool,
    pub physical_remote_gc: bool,
    pub single_location_session: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub struct WorkspaceStatus {
    pub workspace: Workspace,
    pub location: LocationId,
    pub branches: Vec<Branch>,
    pub remote_branches: Vec<BranchRef>,
    pub mounts: Vec<MountBinding>,
    pub pending_jobs: usize,
    pub capabilities: Capabilities,
    pub storage: StorageStats,
}

#[derive(Clone, Debug, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CommitRevision {
    pub guard: BranchGuard,
    pub kind: RevisionKind,
    #[serde(default)]
    pub durability: Durability,
}

#[derive(Clone, Debug, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ForkBranch {
    pub workspace: WorkspaceId,
    pub source: RevisionId,
    pub name: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RestoreBranch {
    pub guard: BranchGuard,
    pub revision: RevisionId,
    #[serde(default)]
    pub durability: Durability,
}

#[derive(Clone, Debug, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ImportDirectory {
    pub workspace: WorkspaceId,
    pub source: PathBuf,
}

#[derive(Clone, Debug, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ExportRevision {
    pub workspace: WorkspaceId,
    pub revision: RevisionId,
    pub destination: PathBuf,
    pub paths: Option<Vec<WorkspacePath>>,
}

#[derive(Clone, Debug, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct OpenSession {
    pub key: SessionKey,
    pub source_revision: Option<RevisionId>,
    pub source_branch: Option<BranchId>,
    pub mount_path: Option<PathBuf>,
    #[serde(default)]
    pub durability: Durability,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RecoveryAction {
    Continue,
    Interrupt,
}

#[derive(Clone, Debug, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ResumeSession {
    pub key: SessionKey,
    pub mount_path: Option<PathBuf>,
    pub recovery_action: Option<RecoveryAction>,
    pub turn_id: Option<String>,
    #[serde(default)]
    pub durability: Durability,
}

#[derive(Clone, Debug, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SessionAction {
    pub key: SessionKey,
    #[serde(default)]
    pub durability: Durability,
}

#[derive(Clone, Debug, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BeginTurn {
    pub key: SessionKey,
    pub turn_id: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct EndTurn {
    pub key: SessionKey,
    pub turn_id: String,
    pub status: TurnStatus,
    #[serde(default)]
    pub durability: Durability,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SyncDirection {
    Push,
    Pull,
    Both,
}

#[derive(Clone, Debug, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SyncRequest {
    pub workspace: WorkspaceId,
    pub branches: Option<Vec<BranchId>>,
    pub direction: SyncDirection,
}

#[derive(Clone, Debug, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RevisionRange {
    pub workspace: WorkspaceId,
    pub revision: RevisionId,
    pub paths: Option<Vec<WorkspacePath>>,
}

#[derive(Clone, Debug, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PinRequest {
    pub range: RevisionRange,
    pub enabled: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TransferOwnership {
    pub guard: BranchGuard,
    pub target: LocationId,
}

#[derive(Clone, Debug, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PrepareMerge {
    pub workspace: WorkspaceId,
    pub source: RevisionId,
    pub target: BranchGuard,
    pub base: Option<RevisionId>,
}

#[derive(Clone, Debug, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(tag = "choice", rename_all = "snake_case")]
pub enum Resolution {
    Source {
        path: WorkspacePath,
    },
    Target {
        path: WorkspacePath,
    },
    Delete {
        path: WorkspacePath,
    },
    Replace {
        path: WorkspacePath,
        content: Vec<u8>,
        mode: u32,
    },
}

impl Resolution {
    pub fn path(&self) -> &WorkspacePath {
        match self {
            Self::Source { path }
            | Self::Target { path }
            | Self::Delete { path }
            | Self::Replace { path, .. } => path,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ResolveMerge {
    pub merge: MergeId,
    pub candidate: RevisionId,
    pub resolutions: Vec<Resolution>,
}

#[derive(Clone, Debug, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ValidateMerge {
    pub merge: MergeId,
    pub candidate: RevisionId,
    pub configuration: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ApplyMerge {
    pub merge: MergeId,
    pub candidate: RevisionId,
    pub target: BranchGuard,
    #[serde(default)]
    pub durability: Durability,
}

#[derive(Clone, Debug, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PrepareMount {
    pub workspace: WorkspaceId,
    pub branch: BranchId,
    pub revision: Option<RevisionId>,
    pub path: PathBuf,
    pub access: AccessMode,
    #[serde(default)]
    pub durability: Durability,
}

#[derive(Clone, Debug, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ReleaseMount {
    pub operation: OperationId,
    pub expected_binding_generation: u64,
}

#[async_trait]
pub trait WorkspaceApi: Send + Sync {
    async fn create(
        &self,
        context: RequestContext,
        request: CreateWorkspace,
    ) -> Result<OperationRecord>;
    async fn list(&self, actor: &Principal) -> Result<Vec<Workspace>>;
    async fn status(&self, actor: &Principal, workspace: WorkspaceId) -> Result<WorkspaceStatus>;
    async fn import_directory(
        &self,
        context: RequestContext,
        request: ImportDirectory,
    ) -> Result<OperationRecord>;
    async fn export_revision(
        &self,
        context: RequestContext,
        request: ExportRevision,
    ) -> Result<OperationRecord>;
}

#[async_trait]
pub trait RevisionApi: Send + Sync {
    async fn commit(
        &self,
        context: RequestContext,
        request: CommitRevision,
    ) -> Result<OperationRecord>;
    async fn fork(&self, context: RequestContext, request: ForkBranch) -> Result<OperationRecord>;
    async fn restore(
        &self,
        context: RequestContext,
        request: RestoreBranch,
    ) -> Result<OperationRecord>;
    async fn list(
        &self,
        actor: &Principal,
        workspace: WorkspaceId,
        branch: Option<BranchId>,
    ) -> Result<Vec<Revision>>;
    async fn diff(
        &self,
        actor: &Principal,
        workspace: WorkspaceId,
        before: RevisionId,
        after: RevisionId,
    ) -> Result<Vec<FileDifference>>;
    async fn read_file(
        &self,
        actor: &Principal,
        workspace: WorkspaceId,
        revision: RevisionId,
        path: WorkspacePath,
        offset: u64,
        size: u32,
    ) -> Result<Bytes>;
}

#[async_trait]
pub trait SessionApi: Send + Sync {
    async fn open(&self, context: RequestContext, request: OpenSession) -> Result<OperationRecord>;
    async fn resume(
        &self,
        context: RequestContext,
        request: ResumeSession,
    ) -> Result<OperationRecord>;
    async fn pause(
        &self,
        context: RequestContext,
        request: SessionAction,
    ) -> Result<OperationRecord>;
    async fn close(
        &self,
        context: RequestContext,
        request: SessionAction,
    ) -> Result<OperationRecord>;
    async fn turn_begin(
        &self,
        context: RequestContext,
        request: BeginTurn,
    ) -> Result<OperationRecord>;
    async fn turn_end(&self, context: RequestContext, request: EndTurn) -> Result<OperationRecord>;
}

#[async_trait]
pub trait ReplicaApi: Send + Sync {
    async fn sync(&self, context: RequestContext, request: SyncRequest) -> Result<OperationRecord>;
}

#[async_trait]
pub trait CacheApi: Send + Sync {
    async fn prefetch(
        &self,
        context: RequestContext,
        request: RevisionRange,
    ) -> Result<OperationRecord>;
    async fn pin(&self, context: RequestContext, request: PinRequest) -> Result<OperationRecord>;
    async fn status(&self, actor: &Principal, request: RevisionRange) -> Result<CacheStatus>;
    async fn collect(&self) -> Result<u64>;
}

#[async_trait]
pub trait OwnershipApi: Send + Sync {
    async fn transfer(
        &self,
        context: RequestContext,
        request: TransferOwnership,
    ) -> Result<OperationRecord>;
}

#[async_trait]
pub trait MergeApi: Send + Sync {
    async fn prepare(
        &self,
        context: RequestContext,
        request: PrepareMerge,
    ) -> Result<OperationRecord>;
    async fn get(&self, actor: &Principal, merge: MergeId) -> Result<MergeCandidate>;
    async fn resolve(
        &self,
        context: RequestContext,
        request: ResolveMerge,
    ) -> Result<OperationRecord>;
    async fn validate(
        &self,
        context: RequestContext,
        request: ValidateMerge,
    ) -> Result<OperationRecord>;
    async fn apply(&self, context: RequestContext, request: ApplyMerge) -> Result<OperationRecord>;
}

#[async_trait]
pub trait MountApi: Send + Sync {
    async fn prepare(
        &self,
        context: RequestContext,
        request: PrepareMount,
    ) -> Result<OperationRecord>;
    async fn release(
        &self,
        context: RequestContext,
        request: ReleaseMount,
    ) -> Result<OperationRecord>;
    async fn unmount(
        &self,
        context: RequestContext,
        mount: MountId,
        generation: u64,
    ) -> Result<OperationRecord>;
}

#[async_trait]
pub trait OperationApi: Send + Sync {
    async fn get(&self, actor: &Principal, operation: OperationId) -> Result<OperationRecord>;
    async fn by_request(&self, actor: &Principal, request: &str) -> Result<OperationRecord>;
    async fn cancel(&self, actor: &Principal, operation: OperationId) -> Result<OperationRecord>;
}

#[derive(Clone)]
pub struct Services {
    pub workspace: Arc<dyn WorkspaceApi>,
    pub revisions: Arc<dyn RevisionApi>,
    pub sessions: Arc<dyn SessionApi>,
    pub replica: Arc<dyn ReplicaApi>,
    pub cache: Arc<dyn CacheApi>,
    pub ownership: Arc<dyn OwnershipApi>,
    pub merge: Arc<dyn MergeApi>,
    pub mounts: Arc<dyn MountApi>,
    pub operations: Arc<dyn OperationApi>,
    pub files: Arc<dyn crate::FileSystem>,
}

impl std::fmt::Debug for Services {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Services").finish_non_exhaustive()
    }
}
