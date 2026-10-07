use crate::FileSystem;
use agentfs_model::*;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::{path::PathBuf, sync::Arc};

pub trait Clock: Send + Sync {
    fn now_ns(&self) -> i64;
    fn monotonic_ms(&self) -> u64;
}

#[derive(Clone, Debug)]
pub struct MountSpec {
    pub binding: MountBinding,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NativeMountState {
    Mounted,
    Absent,
    Unknown,
}

#[async_trait]
pub trait MountDriver: Send + Sync {
    async fn normalize_path(&self, path: &std::path::Path) -> Result<PathBuf>;
    async fn mount(&self, spec: MountSpec, file_system: Arc<dyn FileSystem>) -> Result<()>;
    async fn unmount(&self, binding: &MountBinding) -> Result<()>;
    async fn status(&self, binding: &MountBinding) -> Result<NativeMountState>;
    fn capability(&self) -> &'static str;
}

#[derive(Clone, Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ValidationConfig {
    pub image: String,
    pub program: PathBuf,
    pub arguments: Vec<String>,
    pub timeout_seconds: u64,
    pub max_output_bytes: usize,
}

#[derive(Clone)]
pub struct ValidationRequest {
    pub candidate: RevisionId,
    pub workspace: WorkspaceId,
    pub config: ValidationConfig,
    pub tree: FileTree,
    pub content: Arc<dyn crate::ContentResolver>,
}

impl std::fmt::Debug for ValidationRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ValidationRequest")
            .field("candidate", &self.candidate)
            .finish_non_exhaustive()
    }
}

#[async_trait]
pub trait ValidationRunner: Send + Sync {
    async fn run(&self, request: ValidationRequest) -> Result<ValidationRecord>;
}

#[async_trait]
pub trait DirectoryAccess: Send + Sync {
    async fn import_tree(
        &self,
        workspace: &Workspace,
        source: &std::path::Path,
        objects: Arc<dyn crate::LocalObjectStore>,
    ) -> Result<FileTree>;
    async fn export_tree(
        &self,
        workspace: WorkspaceId,
        tree: FileTree,
        destination: &std::path::Path,
        content: Arc<dyn crate::ContentResolver>,
    ) -> Result<()>;
}
