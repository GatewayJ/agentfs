//! Deterministic adapters for persistence and protocol fault tests.
use agentfs_model::*;
use agentfs_ports::*;
use async_trait::async_trait;
use bytes::Bytes;
use parking_lot::Mutex;
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
};
use tokio::io::AsyncReadExt;

#[derive(Debug, Default)]
pub struct ManualClock {
    milliseconds: AtomicU64,
}
impl ManualClock {
    pub fn advance(&self, milliseconds: u64) {
        self.milliseconds.fetch_add(milliseconds, Ordering::SeqCst);
    }
}
impl Clock for ManualClock {
    fn now_ns(&self) -> i64 {
        1_700_000_000_000_000_000 + self.monotonic_ms() as i64 * 1_000_000
    }
    fn monotonic_ms(&self) -> u64 {
        self.milliseconds.load(Ordering::SeqCst)
    }
}

#[derive(Debug)]
pub struct MemoryRemote {
    objects: Mutex<BTreeMap<(WorkspaceId, ObjectId), (ObjectRef, Bytes)>>,
    refs: Mutex<BTreeMap<RefKey, (u64, Bytes)>>,
    available: AtomicBool,
    lose_response: AtomicBool,
}
impl Default for MemoryRemote {
    fn default() -> Self {
        Self {
            objects: Mutex::new(BTreeMap::new()),
            refs: Mutex::new(BTreeMap::new()),
            available: AtomicBool::new(true),
            lose_response: AtomicBool::new(false),
        }
    }
}
impl MemoryRemote {
    pub fn set_available(&self, available: bool) {
        self.available.store(available, Ordering::SeqCst);
    }
    pub fn lose_next_cas_response(&self) {
        self.lose_response.store(true, Ordering::SeqCst);
    }
    fn check(&self) -> Result<()> {
        if self.available.load(Ordering::SeqCst) {
            Ok(())
        } else {
            Err(Error::new(ErrorCode::Unavailable, "injected remote outage").retryable())
        }
    }
}
#[async_trait]
impl RemoteObjectStore for MemoryRemote {
    async fn upload(&self, workspace: WorkspaceId, mut source: ObjectStream) -> Result<()> {
        self.check()?;
        let mut bytes = Vec::new();
        source.reader.read_to_end(&mut bytes).await?;
        verify_object(&source.reference, &bytes)?;
        self.objects.lock().insert(
            (workspace, source.reference.id.clone()),
            (source.reference, bytes.into()),
        );
        Ok(())
    }
    async fn download(&self, workspace: WorkspaceId, object: &ObjectRef) -> Result<ObjectStream> {
        self.check()?;
        let (reference, bytes) = self
            .objects
            .lock()
            .get(&(workspace, object.id.clone()))
            .cloned()
            .ok_or_else(|| Error::new(ErrorCode::NotFound, "remote object does not exist"))?;
        Ok(ObjectStream {
            reference,
            reader: Box::pin(std::io::Cursor::new(bytes)),
        })
    }
    async fn contains(&self, workspace: WorkspaceId, object: &ObjectRef) -> Result<bool> {
        self.check()?;
        Ok(self
            .objects
            .lock()
            .contains_key(&(workspace, object.id.clone())))
    }
}
#[async_trait]
impl RemoteRefStore for MemoryRemote {
    async fn get(&self, key: &RefKey) -> Result<Option<VersionedRef>> {
        self.check()?;
        Ok(self
            .refs
            .lock()
            .get(key)
            .map(|(version, bytes)| VersionedRef {
                version: version.to_string(),
                bytes: bytes.clone(),
            }))
    }
    async fn compare_exchange(&self, update: RefUpdate) -> Result<CasOutcome> {
        self.check()?;
        let mut refs = self.refs.lock();
        let current = refs
            .get(&update.key)
            .map(|(version, _)| version.to_string());
        if current != update.expected_version {
            return Ok(CasOutcome::Conflict);
        }
        let next = refs
            .get(&update.key)
            .map(|(version, _)| version + 1)
            .unwrap_or(1);
        refs.insert(update.key, (next, update.bytes));
        Ok(if self.lose_response.swap(false, Ordering::SeqCst) {
            CasOutcome::Unknown
        } else {
            CasOutcome::Applied {
                version: next.to_string(),
            }
        })
    }
    async fn list_branches(&self, workspace: WorkspaceId) -> Result<Vec<BranchId>> {
        self.check()?;
        Ok(self
            .refs
            .lock()
            .keys()
            .filter_map(|key| match key {
                RefKey::Branch {
                    workspace: id,
                    branch,
                } if *id == workspace => Some(*branch),
                _ => None,
            })
            .collect())
    }
}

