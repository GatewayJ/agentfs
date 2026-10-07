use agentfs_engine::*;
use agentfs_local::LocalBackend;
use agentfs_model::*;
use agentfs_ports::*;
use agentfs_test_support::*;
use bytes::Bytes;
use std::{collections::BTreeMap, sync::Arc};

fn actor() -> Principal {
    "owner".to_owned().try_into().unwrap()
}
fn context() -> RequestContext {
    RequestContext {
        principal: actor(),
        request_id: OperationId::new().to_string(),
    }
}
fn path(value: &str) -> WorkspacePath {
    value.to_owned().try_into().unwrap()
}
fn identity() -> LocalIdentity {
    LocalIdentity {
        uid: 1000,
        gid: 1000,
    }
}
fn success(operation: OperationRecord) -> OperationRecord {
    assert!(operation.error.is_none(), "{operation:?}");
    operation
}

struct Harness {
    directory: tempfile::TempDir,
    local: Arc<LocalBackend>,
    engine: Arc<Engine>,
    clock: Arc<ManualClock>,
    driver: Arc<MemoryMountDriver>,
}
impl Harness {
    async fn new(remote: Option<Arc<MemoryRemote>>) -> Self {
        let directory = tempfile::tempdir().unwrap();
        let (local, engine, clock, driver) = open(directory.path(), remote).await;
        Self {
            directory,
            local,
            engine,
            clock,
            driver,
        }
    }
    async fn workspace(&self) -> Workspace {
        let operation = success(
            self.engine
                .services
                .workspace
                .create(
                    context(),
                    CreateWorkspace {
                        name: "test".into(),
                        grants: vec![],
                        max_file_bytes: None,
                        max_working_bytes: None,
                        max_inodes: None,
                    },
                )
                .await
                .unwrap(),
        );
        match operation.result.unwrap() {
            CommandResult::Workspace { workspace } => workspace,
            result => panic!("{result:?}"),
        }
    }
    async fn session(
        &self,
        workspace: WorkspaceId,
        name: &str,
        source: Option<RevisionId>,
    ) -> (SessionBinding, FsContext) {
        let key = SessionKey {
            workspace,
            app_namespace: "tests".into(),
            session_id: name.into(),
            location: self.engine.runtime.location,
        };
        let operation = success(
            self.engine
                .services
                .sessions
                .open(
                    context(),
                    OpenSession {
                        key,
                        source_revision: source,
                        source_branch: None,
                        mount_path: Some(self.directory.path().join(name)),
                        durability: Durability::Local,
                    },
                )
                .await
                .unwrap(),
        );
        let binding = match operation.result.unwrap() {
            CommandResult::Session { binding } => binding,
            result => panic!("{result:?}"),
        };
        let mount = self
            .local
            .mount(binding.mount.unwrap())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(mount.state, MountState::Ready);
        let fs = FsContext {
            mount: mount.id,
            generation: mount.generation,
            identity: identity(),
        };
        (binding, fs)
    }
    async fn write_new(&self, fs: &FsContext, name: &str, bytes: &'static [u8]) -> Inode {
        let file = self
            .engine
            .services
            .files
            .create(fs, InodeId::ROOT, name, FileKind::File, 0o644)
            .await
            .unwrap();
        let handle = self
            .engine
            .services
            .files
            .open(
                fs,
                file.id,
                OpenMode {
                    read: true,
                    write: true,
                    append: false,
                },
            )
            .await
            .unwrap();
        self.engine
            .services
            .files
            .write(fs, handle.id, 0, Bytes::from_static(bytes))
            .await
            .unwrap();
        self.engine
            .services
            .files
            .release(fs, handle.id)
            .await
            .unwrap();
        file
    }
    async fn commit(&self, branch: BranchId) -> RevisionId {
        let branch = self.local.branch(branch).await.unwrap().unwrap();
        let operation = success(
            self.engine
                .services
                .revisions
                .commit(
                    context(),
                    CommitRevision {
                        guard: BranchGuard::from(&branch),
                        kind: RevisionKind::Commit,
                        durability: Durability::Local,
                    },
                )
                .await
                .unwrap(),
        );
        match operation.result.unwrap() {
            CommandResult::Revision { revision, .. } => revision,
            result => panic!("{result:?}"),
        }
    }
    async fn sync(&self, workspace: WorkspaceId, direction: SyncDirection) {
        success(
            self.engine
                .services
                .replica
                .sync(
                    context(),
                    SyncRequest {
                        workspace,
                        branches: None,
                        direction,
                    },
                )
                .await
                .unwrap(),
        );
    }
}
async fn open(
    directory: &std::path::Path,
    remote: Option<Arc<MemoryRemote>>,
) -> (
    Arc<LocalBackend>,
    Arc<Engine>,
    Arc<ManualClock>,
    Arc<MemoryMountDriver>,
) {
    let local = Arc::new(LocalBackend::open(directory).unwrap());
    let clock = Arc::new(ManualClock::default());
    let driver = Arc::new(MemoryMountDriver::default());
    let engine = Engine::open(
        Adapters {
            state: local.clone(),
            objects: local.clone(),
            working: local.clone(),
            remote_objects: remote
                .clone()
                .map(|remote| remote as Arc<dyn RemoteObjectStore>),
            remote_refs: remote.map(|remote| remote as Arc<dyn RemoteRefStore>),
            clock: clock.clone(),
            mounts: driver.clone(),
            directories: Arc::new(NoHostDirectories),
            validation: Arc::new(TestValidation),
        },
        EngineConfig {
            cache: CacheConfig {
                max_bytes: 1,
                target_bytes: 0,
                min_free_bytes: 0,
            },
            identity: identity(),
            validation: BTreeMap::new(),
        },
    )
    .await
    .unwrap();
    (local, engine, clock, driver)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_retries_create_exactly_one_workspace() {
    let harness = Harness::new(None).await;
    let request = context();
    let mut tasks = tokio::task::JoinSet::new();
    for _ in 0..16 {
        let service = harness.engine.services.workspace.clone();
        let context = request.clone();
        tasks.spawn(async move {
            service
                .create(
                    context,
                    CreateWorkspace {
                        name: "once".into(),
                        grants: vec![],
                        max_file_bytes: None,
                        max_working_bytes: None,
                        max_inodes: None,
                    },
                )
                .await
                .unwrap()
                .id
        });
    }
    let mut ids = std::collections::BTreeSet::new();
    while let Some(result) = tasks.join_next().await {
        ids.insert(result.unwrap());
    }
    assert_eq!(ids.len(), 1);
    let record = harness
        .engine
        .services
        .operations
        .by_request(&actor(), &request.request_id)
        .await
        .unwrap();
    assert!(record.local_saved);
    assert_eq!(harness.local.workspaces().await.unwrap().len(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn append_is_atomic_and_rename_preserves_open_handles() {
    let harness = Harness::new(None).await;
    let workspace = harness.workspace().await;
    let (_, fs) = harness.session(workspace.id, "append", None).await;
    let file = harness.write_new(&fs, "data", b"").await;
    let handle = harness
        .engine
        .services
        .files
        .open(
            &fs,
            file.id,
            OpenMode {
                read: true,
                write: true,
                append: true,
            },
        )
        .await
        .unwrap();
    let mut tasks = tokio::task::JoinSet::new();
    for value in 0..64u8 {
        let service = harness.engine.services.files.clone();
        let fs = fs.clone();
        let id = handle.id;
        tasks.spawn(async move {
            service
                .write(&fs, id, 0, Bytes::from(vec![value]))
                .await
                .unwrap();
        });
    }
    while let Some(result) = tasks.join_next().await {
        result.unwrap();
    }
    harness
        .engine
        .services
        .files
        .rename(&fs, InodeId::ROOT, "data", InodeId::ROOT, "renamed", false)
        .await
        .unwrap();
    let mut bytes = harness
        .engine
        .services
        .files
        .read(&fs, handle.id, 0, 1024)
        .await
        .unwrap()
        .to_vec();
    bytes.sort();
    assert_eq!(bytes, (0..64u8).collect::<Vec<_>>());
    assert_eq!(
        harness
            .engine
            .services
            .files
            .unlink(&fs, InodeId::ROOT, "renamed", false)
            .await
            .unwrap_err()
            .code,
        ErrorCode::Busy
    );
    harness
        .engine
        .services
        .files
        .release(&fs, handle.id)
        .await
        .unwrap();
    harness.engine.services.files.fsync(&fs).await.unwrap();
}

#[tokio::test]
async fn restart_restores_saved_bytes_and_requires_explicit_turn_recovery() {
    let harness = Harness::new(None).await;
    let workspace = harness.workspace().await;
    let (session, fs) = harness.session(workspace.id, "resume", None).await;
    success(
        harness
            .engine
            .services
            .sessions
            .turn_begin(
                context(),
                BeginTurn {
                    key: session.key.clone(),
                    turn_id: "turn".into(),
                },
            )
            .await
            .unwrap(),
    );
    let file = harness.write_new(&fs, "file", b"saved bytes").await;
    let receipt = harness.engine.services.files.fsync(&fs).await.unwrap();
    assert!(receipt.local_saved);
    let handle = harness
        .engine
        .services
        .files
        .open(
            &fs,
            file.id,
            OpenMode {
                read: true,
                write: true,
                append: false,
            },
        )
        .await
        .unwrap();
    harness
        .engine
        .services
        .files
        .write(&fs, handle.id, 0, Bytes::from_static(b"lost writes"))
        .await
        .unwrap();
    let Harness {
        directory,
        local,
        engine,
        clock,
        driver,
    } = harness;
    drop((local, engine, clock, driver));
    let (local, engine, _, _) = open(directory.path(), None).await;
    assert_eq!(
        local.session(&session.key).await.unwrap().unwrap().state,
        SessionState::RecoveryRequired
    );
    let denied = engine
        .services
        .sessions
        .resume(
            context(),
            ResumeSession {
                key: session.key.clone(),
                mount_path: None,
                recovery_action: None,
                turn_id: None,
                durability: Durability::Local,
            },
        )
        .await
        .unwrap();
    assert_eq!(denied.error.unwrap().code, ErrorCode::RecoveryRequired);
    let pending = success(
        engine
            .services
            .sessions
            .resume(
                context(),
                ResumeSession {
                    key: session.key.clone(),
                    mount_path: None,
                    recovery_action: Some(RecoveryAction::Continue),
                    turn_id: Some("turn".into()),
                    durability: Durability::Local,
                },
            )
            .await
            .unwrap(),
    );
    assert_eq!(pending.phase, OperationPhase::WaitingForQuiesce);
    success(
        engine
            .services
            .mounts
            .release(
                context(),
                ReleaseMount {
                    operation: pending.id,
                    expected_binding_generation: fs.generation,
                },
            )
            .await
            .unwrap(),
    );
    let resumed = local.session(&session.key).await.unwrap().unwrap();
    let mount = local.mount(resumed.mount.unwrap()).await.unwrap().unwrap();
    let fs = FsContext {
        mount: mount.id,
        generation: mount.generation,
        identity: identity(),
    };
    let handle = engine
        .services
        .files
        .open(
            &fs,
            file.id,
            OpenMode {
                read: true,
                write: false,
                append: false,
            },
        )
        .await
        .unwrap();
    assert_eq!(
        engine
            .services
            .files
            .read(&fs, handle.id, 0, 1024)
            .await
            .unwrap(),
        "saved bytes"
    );
}

#[tokio::test]
async fn unchanged_turns_preserve_results_without_creating_revisions() {
    let harness = Harness::new(None).await;
    let workspace = harness.workspace().await;
    let (session, _) = harness.session(workspace.id, "turns", None).await;
    for (index, status) in [
        TurnStatus::Completed,
        TurnStatus::Failed,
        TurnStatus::Cancelled,
    ]
    .into_iter()
    .enumerate()
    {
        let turn_id = index.to_string();
        success(
            harness
                .engine
                .services
                .sessions
                .turn_begin(
                    context(),
                    BeginTurn {
                        key: session.key.clone(),
                        turn_id: turn_id.clone(),
                    },
                )
                .await
                .unwrap(),
        );
        let request = EndTurn {
            key: session.key.clone(),
            turn_id: turn_id.clone(),
            status,
            durability: Durability::Local,
        };
        let first = success(
            harness
                .engine
                .services
                .sessions
                .turn_end(context(), request.clone())
                .await
                .unwrap(),
        );
        let again = success(
            harness
                .engine
                .services
                .sessions
                .turn_end(context(), request)
                .await
                .unwrap(),
        );
        assert_eq!(first.result, again.result);
        let changed = harness
            .engine
            .services
            .sessions
            .turn_end(
                context(),
                EndTurn {
                    key: session.key.clone(),
                    turn_id,
                    status: if status == TurnStatus::Failed {
                        TurnStatus::Completed
                    } else {
                        TurnStatus::Failed
                    },
                    durability: Durability::Local,
                },
            )
            .await
            .unwrap();
        assert_eq!(changed.error.unwrap().code, ErrorCode::TurnResultConflict);
    }
    assert_eq!(
        harness.local.revisions(workspace.id).await.unwrap().len(),
        1
    );
    assert_eq!(harness.local.turns(workspace.id).await.unwrap().len(), 3);
}

#[tokio::test]
async fn autosave_uses_quiet_and_continuous_write_deadlines() {
    let harness = Harness::new(None).await;
    let workspace = harness.workspace().await;
    let (_, fs) = harness.session(workspace.id, "autosave", None).await;
    let file = harness.write_new(&fs, "file", b"first").await;
    harness.clock.advance(4_999);
    assert_eq!(harness.engine.autosave_due().await.unwrap(), 0);
    harness.clock.advance(1);
    assert_eq!(harness.engine.autosave_due().await.unwrap(), 1);
    let handle = harness
        .engine
        .services
        .files
        .open(
            &fs,
            file.id,
            OpenMode {
                read: false,
                write: true,
                append: true,
            },
        )
        .await
        .unwrap();
    for _ in 0..8 {
        harness
            .engine
            .services
            .files
            .write(&fs, handle.id, 0, Bytes::from_static(b"x"))
            .await
            .unwrap();
        harness.clock.advance(4_000);
    }
    assert_eq!(harness.engine.autosave_due().await.unwrap(), 1);
}

#[tokio::test]
async fn remote_outage_preserves_local_save_and_sync_confirms_lost_responses() {
    let remote = Arc::new(MemoryRemote::default());
    let harness = Harness::new(Some(remote.clone())).await;
    let workspace = harness.workspace().await;
    let (session, fs) = harness.session(workspace.id, "remote", None).await;
    harness.write_new(&fs, "file", b"durable").await;
    remote.set_available(false);
    let branch = harness.local.branch(session.branch).await.unwrap().unwrap();
    let saved = harness
        .engine
        .services
        .revisions
        .commit(
            context(),
            CommitRevision {
                guard: BranchGuard::from(&branch),
                kind: RevisionKind::Commit,
                durability: Durability::Remote,
            },
        )
        .await
        .unwrap();
    assert!(saved.local_saved);
    assert!(!saved.remote_confirmed);
    assert_eq!(saved.error.as_ref().unwrap().code, ErrorCode::Unavailable);
    remote.set_available(true);
    remote.lose_next_cas_response();
    harness.sync(workspace.id, SyncDirection::Push).await;
    assert!(
        harness
            .engine
            .services
            .operations
            .get(&actor(), saved.id)
            .await
            .unwrap()
            .remote_confirmed
    );
    let second = Harness::new(Some(remote)).await;
    second.sync(workspace.id, SyncDirection::Pull).await;
    let revision = harness
        .local
        .branch(session.branch)
        .await
        .unwrap()
        .unwrap()
        .formal_head;
    let bytes = second
        .engine
        .services
        .revisions
        .read_file(&actor(), workspace.id, revision, path("/file"), 0, 1024)
        .await
        .unwrap();
    assert_eq!(bytes, "durable");
}

#[tokio::test]
async fn ownership_transfer_stops_old_writer_and_adopts_on_new_location() {
    let remote = Arc::new(MemoryRemote::default());
    let first = Harness::new(Some(remote.clone())).await;
    let second = Harness::new(Some(remote.clone())).await;
    let workspace = first.workspace().await;
    let (session, fs) = first.session(workspace.id, "owner", None).await;
    first.write_new(&fs, "file", b"handoff").await;
    first.commit(session.branch).await;
    first.sync(workspace.id, SyncDirection::Push).await;
    success(
        first
            .engine
            .services
            .mounts
            .unmount(context(), fs.mount, fs.generation)
            .await
            .unwrap(),
    );
    let branch = first.local.branch(session.branch).await.unwrap().unwrap();
    remote.lose_next_cas_response();
    let operation = success(
        first
            .engine
            .services
            .ownership
            .transfer(
                context(),
                TransferOwnership {
                    guard: BranchGuard::from(&branch),
                    target: second.engine.runtime.location,
                },
            )
            .await
            .unwrap(),
    );
    assert!(operation.remote_confirmed);
    second.sync(workspace.id, SyncDirection::Pull).await;
    let adopted = second.local.branch(session.branch).await.unwrap().unwrap();
    assert_eq!(adopted.owner, second.engine.runtime.location);
    assert_eq!(adopted.authority_epoch, branch.authority_epoch + 1);
    assert_eq!(adopted.state, BranchState::Writable);
    let old = first.local.branch(session.branch).await.unwrap().unwrap();
    assert_eq!(old.state, BranchState::Stopped);
    assert_eq!(
        old.check_write(first.engine.runtime.location, old.authority_epoch)
            .unwrap_err()
            .code,
        ErrorCode::StaleAuthority
    );
}

#[tokio::test]
async fn merge_requires_current_validation_and_preserves_both_parents() {
    let harness = Harness::new(None).await;
    let workspace = harness.workspace().await;
    let (base, base_fs) = harness.session(workspace.id, "base", None).await;
    harness.write_new(&base_fs, "file", b"base").await;
    let base_revision = harness.commit(base.branch).await;
    let (source, source_fs) = harness
        .session(workspace.id, "source", Some(base_revision))
        .await;
    let (target, target_fs) = harness
        .session(workspace.id, "target", Some(base_revision))
        .await;
    for (fs, data) in [
        (&source_fs, b"source".as_slice()),
        (&target_fs, b"target".as_slice()),
    ] {
        let file = harness
            .engine
            .services
            .files
            .lookup(fs, InodeId::ROOT, "file")
            .await
            .unwrap();
        let handle = harness
            .engine
            .services
            .files
            .open(
                fs,
                file.id,
                OpenMode {
                    read: false,
                    write: true,
                    append: false,
                },
            )
            .await
            .unwrap();
        harness
            .engine
            .services
            .files
            .write(fs, handle.id, 0, Bytes::copy_from_slice(data))
            .await
            .unwrap();
        harness
            .engine
            .services
            .files
            .release(fs, handle.id)
            .await
            .unwrap();
    }
    let source_revision = harness.commit(source.branch).await;
    let target_revision = harness.commit(target.branch).await;
    let guard = BranchGuard::from(&harness.local.branch(target.branch).await.unwrap().unwrap());
    let prepared = success(
        harness
            .engine
            .services
            .merge
            .prepare(
                context(),
                PrepareMerge {
                    workspace: workspace.id,
                    source: source_revision,
                    target: guard,
                    base: None,
                },
            )
            .await
            .unwrap(),
    );
    let candidate = match prepared.result.unwrap() {
        CommandResult::Merge { candidate } => candidate,
        result => panic!("{result:?}"),
    };
    assert_eq!(candidate.conflicts.len(), 1);
    let rejected = harness
        .engine
        .services
        .merge
        .validate(
            context(),
            ValidateMerge {
                merge: candidate.id,
                candidate: candidate.revision,
                configuration: None,
            },
        )
        .await
        .unwrap();
    assert_eq!(rejected.error.unwrap().code, ErrorCode::UnresolvedConflict);
    let resolved = success(
        harness
            .engine
            .services
            .merge
            .resolve(
                context(),
                ResolveMerge {
                    merge: candidate.id,
                    candidate: candidate.revision,
                    resolutions: vec![Resolution::Source {
                        path: path("/file"),
                    }],
                },
            )
            .await
            .unwrap(),
    );
    let candidate = match resolved.result.unwrap() {
        CommandResult::Merge { candidate } => candidate,
        _ => unreachable!(),
    };
    let denied = harness
        .engine
        .services
        .merge
        .apply(
            context(),
            ApplyMerge {
                merge: candidate.id,
                candidate: candidate.revision,
                target: guard,
                durability: Durability::Local,
            },
        )
        .await
        .unwrap();
    assert_eq!(denied.error.unwrap().code, ErrorCode::ValidationFailed);
    success(
        harness
            .engine
            .services
            .merge
            .validate(
                context(),
                ValidateMerge {
                    merge: candidate.id,
                    candidate: candidate.revision,
                    configuration: None,
                },
            )
            .await
            .unwrap(),
    );
    let pending = success(
        harness
            .engine
            .services
            .merge
            .apply(
                context(),
                ApplyMerge {
                    merge: candidate.id,
                    candidate: candidate.revision,
                    target: guard,
                    durability: Durability::Local,
                },
            )
            .await
            .unwrap(),
    );
    assert_eq!(pending.phase, OperationPhase::WaitingForQuiesce);
    success(
        harness
            .engine
            .services
            .mounts
            .release(
                context(),
                ReleaseMount {
                    operation: pending.id,
                    expected_binding_generation: target_fs.generation,
                },
            )
            .await
            .unwrap(),
    );
    let applied = harness
        .engine
        .services
        .operations
        .get(&actor(), pending.id)
        .await
        .unwrap();
    let revision = match applied.result.unwrap() {
        CommandResult::Revision { revision, .. } => revision,
        _ => unreachable!(),
    };
    assert_eq!(
        harness
            .local
            .revision(revision)
            .await
            .unwrap()
            .unwrap()
            .parents,
        vec![target_revision, source_revision]
    );
    assert_eq!(
        harness
            .engine
            .services
            .revisions
            .read_file(&actor(), workspace.id, revision, path("/file"), 0, 1024)
            .await
            .unwrap(),
        "source"
    );
    assert_eq!(
        harness
            .engine
            .services
            .files
            .getattr(&target_fs, InodeId::ROOT)
            .await
            .unwrap_err()
            .code,
        ErrorCode::StaleBinding
    );
}

#[tokio::test]
async fn pinned_historical_file_survives_eviction_and_unpin_releases_it() {
    let remote = Arc::new(MemoryRemote::default());
    let harness = Harness::new(Some(remote)).await;
    let workspace = harness.workspace().await;
    let (session, fs) = harness.session(workspace.id, "cache", None).await;
    let file = harness.write_new(&fs, "file", b"old bytes").await;
    let old = harness.commit(session.branch).await;
    let object = harness
        .local
        .node(session.branch, file.id)
        .await
        .unwrap()
        .unwrap()
        .inode
        .content
        .unwrap();
    let handle = harness
        .engine
        .services
        .files
        .open(
            &fs,
            file.id,
            OpenMode {
                read: false,
                write: true,
                append: false,
            },
        )
        .await
        .unwrap();
    harness
        .engine
        .services
        .files
        .write(&fs, handle.id, 0, Bytes::from_static(b"new bytes"))
        .await
        .unwrap();
    harness
        .engine
        .services
        .files
        .release(&fs, handle.id)
        .await
        .unwrap();
    harness.commit(session.branch).await;
    harness.sync(workspace.id, SyncDirection::Push).await;
    let range = RevisionRange {
        workspace: workspace.id,
        revision: old,
        paths: Some(vec![path("/file")]),
    };
    success(
        harness
            .engine
            .services
            .cache
            .pin(
                context(),
                PinRequest {
                    range: range.clone(),
                    enabled: true,
                },
            )
            .await
            .unwrap(),
    );
    harness.engine.services.cache.collect().await.unwrap();
    assert!(harness.local.contains(workspace.id, &object).await.unwrap());
    assert!(
        harness
            .engine
            .services
            .cache
            .status(&actor(), range.clone())
            .await
            .unwrap()
            .offline_ready
    );
    success(
        harness
            .engine
            .services
            .cache
            .pin(
                context(),
                PinRequest {
                    range,
                    enabled: false,
                },
            )
            .await
            .unwrap(),
    );
    harness.engine.services.cache.collect().await.unwrap();
    assert!(!harness.local.contains(workspace.id, &object).await.unwrap());
}

#[tokio::test]
async fn moved_restore_target_preserves_the_active_mount_and_can_be_cancelled() {
    let harness = Harness::new(None).await;
    let workspace = harness.workspace().await;
    let (session, fs) = harness.session(workspace.id, "restore", None).await;
    harness.write_new(&fs, "original", b"keep").await;
    harness.commit(session.branch).await;
    let branch = harness.local.branch(session.branch).await.unwrap().unwrap();
    let pending = success(
        harness
            .engine
            .services
            .revisions
            .restore(
                context(),
                RestoreBranch {
                    guard: BranchGuard::from(&branch),
                    revision: workspace.initial_revision,
                    durability: Durability::Local,
                },
            )
            .await
            .unwrap(),
    );
    assert_eq!(pending.phase, OperationPhase::WaitingForQuiesce);
    harness.write_new(&fs, "newer", b"keep newer").await;
    let rejected = harness
        .engine
        .services
        .mounts
        .release(
            context(),
            ReleaseMount {
                operation: pending.id,
                expected_binding_generation: fs.generation,
            },
        )
        .await
        .unwrap();
    assert_eq!(rejected.error.unwrap().code, ErrorCode::TargetMoved);
    assert_eq!(
        harness.local.mount(fs.mount).await.unwrap().unwrap().state,
        MountState::Ready
    );
    harness
        .engine
        .services
        .files
        .lookup(&fs, InodeId::ROOT, "newer")
        .await
        .unwrap();
    let cancelled = harness
        .engine
        .services
        .operations
        .cancel(&actor(), pending.id)
        .await
        .unwrap();
    assert_eq!(cancelled.phase, OperationPhase::Cancelled);
}

#[tokio::test]
async fn repeated_turn_end_can_require_remote_confirmation_without_another_revision() {
    let remote = Arc::new(MemoryRemote::default());
    let harness = Harness::new(Some(remote)).await;
    let workspace = harness.workspace().await;
    let (session, _) = harness.session(workspace.id, "remote-turn", None).await;
    success(
        harness
            .engine
            .services
            .sessions
            .turn_begin(
                context(),
                BeginTurn {
                    key: session.key.clone(),
                    turn_id: "turn".into(),
                },
            )
            .await
            .unwrap(),
    );
    let first = success(
        harness
            .engine
            .services
            .sessions
            .turn_end(
                context(),
                EndTurn {
                    key: session.key.clone(),
                    turn_id: "turn".into(),
                    status: TurnStatus::Completed,
                    durability: Durability::Local,
                },
            )
            .await
            .unwrap(),
    );
    let repeated = success(
        harness
            .engine
            .services
            .sessions
            .turn_end(
                context(),
                EndTurn {
                    key: session.key,
                    turn_id: "turn".into(),
                    status: TurnStatus::Completed,
                    durability: Durability::Remote,
                },
            )
            .await
            .unwrap(),
    );
    assert_eq!(first.result, repeated.result);
    assert!(repeated.remote_confirmed);
    assert_eq!(
        harness.local.revisions(workspace.id).await.unwrap().len(),
        1
    );
}

#[tokio::test]
async fn complete_local_cache_can_be_prefetched_while_remote_is_offline() {
    let remote = Arc::new(MemoryRemote::default());
    let harness = Harness::new(Some(remote.clone())).await;
    let workspace = harness.workspace().await;
    let (session, fs) = harness
        .session(workspace.id, "offline-prefetch", None)
        .await;
    harness.write_new(&fs, "file", b"cached bytes").await;
    let revision = harness.commit(session.branch).await;
    remote.set_available(false);
    let record = harness
        .engine
        .services
        .cache
        .prefetch(
            context(),
            RevisionRange {
                workspace: workspace.id,
                revision,
                paths: None,
            },
        )
        .await
        .unwrap();
    assert!(record.error.is_none(), "{record:?}");
    let Some(CommandResult::Prefetch { status }) = record.result else {
        panic!("wrong result")
    };
    assert!(status.complete);
}
