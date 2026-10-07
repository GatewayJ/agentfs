use crate::{
    BranchId, Error, ErrorCode, HandleId, InodeId, LocationId, MergeId, MountId, ObjectId,
    ObjectRef, OperationId, Result, RevisionId, WorkspaceId, WorkspacePath,
};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, path::PathBuf};

#[derive(
    Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(try_from = "String", into = "String")]
pub struct Principal(String);

impl Principal {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}
impl TryFrom<String> for Principal {
    type Error = Error;
    fn try_from(value: String) -> Result<Self> {
        if value.is_empty() || value.len() > 256 || value.chars().any(char::is_control) {
            return Err(Error::invalid("invalid principal"));
        }
        Ok(Self(value))
    }
}
impl From<Principal> for String {
    fn from(value: Principal) -> Self {
        value.0
    }
}

#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum Permission {
    Read,
    Write,
    Merge,
    Manage,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct Grant {
    pub principal: Principal,
    pub branch: Option<BranchId>,
    pub permissions: Vec<Permission>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct Workspace {
    pub id: WorkspaceId,
    pub name: String,
    pub owner: Principal,
    pub initial_revision: RevisionId,
    pub format_version: u32,
    pub name_policy: String,
    pub grants: Vec<Grant>,
    pub max_file_bytes: u64,
    pub max_working_bytes: u64,
    pub max_inodes: u64,
}

impl Workspace {
    pub fn authorize(
        &self,
        actor: &Principal,
        branch: Option<BranchId>,
        permission: Permission,
    ) -> Result<()> {
        if actor == &self.owner
            || self.grants.iter().any(|grant| {
                grant.principal == *actor
                    && (grant.branch.is_none() || grant.branch == branch)
                    && (grant.permissions.contains(&permission)
                        || grant.permissions.contains(&Permission::Manage))
            })
        {
            return Ok(());
        }
        Err(Error::new(
            ErrorCode::PermissionDenied,
            "principal is not authorized for this operation",
        ))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum FileKind {
    File,
    Directory,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct Inode {
    pub id: InodeId,
    pub kind: FileKind,
    pub mode: u32,
    pub uid: u32,
    pub gid: u32,
    pub size: u64,
    pub created_ns: i64,
    pub modified_ns: i64,
    pub content: Option<ObjectRef>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct Node {
    pub path: WorkspacePath,
    pub inode: Inode,
    pub dirty: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum BranchState {
    Writable,
    Stopped,
    Deleted,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct Branch {
    pub id: BranchId,
    pub workspace: WorkspaceId,
    pub name: String,
    pub owner: LocationId,
    pub authority_epoch: u64,
    pub generation: u64,
    pub mutation_seq: u64,
    pub saved_seq: u64,
    pub source_generation: u64,
    pub formal_head: RevisionId,
    pub autosave_head: Option<RevisionId>,
    pub working_root: ObjectRef,
    pub base_revision: RevisionId,
    pub next_inode: u64,
    pub dirty: bool,
    pub state: BranchState,
    pub protected: bool,
}

impl Branch {
    pub fn check_write(&self, location: LocationId, epoch: u64) -> Result<()> {
        if self.owner != location || self.authority_epoch != epoch {
            return Err(Error::new(
                ErrorCode::StaleAuthority,
                "branch write authority has changed",
            ));
        }
        if self.state != BranchState::Writable || self.protected {
            return Err(Error::new(
                ErrorCode::ReadOnly,
                "branch does not accept direct writes",
            ));
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct BranchGuard {
    pub branch: BranchId,
    pub authority_epoch: u64,
    pub generation: u64,
    pub expected_head: RevisionId,
    pub mutation_seq: Option<u64>,
}

impl From<&Branch> for BranchGuard {
    fn from(branch: &Branch) -> Self {
        Self {
            branch: branch.id,
            authority_epoch: branch.authority_epoch,
            generation: branch.generation,
            expected_head: branch.formal_head,
            mutation_seq: Some(branch.mutation_seq),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RevisionKind {
    Commit,
    Autosave,
    Checkpoint,
    Candidate,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct Revision {
    pub id: RevisionId,
    pub workspace: WorkspaceId,
    pub branch: Option<BranchId>,
    pub parents: Vec<RevisionId>,
    pub root: ObjectRef,
    pub kind: RevisionKind,
    pub actor: Principal,
    pub created_ns: i64,
    pub operation: OperationId,
}

#[derive(
    Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, schemars::JsonSchema,
)]
pub struct SessionKey {
    pub workspace: WorkspaceId,
    pub app_namespace: String,
    pub session_id: String,
    pub location: LocationId,
}

impl SessionKey {
    pub fn validate(&self) -> Result<()> {
        for value in [&self.app_namespace, &self.session_id] {
            if value.is_empty() || value.len() > 512 || value.chars().any(char::is_control) {
                return Err(Error::invalid("invalid session identity"));
            }
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SessionState {
    Active,
    Paused,
    Closed,
    RecoveryRequired,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct SessionBinding {
    pub key: SessionKey,
    pub principal: Principal,
    pub branch: BranchId,
    pub mount: Option<MountId>,
    pub state: SessionState,
    pub active_turn: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum TurnStatus {
    Running,
    Paused,
    Completed,
    Failed,
    Cancelled,
    Interrupted,
}

impl TurnStatus {
    pub fn is_end(self) -> bool {
        matches!(
            self,
            Self::Completed | Self::Failed | Self::Cancelled | Self::Interrupted
        )
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct TurnRecord {
    pub session: SessionKey,
    pub turn_id: String,
    pub begin_revision: RevisionId,
    pub result_revision: Option<RevisionId>,
    pub status: TurnStatus,
    pub operation: OperationId,
}

#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum Durability {
    #[default]
    Local,
    Remote,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum AccessMode {
    ReadWrite,
    ReadOnly,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum MountState {
    Preparing,
    Ready,
    Paused,
    Released,
    RecoveryRequired,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct MountBinding {
    pub id: MountId,
    pub workspace: WorkspaceId,
    pub branch: BranchId,
    pub revision: Option<RevisionId>,
    pub generation: u64,
    pub path: PathBuf,
    pub principal: Principal,
    pub identity: LocalIdentity,
    pub durability: Durability,
    pub access: AccessMode,
    pub state: MountState,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct LocalIdentity {
    pub uid: u32,
    pub gid: u32,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct FsContext {
    pub mount: MountId,
    pub generation: u64,
    pub identity: LocalIdentity,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct OpenMode {
    pub read: bool,
    pub write: bool,
    pub append: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct FileHandle {
    pub id: HandleId,
    pub inode: InodeId,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum OperationPhase {
    Reserved,
    WaitingForQuiesce,
    Saving,
    LocalSaved,
    Uploading,
    CommittedMountPending,
    Complete,
    Cancelled,
    RecoveryRequired,
    Failed,
}

impl OperationPhase {
    pub fn terminal(self) -> bool {
        matches!(self, Self::Complete | Self::Cancelled | Self::Failed)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CommandResult {
    Operation {
        operation: OperationId,
    },
    Workspace {
        workspace: Workspace,
    },
    Branch {
        branch: Branch,
    },
    Revision {
        revision: RevisionId,
        branch: BranchId,
    },
    Session {
        binding: SessionBinding,
    },
    Turn {
        record: TurnRecord,
    },
    Mount {
        binding: MountBinding,
    },
    Merge {
        candidate: MergeCandidate,
    },
    Prefetch {
        status: CacheStatus,
    },
    Synced {
        branches: Vec<BranchId>,
    },
    Ownership {
        branch: BranchId,
        owner: LocationId,
        authority_epoch: u64,
    },
    Updated,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct OperationRecord {
    pub id: OperationId,
    pub request_id: String,
    pub principal: Principal,
    pub workspace: Option<WorkspaceId>,
    pub branch: Option<BranchId>,
    pub name: String,
    pub fingerprint: ObjectId,
    pub phase: OperationPhase,
    pub local_saved: bool,
    pub remote_confirmed: bool,
    pub mount_ready: Option<bool>,
    pub result: Option<CommandResult>,
    pub error: Option<Error>,
    pub pending: Option<PendingAction>,
    pub created_ns: i64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PendingAction {
    MountChange {
        old: Option<MountBinding>,
        new: Option<MountBinding>,
        session: Option<Box<SessionBinding>>,
    },
    ApplyRevision {
        target: BranchGuard,
        revision: RevisionId,
        merge: Option<MergeId>,
        mount: Option<MountBinding>,
        durability: Durability,
    },
    Transfer {
        target: LocationId,
        guard: BranchGuard,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(tag = "kind", content = "record", rename_all = "snake_case")]
pub enum HistoryRecord {
    Revision(Revision),
    Turn(TurnRecord),
    Operation(Box<OperationRecord>),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct BranchRef {
    pub workspace: WorkspaceId,
    pub branch: BranchId,
    pub owner: LocationId,
    pub authority_epoch: u64,
    pub generation: u64,
    pub source_generation: u64,
    pub durable_seq: u64,
    pub formal_head: RevisionId,
    pub autosave_head: Option<RevisionId>,
    pub working_root: ObjectRef,
    pub record_index: ObjectRef,
    pub deleted: bool,
    pub write_stopped: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct SyncJob {
    pub operation: OperationId,
    pub reference: BranchRef,
    pub attempts: u32,
    pub error: Option<Error>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct CachePin {
    pub workspace: WorkspaceId,
    pub revision: RevisionId,
    pub paths: Vec<WorkspacePath>,
    pub objects: Vec<ObjectRef>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct CacheStatus {
    pub revision: RevisionId,
    pub paths: Vec<WorkspacePath>,
    pub complete: bool,
    pub offline_ready: bool,
    pub completed_bytes: u64,
    pub missing: Vec<ObjectId>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct CacheConfig {
    pub max_bytes: u64,
    pub target_bytes: u64,
    pub min_free_bytes: u64,
}

impl Default for CacheConfig {
    fn default() -> Self {
        Self {
            max_bytes: 2 * 1024 * 1024 * 1024,
            target_bytes: 1536 * 1024 * 1024,
            min_free_bytes: 64 * 1024 * 1024,
        }
    }
}

impl CacheConfig {
    pub fn validate(&self) -> Result<()> {
        if self.target_bytes >= self.max_bytes {
            return Err(Error::invalid("cache target must be below its limit"));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct MergeConflict {
    pub path: WorkspacePath,
    pub base: Option<Inode>,
    pub source: Option<Inode>,
    pub target: Option<Inode>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ValidationRecord {
    pub candidate: RevisionId,
    pub config_version: ObjectId,
    pub passed: bool,
    pub skipped: bool,
    pub output: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct MergeCandidate {
    pub id: MergeId,
    pub workspace: WorkspaceId,
    pub base: RevisionId,
    pub source: RevisionId,
    pub target: BranchGuard,
    pub revision: RevisionId,
    pub conflicts: Vec<MergeConflict>,
    pub validation: Option<ValidationRecord>,
    pub applied_revision: Option<RevisionId>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct FileDifference {
    pub path: WorkspacePath,
    pub before: Option<Inode>,
    pub after: Option<Inode>,
}

pub type FileTree = BTreeMap<WorkspacePath, Inode>;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct StorageStats {
    pub total_bytes: u64,
    pub available_bytes: u64,
    pub cached_bytes: u64,
    pub protected_bytes: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct HistoryPin {
    pub operation: OperationId,
    pub roots: Vec<ObjectRef>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum GcPhase {
    Open,
    Closing,
    Collecting,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct WorkspaceControl {
    pub workspace: Workspace,
    pub initial_revision: Revision,
    pub gc_epoch: u64,
    pub phase: GcPhase,
    pub active_operations: BTreeMap<OperationId, Vec<ObjectRef>>,
    pub history_pins: BTreeMap<OperationId, HistoryPin>,
}
