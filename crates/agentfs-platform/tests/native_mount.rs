#![cfg(any(
    target_os = "linux",
    windows,
    all(target_os = "macos", feature = "macos-mount")
))]
use agentfs_engine::{Adapters, Engine, EngineConfig};
use agentfs_local::LocalBackend;
use agentfs_model::*;
use agentfs_platform::{
    ContainerValidation, HostDirectories, NativeMountDriver, SystemClock, local_identity,
};
use agentfs_ports::*;
use std::{
    collections::BTreeMap,
    io::{Read, Seek, SeekFrom, Write},
    sync::Arc,
};

fn context() -> RequestContext {
    RequestContext {
        principal: "native-test".to_owned().try_into().unwrap(),
        request_id: OperationId::new().to_string(),
    }
}
fn success(record: OperationRecord) -> OperationRecord {
    assert!(record.error.is_none(), "{record:?}");
    record
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires an installed native filesystem driver and mount permission"]
async fn native_mount_supports_file_io_durable_snapshots_and_read_only_history() {
    let root = tempfile::tempdir().unwrap();
    let local = Arc::new(LocalBackend::open(root.path().join("state")).unwrap());
    let mounts = root.path().join("mounts");
    let driver = NativeMountDriver::new(vec![mounts.clone()]).unwrap();
    let engine = Engine::open(
        Adapters {
            state: local.clone(),
            objects: local.clone(),
            working: local.clone(),
            remote_objects: None,
            remote_refs: None,
            clock: Arc::new(SystemClock::default()),
            mounts: driver,
            directories: HostDirectories::new(vec![]).unwrap(),
            validation: Arc::new(ContainerValidation::new("docker".into())),
        },
        EngineConfig {
            cache: CacheConfig::default(),
            identity: local_identity(),
            validation: BTreeMap::new(),
        },
    )
    .await
    .unwrap();
    let created = success(
        engine
            .services
            .workspace
            .create(
                context(),
                CreateWorkspace {
                    name: "native".into(),
                    grants: vec![],
                    max_file_bytes: None,
                    max_working_bytes: None,
                    max_inodes: None,
                },
            )
            .await
            .unwrap(),
    );
    let workspace = match created.result.unwrap() {
        CommandResult::Workspace { workspace } => workspace,
        _ => panic!("wrong result"),
    };
    let path = mounts.join("working");
    let opened = success(
        engine
            .services
            .sessions
            .open(
                context(),
                OpenSession {
                    key: SessionKey {
                        workspace: workspace.id,
                        app_namespace: "tests".into(),
                        session_id: "native".into(),
                        location: engine.runtime.location,
                    },
                    source_revision: None,
                    source_branch: None,
                    mount_path: Some(path.clone()),
                    durability: Durability::Local,
                },
            )
            .await
            .unwrap(),
    );
    assert_eq!(opened.mount_ready, Some(true));
    let session = match opened.result.unwrap() {
        CommandResult::Session { binding } => binding,
        _ => panic!("wrong result"),
    };
    let operation_path = path.clone();
    let expected = tokio::task::spawn_blocking(move || {
        let path = operation_path;
        std::fs::create_dir(path.join("directory")).unwrap();
        let mut file = std::fs::OpenOptions::new()
            .create_new(true)
            .read(true)
            .write(true)
            .open(path.join("directory/Data"))
            .unwrap();
        let payload: Vec<u8> = (0..3 * 1024 * 1024 + 17)
            .map(|index| (index % 251) as u8)
            .collect();
        file.write_all(&payload).unwrap();
        file.seek(SeekFrom::Start(23)).unwrap();
        file.write_all(b"updated").unwrap();
        file.set_len(1_000_013).unwrap();
        file.sync_all().unwrap();
        std::fs::rename(path.join("directory/Data"), path.join("directory/Renamed")).unwrap();
        file.seek(SeekFrom::Start(0)).unwrap();
        let mut expected = Vec::new();
        file.read_to_end(&mut expected).unwrap();
        assert_eq!(&expected[23..30], b"updated");
        assert_eq!(expected.len(), 1_000_013);
        drop(file);
        let mut append = std::fs::OpenOptions::new()
            .append(true)
            .open(path.join("directory/renamed"))
            .unwrap();
        append.write_all(b"appended").unwrap();
        append.sync_all().unwrap();
        drop(append);
        expected.extend_from_slice(b"appended");
        assert_eq!(
            std::fs::read(path.join("directory/Renamed")).unwrap(),
            expected
        );
        assert_eq!(
            std::fs::read_dir(path.join("directory")).unwrap().count(),
            1
        );
        std::fs::write(path.join("temporary"), b"remove").unwrap();
        std::fs::remove_file(path.join("temporary")).unwrap();
        for index in 0..130 {
            std::fs::write(path.join(format!("entry-{index:03}")), [index as u8]).unwrap();
        }
        expected
    })
    .await
    .unwrap();
    let branch = local.branch(session.branch).await.unwrap().unwrap();
    let committed = success(
        engine
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
    let revision = match committed.result.unwrap() {
        CommandResult::Revision { revision, .. } => revision,
        _ => panic!("wrong result"),
    };
    let history_path = mounts.join("history");
    let history = success(
        engine
            .services
            .mounts
            .prepare(
                context(),
                PrepareMount {
                    workspace: workspace.id,
                    branch: session.branch,
                    revision: Some(revision),
                    path: history_path.clone(),
                    access: AccessMode::ReadOnly,
                    durability: Durability::Local,
                },
            )
            .await
            .unwrap(),
    );
    assert_eq!(history.mount_ready, Some(true));
    tokio::task::spawn_blocking(move || {
        assert_eq!(
            std::fs::read(history_path.join("directory/Renamed")).unwrap(),
            expected
        );
        assert_eq!(std::fs::read_dir(&history_path).unwrap().count(), 131);
        assert!(std::fs::write(history_path.join("forbidden"), b"readonly").is_err());
    })
    .await
    .unwrap();
    for mount in local.mounts().await.unwrap() {
        let record = success(
            engine
                .services
                .mounts
                .unmount(context(), mount.id, mount.generation)
                .await
                .unwrap(),
        );
        assert_eq!(record.phase, OperationPhase::Complete, "{record:?}");
    }
}
