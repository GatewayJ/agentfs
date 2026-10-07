use agentfs_model::*;
use async_trait::async_trait;
use bytes::Bytes;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Default, Serialize, Deserialize, schemars::JsonSchema)]
pub struct AttributeUpdate {
    pub mode: Option<u32>,
    pub size: Option<u64>,
    pub modified_ns: Option<i64>,
}

#[derive(Clone, Debug)]
pub struct DirectoryItem {
    pub name: String,
    pub inode: Inode,
}

#[derive(Clone, Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub struct DurabilityReceipt {
    pub branch: BranchId,
    pub mutation_seq: u64,
    pub local_saved: bool,
    pub remote_confirmed: bool,
    pub operation: OperationId,
}

#[async_trait]
pub trait FileSystem: Send + Sync {
    async fn lookup(&self, context: &FsContext, parent: InodeId, name: &str) -> Result<Inode>;
    async fn getattr(&self, context: &FsContext, inode: InodeId) -> Result<Inode>;
    async fn readdir(&self, context: &FsContext, inode: InodeId) -> Result<Vec<DirectoryItem>>;
    async fn create(
        &self,
        context: &FsContext,
        parent: InodeId,
        name: &str,
        kind: FileKind,
        mode: u32,
    ) -> Result<Inode>;
    async fn setattr(
        &self,
        context: &FsContext,
        inode: InodeId,
        attributes: AttributeUpdate,
    ) -> Result<Inode>;
    async fn open(&self, context: &FsContext, inode: InodeId, mode: OpenMode)
    -> Result<FileHandle>;
    async fn read(
        &self,
        context: &FsContext,
        handle: HandleId,
        offset: u64,
        size: u32,
    ) -> Result<Bytes>;
    async fn write(
        &self,
        context: &FsContext,
        handle: HandleId,
        offset: u64,
        data: Bytes,
    ) -> Result<u32>;
    async fn release(&self, context: &FsContext, handle: HandleId) -> Result<()>;
    async fn rename(
        &self,
        context: &FsContext,
        parent: InodeId,
        name: &str,
        new_parent: InodeId,
        new_name: &str,
        replace: bool,
    ) -> Result<()>;
    async fn can_unlink(
        &self,
        context: &FsContext,
        parent: InodeId,
        name: &str,
        directory: bool,
        closing_handle: Option<HandleId>,
    ) -> Result<()>;
    async fn unlink(
        &self,
        context: &FsContext,
        parent: InodeId,
        name: &str,
        directory: bool,
    ) -> Result<()>;
    async fn fsync(&self, context: &FsContext) -> Result<DurabilityReceipt>;
    async fn statfs(&self, context: &FsContext) -> Result<StorageStats>;
}
