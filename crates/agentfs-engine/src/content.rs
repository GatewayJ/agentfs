use agentfs_model::*;
use agentfs_ports::*;
use async_trait::async_trait;
use bytes::Bytes;
use parking_lot::Mutex;
use std::{
    collections::BTreeMap,
    sync::{Arc, Weak},
};
use tokio::{io::AsyncReadExt, sync::Mutex as AsyncMutex};

type DownloadGates = BTreeMap<(WorkspaceId, ObjectId), Weak<AsyncMutex<()>>>;

pub struct ContentService {
    pub(crate) local: Arc<dyn LocalObjectStore>,
    pub(crate) remote: Option<Arc<dyn RemoteObjectStore>>,
    pub(crate) refs: Option<Arc<dyn RemoteRefStore>>,
    downloads: Mutex<DownloadGates>,
}

impl std::fmt::Debug for ContentService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ContentService")
            .field("remote", &self.remote.is_some())
            .finish_non_exhaustive()
    }
}

impl ContentService {
    pub fn new(
        local: Arc<dyn LocalObjectStore>,
        remote: Option<Arc<dyn RemoteObjectStore>>,
        refs: Option<Arc<dyn RemoteRefStore>>,
    ) -> Arc<Self> {
        Arc::new(Self {
            local,
            remote,
            refs,
            downloads: Mutex::new(BTreeMap::new()),
        })
    }

    pub(crate) async fn control(
        &self,
        workspace: WorkspaceId,
        mutate: impl Fn(&mut WorkspaceControl) -> Result<bool> + Send,
    ) -> Result<()> {
        let refs = self.refs.as_ref().ok_or_else(|| {
            Error::new(
                ErrorCode::RemoteRequired,
                "remote storage is not configured",
            )
        })?;
        let key = RefKey::Workspace(workspace);
        for _ in 0..16 {
            let current = refs
                .get(&key)
                .await?
                .ok_or_else(|| Error::new(ErrorCode::NotFound, "workspace is not published"))?;
            if current.bytes.len() > MAX_METADATA_BYTES {
                return Err(Error::integrity("workspace control exceeds format limit"));
            }
            let mut control: WorkspaceControl = decode(&current.bytes)?;
            if control.workspace.id != workspace
                || control.workspace.format_version != FORMAT_VERSION
            {
                return Err(Error::integrity(
                    "invalid workspace control identity or format",
                ));
            }
            if !mutate(&mut control)? {
                return Ok(());
            }
            let bytes = encode(&control)?;
            if bytes.len() > MAX_METADATA_BYTES {
                return Err(Error::new(
                    ErrorCode::CapacityExceeded,
                    "workspace control exceeds format limit",
                ));
            }
            match refs
                .compare_exchange(RefUpdate {
                    key: key.clone(),
                    expected_version: Some(current.version),
                    bytes: bytes.into(),
                })
                .await?
            {
                CasOutcome::Applied { .. } => return Ok(()),
                CasOutcome::Conflict | CasOutcome::Unknown => continue,
            }
        }
        Err(Error::new(
            ErrorCode::RemoteUnknown,
            "workspace control update remains unconfirmed",
        )
        .retryable())
    }

    pub(crate) async fn register_publication(
        &self,
        workspace: WorkspaceId,
        operation: OperationId,
        roots: Vec<ObjectRef>,
    ) -> Result<()> {
        self.control(workspace, move |control| {
            if control.active_operations.get(&operation) == Some(&roots) {
                return Ok(false);
            }
            if control.phase != GcPhase::Open {
                return Err(
                    Error::new(ErrorCode::Busy, "workspace reclamation barrier is active")
                        .retryable(),
                );
            }
            control.active_operations.insert(operation, roots.clone());
            Ok(true)
        })
        .await
    }
}

#[async_trait]
impl ContentResolver for ContentService {
    async fn open(&self, workspace: WorkspaceId, reference: &ObjectRef) -> Result<ObjectStream> {
        match self.local.open(workspace, reference).await {
            Ok(stream) => return Ok(stream),
            Err(error) if error.code == ErrorCode::NotFound => (),
            Err(error) => return Err(error),
        }
        let remote = self.remote.as_ref().ok_or_else(|| {
            Error::new(
                ErrorCode::Unavailable,
                "object is not cached and remote storage is unavailable",
            )
            .retryable()
        })?;
        let gate = {
            let mut downloads = self.downloads.lock();
            let key = (workspace, reference.id.clone());
            if let Some(gate) = downloads.get(&key).and_then(Weak::upgrade) {
                gate
            } else {
                downloads.retain(|_, gate| gate.strong_count() != 0);
                let gate = Arc::new(AsyncMutex::new(()));
                downloads.insert(key, Arc::downgrade(&gate));
                gate
            }
        };
        let _guard = gate.lock().await;
        for _ in 0..3 {
            match self.local.open(workspace, reference).await {
                Ok(stream) => return Ok(stream),
                Err(error) if error.code == ErrorCode::NotFound => (),
                Err(error) => return Err(error),
            }
            let stream = remote.download(workspace, reference).await?;
            if stream.reference != *reference {
                return Err(Error::integrity(
                    "remote object identity differs from request",
                ));
            }
            self.local.install(workspace, stream).await?;
        }
        self.local.open(workspace, reference).await
    }

    async fn metadata(&self, workspace: WorkspaceId, reference: &ObjectRef) -> Result<Bytes> {
        if reference.kind == ObjectKind::File || reference.size > MAX_METADATA_BYTES as u64 {
            return Err(Error::integrity("invalid metadata object"));
        }
        let stream = self.open(workspace, reference).await?;
        let mut data = Vec::with_capacity(reference.size as usize);
        stream
            .reader
            .take(reference.size + 1)
            .read_to_end(&mut data)
            .await?;
        verify_object(reference, &data)?;
        Ok(data.into())
    }

    async fn read_range(
        &self,
        workspace: WorkspaceId,
        reference: &ObjectRef,
        offset: u64,
        size: u32,
    ) -> Result<Bytes> {
        let _lease = self.open(workspace, reference).await?;
        self.local
            .read_range(workspace, reference, offset, size)
            .await
    }

    async fn protect_remote(
        &self,
        workspace: WorkspaceId,
        operation: OperationId,
        roots: Vec<ObjectRef>,
    ) -> Result<()> {
        if self.refs.is_none() {
            return Ok(());
        }
        self.control(workspace, move |control| {
            let pin = HistoryPin {
                operation,
                roots: roots.clone(),
            };
            if control.history_pins.get(&operation) == Some(&pin) {
                return Ok(false);
            }
            if control.phase != GcPhase::Open {
                return Err(
                    Error::new(ErrorCode::Busy, "workspace reclamation barrier is active")
                        .retryable(),
                );
            }
            control.history_pins.insert(operation, pin);
            Ok(true)
        })
        .await
    }

    async fn release_remote(&self, workspace: WorkspaceId, operation: OperationId) -> Result<()> {
        if self.refs.is_none() {
            return Ok(());
        }
        self.control(workspace, move |control| {
            let published = control.active_operations.remove(&operation).is_some();
            let pinned = control.history_pins.remove(&operation).is_some();
            Ok(published || pinned)
        })
        .await
    }
}