#[derive(Debug, Default)]
pub struct MemoryMountDriver {
    mounts: Mutex<BTreeMap<MountId, MountBinding>>,
    pub fail_mount: AtomicBool,
    pub busy: AtomicBool,
}
#[async_trait]
impl MountDriver for MemoryMountDriver {
    async fn normalize_path(&self, path: &Path) -> Result<PathBuf> {
        if !path.is_absolute()
            || path
                .components()
                .any(|component| matches!(component, std::path::Component::ParentDir))
        {
            return Err(Error::invalid("mount path must be absolute"));
        }
        Ok(path.to_owned())
    }
    async fn mount(&self, spec: MountSpec, _: Arc<dyn FileSystem>) -> Result<()> {
        if self.fail_mount.swap(false, Ordering::SeqCst) {
            return Err(Error::new(ErrorCode::Unavailable, "injected mount failure"));
        }
        let mut mounts = self.mounts.lock();
        if mounts.values().any(|mount| mount.path == spec.binding.path) {
            return Err(Error::new(ErrorCode::MountBusy, "mount path is occupied"));
        }
        mounts.insert(spec.binding.id, spec.binding);
        Ok(())
    }
    async fn unmount(&self, binding: &MountBinding) -> Result<()> {
        if self.busy.load(Ordering::SeqCst) {
            return Err(Error::new(ErrorCode::MountBusy, "injected busy mount"));
        }
        self.mounts.lock().remove(&binding.id);
        Ok(())
    }
    async fn status(&self, binding: &MountBinding) -> Result<NativeMountState> {
        Ok(match self.mounts.lock().get(&binding.id) {
            Some(current)
                if current.generation == binding.generation && current.path == binding.path =>
            {
                NativeMountState::Mounted
            }
            Some(_) => NativeMountState::Unknown,
            None => NativeMountState::Absent,
        })
    }
    fn capability(&self) -> &'static str {
        "in-memory test mount"
    }
}

#[derive(Debug)]
pub struct NoHostDirectories;
#[async_trait]
impl DirectoryAccess for NoHostDirectories {
    async fn import_tree(
        &self,
        _: &Workspace,
        _: &Path,
        _: Arc<dyn LocalObjectStore>,
    ) -> Result<FileTree> {
        Err(Error::new(
            ErrorCode::Unsupported,
            "host directory access is disabled in this test",
        ))
    }
    async fn export_tree(
        &self,
        _: WorkspaceId,
        _: FileTree,
        _: &Path,
        _: Arc<dyn ContentResolver>,
    ) -> Result<()> {
        Err(Error::new(
            ErrorCode::Unsupported,
            "host directory access is disabled in this test",
        ))
    }
}

#[derive(Debug)]
pub struct TestValidation;
#[async_trait]
impl ValidationRunner for TestValidation {
    async fn run(&self, request: ValidationRequest) -> Result<ValidationRecord> {
        Ok(ValidationRecord {
            candidate: request.candidate,
            config_version: object_ref(ObjectKind::RecordIndex, &encode(&request.config)?).id,
            passed: request.config.image != "fail",
            skipped: false,
            output: "Deterministic test validation.".into(),
        })
    }
}
